//! Direct Hugging Face Xet CAS range reads.
//!
//! Hub metadata and the read token are obtained from the Hub API. File bytes
//! are reconstructed here from signed CAS Xorb ranges, without `hf-xet`.

use anyhow::{Context, bail};
use hf_hub::{HFClientSync, HFRepositorySync, RepoTypeAny, repository::RepoTreeEntry, split_id};
use reqwest::{
    StatusCode, Url,
    blocking::{Client, Response},
    header,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env,
    io::{Cursor, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_RANGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CAS_RESPONSE_BYTES: u64 = MAX_RANGE_BYTES + 8 * 1024 * 1024;
const MAX_RECONSTRUCTION_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_XORB_RANGES: usize = 4096;
const MAX_XORB_FETCHES: usize = 4096;
pub const DEFAULT_CAS_WORKERS: usize = 1;
const TOKEN_REFRESH_SKEW: u64 = 30;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CasToken {
    access_token: String,
    exp: u64,
    cas_url: String,
}

#[derive(Debug, Deserialize)]
struct CasReconstruction {
    offset_into_first_range: u64,
    terms: Vec<CasTerm>,
    xorbs: HashMap<String, Vec<XorbFetch>>,
}

#[derive(Debug, Deserialize)]
struct LegacyReconstruction {
    offset_into_first_range: u64,
    terms: Vec<CasTerm>,
    fetch_info: HashMap<String, Vec<LegacyXorbFetch>>,
}

#[derive(Debug, Deserialize)]
struct CasTerm {
    hash: String,
    unpacked_length: u64,
    range: ChunkRange,
}

#[derive(Clone, Debug, Deserialize)]
struct XorbFetch {
    url: String,
    ranges: Vec<XorbRange>,
}

#[derive(Debug, Deserialize)]
struct LegacyXorbFetch {
    range: ChunkRange,
    url: String,
    url_range: ByteRange,
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct ChunkRange {
    start: u64,
    end: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct XorbRange {
    chunks: ChunkRange,
    bytes: ByteRange,
}

fn repository_path(path: &str) -> anyhow::Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("file path must be a safe repository-relative path");
    }
    Ok(())
}

fn is_truthy_environment_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_uppercase().as_str(),
        "1" | "ON" | "YES" | "TRUE"
    )
}

pub(crate) fn hub_token() -> Option<String> {
    if env::var("HF_HUB_DISABLE_IMPLICIT_TOKEN")
        .is_ok_and(|value| is_truthy_environment_value(&value))
    {
        return None;
    }
    if let Ok(token) = env::var("HF_TOKEN")
        && !token.is_empty()
    {
        return Some(token);
    }
    if let Ok(path) = env::var("HF_TOKEN_PATH")
        && let Ok(token) = std::fs::read_to_string(path)
    {
        let token = token.trim().to_owned();
        if !token.is_empty() {
            return Some(token);
        }
    }

    let home = if let Ok(home) = env::var("HF_HOME") {
        PathBuf::from(home)
    } else if let Ok(cache) = env::var("XDG_CACHE_HOME") {
        PathBuf::from(cache).join("huggingface")
    } else {
        PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
            .join(".cache")
            .join("huggingface")
    };
    std::fs::read_to_string(home.join("token"))
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

fn append_segments(url: &mut Url, segments: &[&str]) -> anyhow::Result<()> {
    let mut path = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("service endpoint cannot accept URL path segments"))?;
    path.pop_if_empty();
    path.extend(segments.iter().copied());
    Ok(())
}

fn hub_api_url(
    endpoint: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
) -> anyhow::Result<Url> {
    hub_repo_api_url(endpoint, repo_id, repo_type, &["xet-read-token", revision])
}

pub(crate) fn hub_repo_api_url(
    endpoint: &str,
    repo_id: &str,
    repo_type: &str,
    route: &[&str],
) -> anyhow::Result<Url> {
    let plural = match repo_type {
        "model" => "models",
        "dataset" => "datasets",
        "space" => "spaces",
        "kernel" => "kernels",
        _ => bail!("repo type must be model, dataset, space, or kernel"),
    };
    let (owner, name) = split_id(repo_id);
    if name.is_empty() {
        bail!("invalid Hugging Face repository id: {repo_id}");
    }
    let mut url = Url::parse(endpoint.trim_end_matches('/'))
        .context("HF_ENDPOINT must be a valid absolute URL")?;
    anyhow::ensure!(
        url.scheme() == "https" || url.host_str() == Some("localhost"),
        "HF_ENDPOINT must use HTTPS"
    );
    let mut segments = vec!["api", plural];
    if !owner.is_empty() {
        segments.push(owner);
    }
    segments.extend(name.split('/'));
    segments.extend(route.iter().copied());
    append_segments(&mut url, &segments)?;
    Ok(url)
}

fn resolve_cas_token(
    client: &Client,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
) -> anyhow::Result<CasToken> {
    let endpoint = env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".into());
    let url = hub_api_url(&endpoint, repo_id, repo_type, revision)?;
    let mut request = client.get(url).header(header::ACCEPT_ENCODING, "identity");
    if let Some(token) = hub_token() {
        let auth = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("HF token contains invalid header characters")?;
        request = request.header(header::AUTHORIZATION, auth);
    }
    let response = request.send().map_err(|error| {
        anyhow::anyhow!("Hub Xet token request failed: {}", error.without_url())
    })?;
    if !response.status().is_success() {
        bail!("Hub Xet token request returned HTTP {}", response.status());
    }
    let body = response_bytes_limited(response, MAX_TOKEN_RESPONSE_BYTES, "Hub Xet token")?;
    let token: CasToken = serde_json::from_slice(&body).context("decode Hub Xet token response")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    anyhow::ensure!(
        token.exp > now.saturating_add(TOKEN_REFRESH_SKEW),
        "Hub returned an Xet token that is expired or near expiry"
    );
    let cas_url = Url::parse(&token.cas_url).context("Hub returned an invalid CAS endpoint")?;
    anyhow::ensure!(
        cas_url.scheme() == "https" || cas_url.host_str() == Some("localhost"),
        "Hub CAS endpoint must use HTTPS"
    );
    anyhow::ensure!(
        cas_url.username().is_empty() && cas_url.password().is_none(),
        "Hub CAS endpoint must not contain user information"
    );
    Ok(token)
}

fn cas_url(base: &str, version: &str, file_id: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(base).context("invalid Xet CAS endpoint")?;
    append_segments(&mut url, &[version, "reconstructions", file_id])?;
    Ok(url)
}

fn cas_auth(token: &str) -> anyhow::Result<header::HeaderValue> {
    header::HeaderValue::from_str(&format!("Bearer {token}"))
        .context("CAS token contains invalid header characters")
}

fn response_bytes_limited(
    response: Response,
    limit: u64,
    description: &str,
) -> anyhow::Result<Vec<u8>> {
    if let Some(content_length) = response.content_length() {
        anyhow::ensure!(
            content_length <= limit,
            "{description} response exceeds the client size limit"
        );
    }
    let mut body = Vec::new();
    response
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .with_context(|| format!("read {description} response"))?;
    anyhow::ensure!(
        body.len() as u64 <= limit,
        "{description} response exceeds the client size limit"
    );
    Ok(body)
}

fn get_reconstruction(
    client: &Client,
    token: &CasToken,
    file_id: &str,
    byte_range: ByteRange,
) -> anyhow::Result<CasReconstruction> {
    let range_header = format!("bytes={}-{}", byte_range.start, byte_range.end);
    let auth = cas_auth(&token.access_token)?;
    let response = client
        .get(cas_url(&token.cas_url, "v2", file_id)?)
        .header(header::AUTHORIZATION, auth.clone())
        .header(header::RANGE, &range_header)
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .map_err(|error| {
            anyhow::anyhow!("CAS reconstruction request failed: {}", error.without_url())
        })?;

    let response = if response.status() == StatusCode::NOT_FOUND
        || response.status() == StatusCode::NOT_IMPLEMENTED
    {
        client
            .get(cas_url(&token.cas_url, "v1", file_id)?)
            .header(header::AUTHORIZATION, auth)
            .header(header::RANGE, range_header)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .map_err(|error| {
                anyhow::anyhow!(
                    "legacy CAS reconstruction request failed: {}",
                    error.without_url()
                )
            })?
    } else {
        response
    };
    if !response.status().is_success() {
        bail!(
            "CAS reconstruction request returned HTTP {}",
            response.status()
        );
    }
    let body = response_bytes_limited(response, MAX_RECONSTRUCTION_BYTES, "CAS reconstruction")?;
    let body: serde_json::Value =
        serde_json::from_slice(&body).context("decode CAS reconstruction response")?;
    if body.get("xorbs").is_some() {
        serde_json::from_value(body).context("decode Xet v2 reconstruction")
    } else {
        let legacy: LegacyReconstruction =
            serde_json::from_value(body).context("decode Xet v1 reconstruction")?;
        let xorbs = legacy
            .fetch_info
            .into_iter()
            .map(|(hash, fetches)| {
                let fetches = fetches
                    .into_iter()
                    .map(|fetch| XorbFetch {
                        url: fetch.url,
                        ranges: vec![XorbRange {
                            chunks: fetch.range,
                            bytes: fetch.url_range,
                        }],
                    })
                    .collect();
                (hash, fetches)
            })
            .collect();
        Ok(CasReconstruction {
            offset_into_first_range: legacy.offset_into_first_range,
            terms: legacy.terms,
            xorbs,
        })
    }
}

fn parse_content_range(value: &str) -> anyhow::Result<ByteRange> {
    let value = value.trim();
    anyhow::ensure!(
        value
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bytes ")),
        "invalid multipart Content-Range"
    );
    let value = &value[6..];
    let (range, _) = value
        .split_once('/')
        .context("invalid multipart Content-Range")?;
    let (start, end) = range
        .split_once('-')
        .context("invalid multipart Content-Range")?;
    let start = start.parse().context("invalid multipart range start")?;
    let end = end.parse().context("invalid multipart range end")?;
    anyhow::ensure!(start <= end, "multipart byte range is inverted");
    Ok(ByteRange { start, end })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn multipart_boundary(content_type: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        content_type
            .to_ascii_lowercase()
            .starts_with("multipart/byteranges"),
        "CAS returned a non-multipart response for multiple Xorb ranges"
    );
    content_type
        .split(';')
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("boundary")
                .then_some(value.trim())
        })
        .map(|value| value.trim_matches('"'))
        .filter(|value| !value.is_empty())
        .context("CAS multipart response has no boundary")
}

fn parse_multipart_ranges(
    content_type: &str,
    body: &[u8],
    expected: &[ByteRange],
) -> anyhow::Result<Vec<Vec<u8>>> {
    let boundary = multipart_boundary(content_type)?;
    let marker = format!("--{boundary}");
    let separator = b"\r\n\r\n";
    let mut position = find_bytes(body, marker.as_bytes())
        .context("CAS multipart response contains no boundary")?
        + marker.len();
    let mut parts = Vec::with_capacity(expected.len());

    for (index, expected_range) in expected.iter().enumerate() {
        anyhow::ensure!(
            body.get(position..position + 2) == Some(b"\r\n"),
            "malformed CAS multipart boundary"
        );
        position += 2;
        let headers_end = find_bytes(&body[position..], separator)
            .context("CAS multipart part has no header separator")?
            + position;
        let headers = std::str::from_utf8(&body[position..headers_end])
            .context("CAS multipart headers are not UTF-8")?;
        let content_range = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-range"))
                    .map(|(_, value)| value.trim())
            })
            .context("CAS multipart part has no Content-Range")?;
        let actual_range = parse_content_range(content_range)?;
        anyhow::ensure!(
            actual_range == *expected_range,
            "CAS multipart ranges arrived out of order or mismatched"
        );
        let data_start = headers_end + separator.len();
        let data_len = usize::try_from(actual_range.end - actual_range.start + 1)
            .context("CAS byte range exceeds addressable memory")?;
        let data_end = data_start
            .checked_add(data_len)
            .context("CAS multipart data length overflow")?;
        let data = body
            .get(data_start..data_end)
            .context("CAS multipart part is truncated")?
            .to_vec();
        position = data_end;
        let next_marker = format!("\r\n{marker}");
        anyhow::ensure!(
            body.get(position..position + next_marker.len()) == Some(next_marker.as_bytes()),
            "CAS multipart part is missing its closing boundary"
        );
        position += next_marker.len();
        parts.push(data);
        if index + 1 == expected.len() {
            anyhow::ensure!(
                body.get(position..position + 2) == Some(b"--"),
                "CAS multipart response has extra parts"
            );
        }
    }
    Ok(parts)
}

fn fetch_xorb_bytes(client: &Client, fetch: &XorbFetch) -> anyhow::Result<Vec<Vec<u8>>> {
    anyhow::ensure!(
        !fetch.ranges.is_empty(),
        "CAS returned an empty Xorb range list"
    );
    anyhow::ensure!(
        fetch.ranges.len() <= MAX_XORB_RANGES,
        "CAS returned too many Xorb ranges in one request"
    );
    let mut total_bytes = 0u64;
    let mut ranges = Vec::with_capacity(fetch.ranges.len());
    for range in &fetch.ranges {
        anyhow::ensure!(
            range.chunks.start < range.chunks.end,
            "CAS returned an empty chunk range"
        );
        anyhow::ensure!(
            range.bytes.start <= range.bytes.end,
            "CAS returned an inverted byte range"
        );
        let range_bytes = range
            .bytes
            .end
            .checked_sub(range.bytes.start)
            .and_then(|size| size.checked_add(1))
            .context("CAS returned an invalid byte range")?;
        total_bytes = total_bytes
            .checked_add(range_bytes)
            .context("CAS response range length overflow")?;
        ranges.push(range.bytes);
    }
    anyhow::ensure!(
        total_bytes <= MAX_RANGE_BYTES,
        "CAS signed range batch exceeds the 64 MiB client limit"
    );
    let range_header = format!(
        "bytes={}",
        ranges
            .iter()
            .map(|range| format!("{}-{}", range.start, range.end))
            .collect::<Vec<_>>()
            .join(",")
    );
    let url = Url::parse(&fetch.url).context("CAS returned an invalid signed Xorb URL")?;
    anyhow::ensure!(
        url.scheme() == "https" || url.host_str() == Some("localhost"),
        "signed Xorb URL must use HTTPS"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "signed Xorb URL must not contain user information"
    );
    let response = client
        .get(url)
        .header(header::RANGE, range_header)
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .map_err(|error| {
            anyhow::anyhow!("signed Xorb range request failed: {}", error.without_url())
        })?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        bail!(
            "signed Xorb range request returned HTTP {}",
            response.status()
        );
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let is_multipart = content_type
        .to_ascii_lowercase()
        .starts_with("multipart/byteranges");
    let single_content_range = if is_multipart {
        None
    } else {
        Some(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .map(parse_content_range)
                .transpose()?
                .context("CAS single-range response has no Content-Range")?,
        )
    };
    let body = response_bytes_limited(response, MAX_CAS_RESPONSE_BYTES, "signed Xorb range")?;
    if is_multipart {
        return parse_multipart_ranges(&content_type, &body, &ranges);
    }
    anyhow::ensure!(
        ranges.len() == 1,
        "CAS omitted multipart framing for multiple byte ranges"
    );
    let expected_size = ranges[0]
        .end
        .checked_sub(ranges[0].start)
        .and_then(|size| size.checked_add(1))
        .context("invalid CAS range length")?;
    anyhow::ensure!(
        body.len() as u64 == expected_size,
        "CAS Xorb response has the wrong byte length"
    );
    anyhow::ensure!(
        single_content_range == Some(ranges[0]),
        "CAS returned the wrong byte range"
    );
    Ok(vec![body.to_vec()])
}

fn decode_fetch(
    client: &Client,
    xorb: &str,
    fetch: &XorbFetch,
) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
    let bodies = fetch_xorb_bytes(client, fetch)?;
    anyhow::ensure!(
        bodies.len() == fetch.ranges.len(),
        "CAS multipart range count mismatch"
    );
    let mut chunks = Vec::new();
    for (descriptor, body) in fetch.ranges.iter().zip(bodies) {
        let mut input = Cursor::new(body);
        for index in descriptor.chunks.start..descriptor.chunks.end {
            let (chunk, _) = crate::xorb::decode(&mut input)?
                .with_context(|| format!("Xorb {xorb} ended before chunk {index}"))?;
            chunks.push((index, chunk));
        }
        anyhow::ensure!(
            crate::xorb::decode(&mut input)?.is_none(),
            "CAS byte range contains extra Xorb chunks"
        );
    }
    Ok(chunks)
}

fn fetch_chunks(
    client: &Client,
    reconstruction: &CasReconstruction,
    workers: usize,
    requested_bytes: u64,
) -> anyhow::Result<HashMap<(String, u64), Vec<u8>>> {
    let mut tasks = Vec::new();
    let max_reconstructed_bytes = requested_bytes
        .checked_add(2 * crate::chunker::MAX_CHUNK as u64)
        .context("requested range length overflow")?;
    let reconstructed_bytes = reconstruction.terms.iter().try_fold(0u64, |total, term| {
        total
            .checked_add(term.unpacked_length)
            .context("CAS term length overflow")
    })?;
    anyhow::ensure!(
        reconstructed_bytes <= max_reconstructed_bytes,
        "CAS reconstruction exceeds the requested range and chunk boundary allowance"
    );
    for (xorb, fetches) in &reconstruction.xorbs {
        let parsed_hash =
            crate::hash::from_hex(xorb).context("CAS returned an invalid Xorb hash")?;
        anyhow::ensure!(
            crate::hash::to_hex(&parsed_hash) == *xorb,
            "CAS Xorb hashes must be lowercase hex"
        );
        tasks.extend(fetches.iter().cloned().map(|fetch| (xorb.clone(), fetch)));
    }
    anyhow::ensure!(
        tasks.len() <= MAX_XORB_FETCHES,
        "CAS returned too many signed Xorb fetches"
    );
    let max_wire_bytes = max_reconstructed_bytes
        .checked_add(MAX_CAS_RESPONSE_BYTES - MAX_RANGE_BYTES)
        .context("CAS wire range limit overflow")?;
    let wire_bytes = tasks.iter().try_fold(0u64, |total, (_, fetch)| {
        fetch.ranges.iter().try_fold(total, |total, range| {
            let length = range
                .bytes
                .end
                .checked_sub(range.bytes.start)
                .and_then(|size| size.checked_add(1))
                .context("CAS returned an inverted byte range")?;
            total
                .checked_add(length)
                .context("CAS signed range length overflow")
        })
    })?;
    anyhow::ensure!(
        wire_bytes <= max_wire_bytes,
        "CAS signed ranges exceed the request size limit"
    );
    let worker_count = workers.min(tasks.len());
    let cursor = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::sync_channel(worker_count);
    let mut chunks = HashMap::new();
    thread::scope(|scope| -> anyhow::Result<()> {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let client = client.clone();
            let tasks = &tasks;
            let cursor = &cursor;
            scope.spawn(move || {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((xorb, fetch)) = tasks.get(index) else {
                        break;
                    };
                    if sender
                        .send((xorb.clone(), decode_fetch(&client, xorb, fetch)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for (xorb, decoded) in receiver {
            for (index, chunk) in decoded.with_context(|| format!("fetch Xorb {xorb}"))? {
                match chunks.entry((xorb.clone(), index)) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(chunk);
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        anyhow::ensure!(*entry.get() == chunk, "duplicate CAS chunk data differed");
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(chunks)
}

fn decode_range(
    file_id: &str,
    file_size: u64,
    request_range: ByteRange,
    reconstruction: CasReconstruction,
    chunks: HashMap<(String, u64), Vec<u8>>,
) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::with_capacity((request_range.end - request_range.start + 1) as usize);
    let mut skip = reconstruction.offset_into_first_range;
    let mut full_chunks = Vec::new();
    let full_read = request_range.start == 0 && request_range.end == file_size - 1;

    for term in &reconstruction.terms {
        let xorb_hash =
            crate::hash::from_hex(&term.hash).context("CAS term has an invalid Xorb hash")?;
        anyhow::ensure!(
            crate::hash::to_hex(&xorb_hash) == term.hash,
            "CAS term hashes must be lowercase hex"
        );
        anyhow::ensure!(
            term.range.start < term.range.end,
            "CAS term contains an empty chunk range"
        );
        let mut term_bytes = 0u64;
        let mut term_chunks = Vec::new();
        for index in term.range.start..term.range.end {
            let chunk = chunks.get(&(term.hash.clone(), index)).with_context(|| {
                format!("CAS response omitted Xorb {} chunk {index}", term.hash)
            })?;
            term_bytes = term_bytes
                .checked_add(chunk.len() as u64)
                .context("CAS term length overflow")?;
            term_chunks.push(chunk.as_slice());
        }
        anyhow::ensure!(
            term_bytes == term.unpacked_length,
            "CAS term unpacked length mismatch"
        );
        if skip >= term_bytes {
            skip -= term_bytes;
            if full_read {
                for chunk in term_chunks {
                    full_chunks.push((crate::hash::chunk_hash(chunk), chunk.len() as u64));
                }
            }
            continue;
        }
        for chunk in term_chunks {
            if full_read {
                full_chunks.push((crate::hash::chunk_hash(chunk), chunk.len() as u64));
            }
            let start = skip.min(chunk.len() as u64) as usize;
            skip -= start as u64;
            if start == chunk.len() {
                continue;
            }
            let remaining = (request_range.end - request_range.start + 1) as usize - output.len();
            let take = remaining.min(chunk.len() - start);
            output.extend_from_slice(&chunk[start..start + take]);
            if output.len() as u64 == request_range.end - request_range.start + 1 {
                break;
            }
        }
        if output.len() as u64 == request_range.end - request_range.start + 1 {
            break;
        }
    }
    let expected = request_range.end - request_range.start + 1;
    anyhow::ensure!(
        output.len() as u64 == expected,
        "CAS reconstruction returned fewer bytes than requested"
    );
    if full_read {
        let actual = crate::hash::to_hex(&crate::hash::file_hash(&full_chunks));
        anyhow::ensure!(actual == file_id, "CAS reconstructed file hash mismatch");
    }
    Ok(output)
}

/// Read a byte range directly from Hugging Face Xet CAS.
///
/// The range is end-exclusive; a maximum of 64 MiB is returned per call.
pub fn read_range(
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    path: &str,
    offset: u64,
    length: u64,
) -> anyhow::Result<Vec<u8>> {
    read_range_with_workers(
        repo_id,
        repo_type,
        revision,
        path,
        offset,
        length,
        DEFAULT_CAS_WORKERS,
    )
}

/// Read a byte range directly from Xet CAS using a bounded number of workers.
pub fn read_range_with_workers(
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    path: &str,
    offset: u64,
    length: u64,
    workers: usize,
) -> anyhow::Result<Vec<u8>> {
    repository_path(path)?;
    anyhow::ensure!(
        (1..=64).contains(&workers),
        "CAS worker count must be between 1 and 64"
    );
    anyhow::ensure!(
        length <= MAX_RANGE_BYTES,
        "remote CAS reads are limited to 64 MiB per request"
    );
    if length == 0 {
        return Ok(Vec::new());
    }
    let kind: RepoTypeAny = repo_type
        .parse()
        .context("invalid Hugging Face repo type")?;
    let (owner, name) = split_id(repo_id);
    if name.is_empty() {
        bail!("invalid Hugging Face repository id: {repo_id}");
    }
    let hub = HFClientSync::new().context("initialize Hugging Face client and token")?;
    let repo: HFRepositorySync<RepoTypeAny> = hub.repository(kind, owner, name);
    let entries = repo
        .get_paths_info()
        .paths(vec![path.to_owned()])
        .revision(revision.to_owned())
        .send()
        .context("read Hub file metadata")?;
    let (file_size, xet_hash) = entries
        .into_iter()
        .find_map(|entry| match entry {
            RepoTreeEntry::File {
                path: listed,
                size,
                xet_hash,
                ..
            } if listed == path => Some((size, xet_hash)),
            _ => None,
        })
        .context("Hub returned no matching file metadata")?;
    let Some(file_id) = xet_hash else {
        bail!("Hub file is not stored in Xet CAS; direct range reads require an Xet file id");
    };
    let parsed_id =
        crate::hash::from_hex(&file_id).context("Hub returned an invalid Xet file id")?;
    anyhow::ensure!(
        crate::hash::to_hex(&parsed_id) == file_id,
        "Hub file id must be lowercase hex"
    );
    if offset >= file_size {
        return Ok(Vec::new());
    }
    let actual_end = offset.saturating_add(length).min(file_size);
    let request_range = ByteRange {
        start: offset,
        end: actual_end - 1,
    };

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()
        .context("initialize direct Xet CAS HTTP client")?;
    let token = resolve_cas_token(&client, repo_id, repo_type, revision)?;
    let reconstruction = get_reconstruction(&client, &token, &file_id, request_range)?;
    let chunks = fetch_chunks(
        &client,
        &reconstruction,
        workers,
        request_range.end - request_range.start + 1,
    )?;
    decode_range(&file_id, file_size, request_range, reconstruction, chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hugging_face_boolean_environment_values() {
        for enabled in ["1", "on", "YES", " true "] {
            assert!(is_truthy_environment_value(enabled));
        }
        for disabled in ["", "0", "false", "off"] {
            assert!(!is_truthy_environment_value(disabled));
        }
    }

    #[test]
    fn parses_multipart_ranges_without_splitting_binary_payload() {
        let boundary = "xet-test-boundary";
        let binary = b"a\r\n--xet-test-boundaryinside\0z";
        let body = [
            format!("--{boundary}\r\nContent-Type: application/octet-stream\r\nContent-Range: bytes 10-{}/100\r\n\r\n", 9 + binary.len()),
            String::from_utf8_lossy(binary).to_string(),
            format!("\r\n--{boundary}\r\nContent-Range: bytes 30-31/100\r\n\r\nxy\r\n--{boundary}--\r\n"),
        ]
        .concat()
        .into_bytes();
        let expected = [
            ByteRange {
                start: 10,
                end: 9 + binary.len() as u64,
            },
            ByteRange { start: 30, end: 31 },
        ];
        let parts = parse_multipart_ranges(
            &format!("multipart/byteranges; boundary={boundary}"),
            &body,
            &expected,
        )
        .unwrap();
        assert_eq!(parts, vec![binary.to_vec(), b"xy".to_vec()]);
    }
}
