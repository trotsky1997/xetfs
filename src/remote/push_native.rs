//! XetFS-owned Xet CAS upload path for committed local content.
//!
//! Local immutable Xorbs are validated and uploaded as-is. XetFS builds the
//! Hub shard and Git LFS commit records itself; `hf-xet` is not in this path.

use super::{
    LocalFile, PushReport, collect_local_files, open_repo, validate_prefix, validate_repo_path,
};
use crate::{FileSystem, hash};
use crate::{remote_cas, repository::UploadXorb};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::{StatusCode, Url, blocking::Client, header};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    env,
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PREUPLOAD_SAMPLE_SIZE: usize = 512;
const MAX_INLINE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SHARD_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const TOKEN_REFRESH_SKEW: u64 = 30;
const CAS_UPLOAD_MAX_ATTEMPTS: usize = 4;
const CAS_ERROR_BODY_LIMIT: u64 = 4096;
const MAX_PARALLEL_XORB_UPLOAD_BYTES: usize = 128 * 1024 * 1024;
const FILE_FLAG_WITH_VERIFICATION: u32 = 1 << 31;
const FILE_FLAG_WITH_METADATA_EXT: u32 = 1 << 30;
const SHARD_HEADER_TAG: [u8; 32] = [
    b'H', b'F', b'R', b'e', b'p', b'o', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a', 0, 85, 105,
    103, 69, 106, 123, 129, 87, 131, 165, 189, 217, 92, 205, 209, 74, 169,
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreuploadResponse {
    files: Vec<PreuploadFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreuploadFile {
    path: String,
    upload_mode: String,
}

#[derive(Deserialize)]
struct LfsBatchResponse {
    transfer: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CasToken {
    access_token: String,
    exp: u64,
    cas_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitInfo {
    commit_oid: Option<String>,
    commit_url: Option<String>,
}

struct Source {
    local: LocalFile,
    snapshot: Arc<crate::repository::UploadSnapshot>,
    sample: Vec<u8>,
    reconstruction: crate::repository::Reconstruction,
}

struct FileManifest {
    file_hash: hash::Hash,
    sha256: [u8; 32],
    size: u64,
    terms: Vec<FileTerm>,
}

struct FileTerm {
    xorb_hash: hash::Hash,
    unpacked_bytes: u32,
    chunk_start: u32,
    chunk_end: u32,
    verification_hash: hash::Hash,
}

#[derive(Clone)]
struct CasXorbInfo {
    xorb_hash: hash::Hash,
    raw_bytes: u32,
    disk_bytes: u32,
    chunks: Vec<CasChunkInfo>,
}

#[derive(Clone)]
struct CasChunkInfo {
    chunk_hash: hash::Hash,
    offset: u32,
    len: u32,
}

struct PendingXorb {
    id: String,
    xorb: UploadXorb,
}

#[derive(Default)]
struct XorbUploadStats {
    uploaded: usize,
    skipped: usize,
    observed_disk_bytes: HashMap<String, u32>,
}

impl XorbUploadStats {
    fn merge(&mut self, other: Self) {
        self.uploaded += other.uploaded;
        self.skipped += other.skipped;
        self.observed_disk_bytes.extend(other.observed_disk_bytes);
    }
}

pub(super) fn push(
    fs: &FileSystem,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    prefix: Option<&str>,
    commit_message: &str,
    workers: usize,
) -> anyhow::Result<PushReport> {
    validate_prefix(prefix)?;
    anyhow::ensure!(
        (1..=64).contains(&workers),
        "native CAS upload workers must be between 1 and 64"
    );
    // Preserve the upstream streaming path for unflushed handles. The direct
    // CAS path below consumes immutable repository Xorbs without rechunking.
    let _repo = open_repo(repo_id, repo_type)?;
    let source_path = prefix.unwrap_or("/");
    let source = fs
        .resolve_nofollow(source_path)
        .with_context(|| format!("resolve local XetFS path {source_path}"))?;
    let mut local_files = Vec::new();
    let mut skipped_symlinks = 0u64;
    let path = if source_path == "/" {
        "/".to_owned()
    } else if source_path.starts_with('/') {
        source_path.to_owned()
    } else {
        format!("/{source_path}")
    };
    collect_local_files(fs, source, path, &mut local_files, &mut skipped_symlinks)?;
    if local_files.is_empty() {
        bail!("there are no regular files to upload");
    }
    for file in &local_files {
        if fs.committed_upload_content(file.inode)?.is_none() {
            return super::push_official(fs, repo_id, repo_type, revision, prefix, commit_message);
        }
    }

    let token = remote_cas::hub_token()
        .context("no Hugging Face token found; log in or set HF_TOKEN with write access")?;
    let endpoint = env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".into());
    let client = Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("initialize XetFS Hub/CAS upload client")?;

    let sources = prepare_sources(fs, local_files)?;
    let modes = preupload_modes(
        &client, &endpoint, &token, repo_id, repo_type, revision, &sources,
    )?;
    if sources.iter().any(|source| {
        modes.get(&source.local.repo_path).map(String::as_str) != Some("lfs")
            && source.local.size > MAX_INLINE_BYTES
    }) {
        return super::push_official(fs, repo_id, repo_type, revision, prefix, commit_message);
    }

    let mut manifests = Vec::with_capacity(sources.len());
    let mut xorb_ids = BTreeSet::new();
    for source in &sources {
        let mode = modes
            .get(&source.local.repo_path)
            .map(String::as_str)
            .unwrap_or("regular");
        if mode == "lfs" && source.local.size > 0 {
            let manifest = build_manifest(source)?;
            xorb_ids.extend(
                manifest
                    .terms
                    .iter()
                    .map(|term| hash::to_hex(&term.xorb_hash)),
            );
            manifests.push(Some(manifest));
        } else {
            manifests.push(None);
        }
    }

    // Hub's LFS batch negotiation is part of the Xet write protocol, not an
    // optional optimization. In particular, it registers the SHA-256/size
    // pair before the LFS pointer is committed. The official Hub client does
    // this before uploading any Xet CAS objects.
    let lfs_objects = sources
        .iter()
        .zip(&manifests)
        .filter_map(|(source, manifest)| {
            manifest
                .as_ref()
                .map(|manifest| (hex(&manifest.sha256), source.local.size))
        })
        .collect::<Vec<_>>();
    if !lfs_objects.is_empty()
        && lfs_batch_transfer(
            &client,
            &endpoint,
            &token,
            repo_id,
            repo_type,
            revision,
            &lfs_objects,
        )? != Some("xet".to_owned())
    {
        return super::push_official(fs, repo_id, repo_type, revision, prefix, commit_message);
    }

    // Validate every local Xorb and file recipe before the first remote write.
    let mut xorb_metadata = load_xorb_metadata(fs, &xorb_ids)?;
    for manifest in manifests.iter().flatten() {
        let cas_infos = manifest_xorb_infos(manifest, &xorb_metadata)?;
        validate_manifest_xorbs(manifest, &cas_infos)?;
    }

    // CAS objects are content-addressed and independent across files. Upload
    // them in bounded parallel batches before writing the per-file shards.
    let mut xet_token: Option<CasToken> = None;
    let xorb_upload_started = Instant::now();
    let xorb_upload_stats = upload_xorbs(
        fs,
        &client,
        &endpoint,
        &token,
        repo_id,
        repo_type,
        revision,
        &xorb_ids,
        &mut xorb_metadata,
        &mut xet_token,
        workers,
    )?;
    let xorb_upload_seconds = xorb_upload_started.elapsed().as_secs_f64();

    let mut operations = Vec::with_capacity(sources.len());
    let mut bytes_uploaded = 0u64;
    for (source, manifest) in sources.iter().zip(manifests) {
        let relative = source.local.repo_path.trim_start_matches('/');
        validate_repo_path(relative)?;
        if let Some(manifest) = manifest {
            let cas_infos = manifest_xorb_infos(&manifest, &xorb_metadata)?;
            let shard = serialize_upload_shard(&manifest, &cas_infos)?;
            let token = ensure_write_token(
                &client,
                &endpoint,
                &token,
                repo_id,
                repo_type,
                revision,
                &mut xet_token,
            )?;
            upload_shard(&client, token, &shard)?;
            operations.push(json!({
                "key": "lfsFile",
                "value": {
                    "path": relative,
                    "algo": "sha256",
                    "oid": hex(&manifest.sha256),
                    "size": manifest.size,
                }
            }));
        } else {
            let content = snapshot_bytes(&source.snapshot, source.local.size)?;
            operations.push(json!({
                "key": "file",
                "value": {
                    "content": BASE64.encode(content),
                    "path": relative,
                    "encoding": "base64",
                }
            }));
        }
        bytes_uploaded = bytes_uploaded.saturating_add(source.local.size);
    }

    let body = commit_body(commit_message, operations)?;
    let commit_url =
        remote_cas::hub_repo_api_url(&endpoint, repo_id, repo_type, &["commit", revision])?;
    let response = client
        .post(commit_url)
        .bearer_auth(token)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(body)
        .send()
        .map_err(|error| anyhow::anyhow!("Hub commit request failed: {}", error.without_url()))?;
    if !response.status().is_success() {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("x-amzn-trace-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response_bytes(response, CAS_ERROR_BODY_LIMIT, "Hub commit error body")
            .unwrap_or_default();
        let excerpt = response_excerpt(&body);
        bail!(
            "Hub commit request returned HTTP {status}{}{}",
            request_id
                .map(|value| format!(" (request id {value})"))
                .unwrap_or_default(),
            if excerpt.is_empty() {
                String::new()
            } else {
                format!(": {excerpt}")
            }
        );
    }
    let commit: CommitInfo = response.json().context("decode Hub commit response")?;
    if commit.commit_oid.as_deref().is_none_or(str::is_empty) {
        bail!("Hub commit response did not contain a commit OID");
    }

    Ok(PushReport {
        repository: repo_id.to_owned(),
        revision: revision.to_owned(),
        files_uploaded: sources.len() as u64,
        symlinks_skipped: skipped_symlinks,
        bytes_uploaded,
        native_cas_upload_workers: Some(workers),
        native_cas_xorbs_uploaded: Some(xorb_upload_stats.uploaded as u64),
        native_cas_xorbs_skipped: Some(xorb_upload_stats.skipped as u64),
        native_cas_xorb_upload_seconds: Some(xorb_upload_seconds),
        commit_oid: commit.commit_oid,
        commit_url: commit.commit_url,
    })
}

fn prepare_sources(fs: &FileSystem, files: Vec<LocalFile>) -> anyhow::Result<Vec<Source>> {
    let mut sources = Vec::with_capacity(files.len());
    for local in files {
        let content_id = fs
            .committed_upload_content(local.inode)?
            .context("file acquired a write stage while preparing remote upload")?;
        let snapshot = Arc::new(fs.upload_snapshot(local.inode)?);
        anyhow::ensure!(
            snapshot.size() == local.size,
            "local file {} changed while preparing upload",
            local.repo_path
        );
        let sample = snapshot.read_at(0, local.size.min(PREUPLOAD_SAMPLE_SIZE as u64) as usize)?;
        let reconstruction = fs.repository().reconstruction(&content_id)?;
        anyhow::ensure!(
            reconstruction.size == local.size,
            "local recipe size changed while preparing upload: {}",
            local.repo_path
        );
        sources.push(Source {
            local,
            snapshot,
            sample,
            reconstruction,
        });
    }
    Ok(sources)
}

fn preupload_modes(
    client: &Client,
    endpoint: &str,
    token: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    sources: &[Source],
) -> anyhow::Result<HashMap<String, String>> {
    let files = sources
        .iter()
        .map(|source| {
            json!({
                "path": source.local.repo_path.trim_start_matches('/'),
                "size": source.local.size,
                "sample": BASE64.encode(&source.sample),
            })
        })
        .collect::<Vec<_>>();
    let url = remote_cas::hub_repo_api_url(endpoint, repo_id, repo_type, &["preupload", revision])?;
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&json!({"files": files}))
        .send()
        .map_err(|error| {
            anyhow::anyhow!("Hub preupload request failed: {}", error.without_url())
        })?;
    if !response.status().is_success() {
        bail!("Hub preupload request returned HTTP {}", response.status());
    }
    let result: PreuploadResponse = response.json().context("decode Hub preupload response")?;
    Ok(result
        .files
        .into_iter()
        .map(|file| (format!("/{}", file.path), file.upload_mode))
        .collect())
}

fn lfs_batch_transfer(
    client: &Client,
    endpoint: &str,
    token: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    objects: &[(String, u64)],
) -> anyhow::Result<Option<String>> {
    let url = lfs_batch_url(endpoint, repo_id, repo_type)?;
    let payload = json!({
        "operation": "upload",
        "transfers": ["basic", "multipart", "xet"],
        "objects": objects.iter().map(|(oid, size)| json!({"oid": oid, "size": size})).collect::<Vec<_>>(),
        "hash_algo": "sha256",
        "ref": {"name": revision},
    });
    let response = client
        .post(url)
        .bearer_auth(token)
        .header(header::ACCEPT, "application/vnd.git-lfs+json")
        .header(header::CONTENT_TYPE, "application/vnd.git-lfs+json")
        .json(&payload)
        .send()
        .map_err(|error| {
            anyhow::anyhow!("Hub LFS batch request failed: {}", error.without_url())
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response_bytes(response, CAS_ERROR_BODY_LIMIT, "Hub LFS batch error body")
            .unwrap_or_default();
        let excerpt = response_excerpt(&body);
        bail!(
            "Hub LFS batch request returned HTTP {status}{}",
            if excerpt.is_empty() {
                String::new()
            } else {
                format!(": {excerpt}")
            }
        );
    }
    let result: LfsBatchResponse = response.json().context("decode Hub LFS batch response")?;
    Ok(result.transfer)
}

fn lfs_batch_url(endpoint: &str, repo_id: &str, repo_type: &str) -> anyhow::Result<Url> {
    let prefix = match repo_type {
        "model" => "",
        "dataset" => "datasets/",
        "space" => "spaces/",
        "kernel" => "kernels/",
        _ => bail!("repo type must be model, dataset, space, or kernel"),
    };
    let (owner, name) = hf_hub::split_id(repo_id);
    anyhow::ensure!(
        !name.is_empty(),
        "invalid Hugging Face repository id: {repo_id}"
    );
    let mut url = Url::parse(endpoint.trim_end_matches('/'))
        .context("HF_ENDPOINT must be a valid absolute URL")?;
    anyhow::ensure!(
        url.scheme() == "https" || url.host_str() == Some("localhost"),
        "HF_ENDPOINT must use HTTPS"
    );
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Hub endpoint cannot accept URL path segments"))?;
        segments.pop_if_empty();
        if !prefix.is_empty() {
            segments.push(prefix.trim_end_matches('/'));
        }
        if !owner.is_empty() {
            segments.push(owner);
        }
        let mut name_parts = name.split('/').peekable();
        while let Some(part) = name_parts.next() {
            if name_parts.peek().is_none() {
                segments.push(&format!("{part}.git"));
            } else {
                segments.push(part);
            }
        }
        segments
            .push("info")
            .push("lfs")
            .push("objects")
            .push("batch");
    }
    Ok(url)
}

fn build_manifest(source: &Source) -> anyhow::Result<FileManifest> {
    let mut sha = Sha256::new();
    let mut offset = 0u64;
    while offset < source.local.size {
        let length = (source.local.size - offset).min(1024 * 1024) as usize;
        let bytes = source.snapshot.read_at(offset, length)?;
        anyhow::ensure!(
            bytes.len() == length,
            "local file ended before its recorded size: {}",
            source.local.repo_path
        );
        sha.update(&bytes);
        offset += length as u64;
    }
    let sha256: [u8; 32] = sha.finalize().into();
    let file_hash = hash::from_hex(&source.reconstruction.file_hash)?;
    let terms = source
        .reconstruction
        .terms
        .iter()
        .map(|term| {
            let chunk_start = u32::try_from(term.chunk_start)
                .context("Xet reconstruction start exceeds shard format")?;
            let chunk_end = u32::try_from(term.chunk_end)
                .context("Xet reconstruction end exceeds shard format")?;
            let unpacked_bytes =
                u32::try_from(term.size).context("Xet reconstruction term exceeds shard format")?;
            anyhow::ensure!(chunk_start < chunk_end, "empty Xet reconstruction term");
            Ok(FileTerm {
                xorb_hash: hash::from_hex(&term.xorb)?,
                unpacked_bytes,
                chunk_start,
                chunk_end,
                verification_hash: hash::from_hex(&term.verification_hash)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !terms.is_empty(),
        "non-empty Xet file has no reconstruction terms"
    );
    Ok(FileManifest {
        file_hash,
        sha256,
        size: source.local.size,
        terms,
    })
}

fn load_xorb_metadata(
    fs: &FileSystem,
    ids: &BTreeSet<String>,
) -> anyhow::Result<HashMap<String, CasXorbInfo>> {
    ids.iter()
        .map(|id| {
            let xorb = fs.repository().read_upload_xorb(id)?;
            anyhow::ensure!(hash::to_hex(&xorb.hash) == *id, "local Xorb key mismatch");
            Ok((id.clone(), cas_info(&xorb)?))
        })
        .collect()
}

fn manifest_xorb_infos(
    manifest: &FileManifest,
    metadata: &HashMap<String, CasXorbInfo>,
) -> anyhow::Result<Vec<CasXorbInfo>> {
    manifest
        .terms
        .iter()
        .map(|term| hash::to_hex(&term.xorb_hash))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|id| {
            metadata
                .get(&id)
                .cloned()
                .with_context(|| format!("missing local CAS metadata for Xorb {id}"))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn upload_xorbs(
    fs: &FileSystem,
    client: &Client,
    endpoint: &str,
    hub_token: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    ids: &BTreeSet<String>,
    metadata: &mut HashMap<String, CasXorbInfo>,
    cas_token: &mut Option<CasToken>,
    workers: usize,
) -> anyhow::Result<XorbUploadStats> {
    let mut pending = Vec::new();
    let mut pending_bytes = 0usize;
    let mut totals = XorbUploadStats::default();
    for id in ids {
        let expected_bytes = metadata
            .get(id)
            .with_context(|| format!("missing local CAS metadata for Xorb {id}"))?
            .disk_bytes as usize;
        if !pending.is_empty()
            && (pending.len() >= workers
                || pending_bytes.saturating_add(expected_bytes) > MAX_PARALLEL_XORB_UPLOAD_BYTES)
        {
            totals.merge(upload_xorb_batch(
                std::mem::take(&mut pending),
                client,
                endpoint,
                hub_token,
                repo_id,
                repo_type,
                revision,
                cas_token,
                workers,
            )?);
            pending_bytes = 0;
        }

        let xorb = fs.repository().read_upload_xorb(id)?;
        anyhow::ensure!(hash::to_hex(&xorb.hash) == *id, "local Xorb key mismatch");
        anyhow::ensure!(
            xorb.bytes.len() == expected_bytes,
            "local Xorb size changed after validation for {id}"
        );
        pending_bytes = pending_bytes.saturating_add(expected_bytes);
        pending.push(PendingXorb {
            id: id.clone(),
            xorb,
        });
    }
    totals.merge(upload_xorb_batch(
        pending, client, endpoint, hub_token, repo_id, repo_type, revision, cas_token, workers,
    )?);
    for (id, disk_bytes) in &totals.observed_disk_bytes {
        metadata
            .get_mut(id)
            .with_context(|| format!("missing local CAS metadata for Xorb {id}"))?
            .disk_bytes = *disk_bytes;
    }
    Ok(totals)
}

#[allow(clippy::too_many_arguments)]
fn upload_xorb_batch(
    batch: Vec<PendingXorb>,
    client: &Client,
    endpoint: &str,
    hub_token: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    cas_token: &mut Option<CasToken>,
    workers: usize,
) -> anyhow::Result<XorbUploadStats> {
    if batch.is_empty() {
        return Ok(XorbUploadStats::default());
    }
    let token = ensure_write_token(
        client, endpoint, hub_token, repo_id, repo_type, revision, cas_token,
    )?
    .clone();
    let uploaded = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let observed_disk_bytes = Mutex::new(HashMap::new());
    upload_bounded(batch, workers, |pending| {
        let result = xorb_head_state(client, &token, &pending.id)
            .with_context(|| format!("check Xet CAS Xorb {}", pending.id))?;
        match result {
            XorbHeadState::Present(content_length) => {
                record_observed_xorb_size(&observed_disk_bytes, &pending.id, content_length)?;
                skipped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            XorbHeadState::Missing | XorbHeadState::Unsupported => {
                let id = pending.id.clone();
                let uploaded_here = upload_xorb(client, &token, pending.xorb)
                    .with_context(|| format!("upload Xet CAS Xorb {id}"))?;
                if uploaded_here {
                    uploaded.fetch_add(1, Ordering::Relaxed);
                } else if result == XorbHeadState::Missing
                    && let XorbHeadState::Present(content_length) =
                        xorb_head_state(client, &token, &id)?
                {
                    record_observed_xorb_size(&observed_disk_bytes, &id, content_length)?;
                }
                Ok(())
            }
        }
    })?;
    Ok(XorbUploadStats {
        uploaded: uploaded.load(Ordering::Relaxed),
        skipped: skipped.load(Ordering::Relaxed),
        observed_disk_bytes: observed_disk_bytes
            .into_inner()
            .map_err(|_| anyhow::anyhow!("observed Xorb size state poisoned"))?,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XorbHeadState {
    Present(Option<u64>),
    Missing,
    Unsupported,
}

fn classify_xorb_head(
    status: StatusCode,
    content_length: Option<u64>,
) -> anyhow::Result<XorbHeadState> {
    if status.is_success() {
        return Ok(XorbHeadState::Present(content_length));
    }
    if status == StatusCode::NOT_FOUND {
        return Ok(XorbHeadState::Missing);
    }
    if matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    ) {
        return Ok(XorbHeadState::Unsupported);
    }
    bail!("CAS Xorb existence check returned HTTP {status}")
}

fn xorb_head_state(client: &Client, token: &CasToken, id: &str) -> anyhow::Result<XorbHeadState> {
    let mut url = Url::parse(&token.cas_url).context("invalid Xet CAS endpoint")?;
    append_segments(&mut url, &["v1", "xorbs", "default", id])?;
    let response = client
        .head(url)
        .timeout(Duration::from_secs(30))
        .bearer_auth(&token.access_token)
        .send()
        .map_err(|error| {
            anyhow::anyhow!("CAS Xorb existence request failed: {}", error.without_url())
        })?;
    let content_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .map(|value| {
            value
                .to_str()
                .context("CAS Xorb Content-Length is not valid text")?
                .parse::<u64>()
                .context("CAS Xorb Content-Length is not a valid byte count")
        })
        .transpose()?;
    classify_xorb_head(response.status(), content_length)
}

fn upload_bounded<T, F>(items: Vec<T>, workers: usize, upload: F) -> anyhow::Result<()>
where
    T: Send,
    F: Fn(T) -> anyhow::Result<()> + Sync,
{
    anyhow::ensure!(workers > 0, "upload workers must be greater than zero");
    if items.is_empty() {
        return Ok(());
    }
    let worker_count = workers.min(items.len());
    let queue = Mutex::new(VecDeque::from(items));
    let first_error = Mutex::new(None::<anyhow::Error>);
    let stopped = AtomicBool::new(false);
    let mut worker_panicked = false;

    thread::scope(|scope| {
        let handles = (0..worker_count)
            .map(|_| {
                scope.spawn(|| {
                    loop {
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        let item = queue.lock().expect("upload queue poisoned").pop_front();
                        let Some(item) = item else {
                            break;
                        };
                        if let Err(error) = upload(item) {
                            stopped.store(true, Ordering::Release);
                            if let Ok(mut first) = first_error.lock()
                                && first.is_none()
                            {
                                *first = Some(error);
                            }
                            break;
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            if handle.join().is_err() {
                stopped.store(true, Ordering::Release);
                worker_panicked = true;
            }
        }
    });

    if let Some(error) = first_error
        .into_inner()
        .map_err(|_| anyhow::anyhow!("upload error state poisoned"))?
    {
        return Err(error);
    }
    anyhow::ensure!(!worker_panicked, "Xet CAS upload worker panicked");
    Ok(())
}

fn cas_info(xorb: &UploadXorb) -> anyhow::Result<CasXorbInfo> {
    Ok(CasXorbInfo {
        xorb_hash: xorb.hash,
        raw_bytes: u32::try_from(xorb.raw_bytes).context("Xorb raw length exceeds shard format")?,
        disk_bytes: u32::try_from(xorb.bytes.len())
            .context("serialized Xorb length exceeds shard format")?,
        chunks: xorb
            .chunks
            .iter()
            .map(|chunk| {
                Ok(CasChunkInfo {
                    chunk_hash: chunk.hash,
                    offset: u32::try_from(chunk.offset)
                        .context("Xorb chunk offset exceeds shard format")?,
                    len: u32::try_from(chunk.len)
                        .context("Xorb chunk length exceeds shard format")?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
    })
}

fn validate_manifest_xorbs(file: &FileManifest, xorbs: &[CasXorbInfo]) -> anyhow::Result<()> {
    let by_hash: HashMap<hash::Hash, &CasXorbInfo> =
        xorbs.iter().map(|xorb| (xorb.xorb_hash, xorb)).collect();
    let mut file_chunks = Vec::new();
    let mut file_bytes = 0u64;
    for term in &file.terms {
        let xorb = by_hash
            .get(&term.xorb_hash)
            .context("file reconstruction references a missing Xorb")?;
        let start = term.chunk_start as usize;
        let end = term.chunk_end as usize;
        let chunks = xorb
            .chunks
            .get(start..end)
            .context("file reconstruction range exceeds its Xorb")?;
        let mut term_bytes = 0u64;
        let mut term_hashes = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            term_bytes = term_bytes
                .checked_add(chunk.len as u64)
                .context("file reconstruction length overflow")?;
            term_hashes.push(chunk.chunk_hash);
            file_chunks.push((chunk.chunk_hash, chunk.len as u64));
        }
        anyhow::ensure!(
            term_bytes == term.unpacked_bytes as u64
                && hash::verification_hash(&term_hashes) == term.verification_hash,
            "file reconstruction term verification mismatch"
        );
        file_bytes = file_bytes
            .checked_add(term_bytes)
            .context("file length overflow")?;
    }
    anyhow::ensure!(
        file_bytes == file.size && hash::file_hash(&file_chunks) == file.file_hash,
        "file reconstruction hash or size does not match the local file"
    );
    Ok(())
}

fn ensure_write_token<'a>(
    client: &Client,
    endpoint: &str,
    hub_token: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    token: &'a mut Option<CasToken>,
) -> anyhow::Result<&'a CasToken> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if token
        .as_ref()
        .is_none_or(|value| value.exp <= now.saturating_add(TOKEN_REFRESH_SKEW))
    {
        *token = Some(request_write_token(
            client, endpoint, hub_token, repo_id, repo_type, revision,
        )?);
    }
    Ok(token.as_ref().expect("write token populated"))
}

fn request_write_token(
    client: &Client,
    endpoint: &str,
    hub_token: &str,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
) -> anyhow::Result<CasToken> {
    let url =
        remote_cas::hub_repo_api_url(endpoint, repo_id, repo_type, &["xet-write-token", revision])?;
    let response = client
        .get(url)
        .bearer_auth(hub_token)
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .map_err(|error| {
            anyhow::anyhow!(
                "Hub Xet write-token request failed: {}",
                error.without_url()
            )
        })?;
    if !response.status().is_success() {
        bail!(
            "Hub Xet write-token request returned HTTP {}",
            response.status()
        );
    }
    let token: CasToken = response
        .json()
        .context("decode Hub Xet write-token response")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    anyhow::ensure!(
        token.exp > now.saturating_add(TOKEN_REFRESH_SKEW),
        "Hub returned a write token that is expired or near expiry"
    );
    let url = Url::parse(&token.cas_url).context("Hub returned an invalid CAS endpoint")?;
    validate_service_url(&url, "Hub CAS endpoint")?;
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Hub CAS endpoint contains user information"
    );
    Ok(token)
}

fn upload_xorb(client: &Client, token: &CasToken, xorb: UploadXorb) -> anyhow::Result<bool> {
    let id = hash::to_hex(&xorb.hash);
    let mut url = Url::parse(&token.cas_url).context("invalid Xet CAS endpoint")?;
    append_segments(&mut url, &["v1", "xorbs", "default", &id])?;
    let body = bytes::Bytes::from(xorb.bytes);
    let body_len = body.len();
    let mut last_error = String::new();
    for attempt in 0..CAS_UPLOAD_MAX_ATTEMPTS {
        let response = client
            .post(url.clone())
            .bearer_auth(&token.access_token)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, body_len)
            .body(body.clone())
            .send();
        match response {
            Err(error) if error.is_timeout() || error.is_connect() => {
                last_error = format!("request failed: {}", error.without_url());
            }
            Err(error) => {
                bail!("CAS Xorb upload failed: {}", error.without_url());
            }
            Ok(response) if response.status().is_success() => {
                let result: Value = response.json().context("decode CAS Xorb upload response")?;
                return result
                    .get("was_inserted")
                    .and_then(Value::as_bool)
                    .context("CAS Xorb upload response omitted was_inserted");
            }
            Ok(response) => {
                let status = response.status();
                let request_id = response
                    .headers()
                    .get("x-request-id")
                    .or_else(|| response.headers().get("x-amzn-trace-id"))
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = response_bytes(response, CAS_ERROR_BODY_LIMIT, "CAS Xorb error body")
                    .unwrap_or_default();
                let excerpt = String::from_utf8_lossy(&body)
                    .chars()
                    .filter(|character| {
                        !character.is_control() || *character == '\n' || *character == '\t'
                    })
                    .take(1024)
                    .collect::<String>();
                last_error = format!(
                    "HTTP {status}{}{}",
                    request_id
                        .map(|value| format!(" (request id {value})"))
                        .unwrap_or_default(),
                    if excerpt.is_empty() {
                        String::new()
                    } else {
                        format!(": {excerpt}")
                    }
                );
                if !(status.is_server_error()
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::TOO_MANY_REQUESTS)
                {
                    bail!("CAS Xorb upload returned {last_error}");
                }
            }
        }

        if attempt + 1 < CAS_UPLOAD_MAX_ATTEMPTS {
            let delay = Duration::from_millis(500u64.saturating_mul(1 << attempt));
            std::thread::sleep(delay);
        }
    }
    bail!(
        "CAS Xorb upload failed after {CAS_UPLOAD_MAX_ATTEMPTS} attempts for {id} ({} bytes): {last_error}",
        body_len
    )
}

fn record_observed_xorb_size(
    observed_disk_bytes: &Mutex<HashMap<String, u32>>,
    id: &str,
    content_length: Option<u64>,
) -> anyhow::Result<()> {
    let Some(content_length) = content_length else {
        return Ok(());
    };
    anyhow::ensure!(
        content_length > 0,
        "CAS Xorb has an empty stored representation"
    );
    let disk_bytes =
        u32::try_from(content_length).context("stored CAS Xorb exceeds shard format")?;
    observed_disk_bytes
        .lock()
        .map_err(|_| anyhow::anyhow!("observed Xorb size state poisoned"))?
        .insert(id.to_owned(), disk_bytes);
    Ok(())
}

fn upload_shard(client: &Client, token: &CasToken, shard: &[u8]) -> anyhow::Result<()> {
    let mut v2_url = Url::parse(&token.cas_url).context("invalid Xet CAS endpoint")?;
    append_segments(&mut v2_url, &["v2", "shards"])?;
    let response = client
        .post(v2_url)
        .bearer_auth(&token.access_token)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(shard.to_vec())
        .send()
        .map_err(|error| anyhow::anyhow!("CAS shard upload failed: {}", error.without_url()))?;
    if response.status() == StatusCode::NOT_FOUND {
        let mut v1_url = Url::parse(&token.cas_url).context("invalid Xet CAS endpoint")?;
        append_segments(&mut v1_url, &["v1", "shards"])?;
        let response = client
            .post(v1_url)
            .bearer_auth(&token.access_token)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(shard.to_vec())
            .send()
            .map_err(|error| {
                anyhow::anyhow!("legacy CAS shard upload failed: {}", error.without_url())
            })?;
        if !response.status().is_success() {
            bail!(
                "legacy CAS shard upload returned HTTP {}",
                response.status()
            );
        }
        let body = response_bytes(response, MAX_RESPONSE_BYTES, "CAS shard response")?;
        let _: Value = serde_json::from_slice(&body).context("decode CAS shard response")?;
        return Ok(());
    }
    if !response.status().is_success() {
        bail!("CAS shard upload returned HTTP {}", response.status());
    }
    let body = response_bytes(response, MAX_RESPONSE_BYTES, "CAS shard progress")?;
    let mut terminal_result = false;
    for line in body
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: Value =
            serde_json::from_slice(line).context("decode CAS shard progress event")?;
        match event.get("type").and_then(Value::as_str) {
            Some("error") => bail!(
                "CAS shard registration failed: {}",
                event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ),
            Some("result") => terminal_result = true,
            _ => {}
        }
    }
    anyhow::ensure!(
        terminal_result,
        "CAS shard stream ended without a terminal result"
    );
    Ok(())
}

fn serialize_upload_shard(file: &FileManifest, xorbs: &[CasXorbInfo]) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    output.extend_from_slice(&SHARD_HEADER_TAG);
    output.extend_from_slice(&2u64.to_le_bytes());
    output.extend_from_slice(&0u64.to_le_bytes());

    output.extend_from_slice(&file.file_hash);
    output.extend_from_slice(
        &(FILE_FLAG_WITH_VERIFICATION | FILE_FLAG_WITH_METADATA_EXT).to_le_bytes(),
    );
    output.extend_from_slice(
        &u32::try_from(file.terms.len())
            .context("file has too many Xet reconstruction terms")?
            .to_le_bytes(),
    );
    output.extend_from_slice(&0u64.to_le_bytes());
    for term in &file.terms {
        output.extend_from_slice(&term.xorb_hash);
        output.extend_from_slice(&0u32.to_le_bytes());
        output.extend_from_slice(&term.unpacked_bytes.to_le_bytes());
        output.extend_from_slice(&term.chunk_start.to_le_bytes());
        output.extend_from_slice(&term.chunk_end.to_le_bytes());
    }
    for term in &file.terms {
        output.extend_from_slice(&term.verification_hash);
        output.extend_from_slice(&[0u8; 16]);
    }
    // Xet's DataHash stores canonical hashes as four little-endian u64 words.
    // The LFS OID remains conventional SHA-256 hex, but the shard metadata
    // extension must use the DataHash byte layout expected by xet-core.
    output.extend_from_slice(&sha256_data_hash_bytes(&file.sha256));
    output.extend_from_slice(&[0u8; 16]);
    append_bookend(&mut output);

    let mut seen = HashSet::new();
    for xorb in xorbs {
        let id = hash::to_hex(&xorb.xorb_hash);
        anyhow::ensure!(seen.insert(id), "duplicate Xorb metadata in one shard");
        output.extend_from_slice(&xorb.xorb_hash);
        output.extend_from_slice(&0u32.to_le_bytes());
        output.extend_from_slice(
            &u32::try_from(xorb.chunks.len())
                .context("Xorb has too many chunks for shard format")?
                .to_le_bytes(),
        );
        output.extend_from_slice(&xorb.raw_bytes.to_le_bytes());
        output.extend_from_slice(&xorb.disk_bytes.to_le_bytes());
        for chunk in &xorb.chunks {
            output.extend_from_slice(&chunk.chunk_hash);
            output.extend_from_slice(&chunk.offset.to_le_bytes());
            output.extend_from_slice(&chunk.len.to_le_bytes());
            output.extend_from_slice(&[0u8; 8]);
        }
    }
    for term in &file.terms {
        anyhow::ensure!(
            seen.contains(&hash::to_hex(&term.xorb_hash)),
            "file reconstruction references a Xorb missing from shard metadata"
        );
    }
    append_bookend(&mut output);
    anyhow::ensure!(
        output.len() <= MAX_SHARD_BYTES,
        "serialized Xet shard exceeds the 64 MiB Hub CAS limit"
    );
    Ok(output)
}

fn append_bookend(output: &mut Vec<u8>) {
    output.extend_from_slice(&[0xff; 32]);
    output.extend_from_slice(&[0u8; 16]);
}

fn sha256_data_hash_bytes(sha256: &[u8; 32]) -> [u8; 32] {
    let mut output = [0u8; 32];
    let (output_words, _) = output.as_chunks_mut::<8>();
    let (sha256_words, _) = sha256.as_chunks::<8>();
    for (destination, source) in output_words.iter_mut().zip(sha256_words) {
        *destination = u64::from_be_bytes(*source).to_le_bytes();
    }
    output
}

fn commit_body(message: &str, operations: Vec<Value>) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    let header = json!({
        "key": "header",
        "value": {"summary": message, "description": ""}
    });
    body.extend_from_slice(&serde_json::to_vec(&header)?);
    body.push(b'\n');
    for operation in operations {
        body.extend_from_slice(&serde_json::to_vec(&operation)?);
        body.push(b'\n');
    }
    Ok(body)
}

fn snapshot_bytes(
    snapshot: &crate::repository::UploadSnapshot,
    size: u64,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        size <= MAX_INLINE_BYTES,
        "inline Hub file exceeds the XetFS limit"
    );
    let capacity = usize::try_from(size).context("inline file exceeds addressable memory")?;
    let mut output = Vec::with_capacity(capacity);
    let mut offset = 0u64;
    while offset < size {
        let length = (size - offset).min(1024 * 1024) as usize;
        let bytes = snapshot.read_at(offset, length)?;
        anyhow::ensure!(
            bytes.len() == length,
            "local file ended before its recorded size"
        );
        output.extend_from_slice(&bytes);
        offset += length as u64;
    }
    Ok(output)
}

fn response_bytes(
    response: reqwest::blocking::Response,
    limit: u64,
    description: &str,
) -> anyhow::Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(length <= limit, "{description} exceeds its size limit");
    }
    let mut bytes = Vec::new();
    response
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {description}"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "{description} exceeds its size limit"
    );
    Ok(bytes)
}

fn response_excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .take(1024)
        .collect()
}

fn append_segments(url: &mut Url, segments: &[&str]) -> anyhow::Result<()> {
    let mut path = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("service endpoint cannot accept URL path segments"))?;
    path.pop_if_empty();
    path.extend(segments.iter().copied());
    Ok(())
}

fn validate_service_url(url: &Url, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        url.scheme() == "https" || url.host_str() == Some("localhost"),
        "{description} must use HTTPS"
    );
    anyhow::ensure!(url.host().is_some(), "{description} must have a host");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{Arc, Barrier, atomic::AtomicUsize},
    };

    #[test]
    fn xorb_upload_scheduler_runs_concurrently_with_a_fixed_bound() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let start_together = Arc::new(Barrier::new(3));
        upload_bounded(vec![0; 9], 3, {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let completed = Arc::clone(&completed);
            let start_together = Arc::clone(&start_together);
            move |_| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                start_together.wait();
                thread::sleep(Duration::from_millis(5));
                active.fetch_sub(1, Ordering::SeqCst);
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .unwrap();

        assert_eq!(completed.load(Ordering::SeqCst), 9);
        assert_eq!(maximum.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn xorb_upload_scheduler_propagates_worker_errors() {
        let result = upload_bounded(vec![0, 1, 2, 3], 4, |item| {
            if item == 0 {
                bail!("injected upload failure");
            }
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn xorb_head_check_skips_matching_objects_and_falls_back_safely() {
        assert_eq!(
            classify_xorb_head(StatusCode::OK, Some(64)).unwrap(),
            XorbHeadState::Present(Some(64))
        );
        assert_eq!(
            classify_xorb_head(StatusCode::OK, Some(63)).unwrap(),
            XorbHeadState::Present(Some(63))
        );
        assert_eq!(
            classify_xorb_head(StatusCode::OK, None).unwrap(),
            XorbHeadState::Present(None)
        );
        assert_eq!(
            classify_xorb_head(StatusCode::NOT_FOUND, None).unwrap(),
            XorbHeadState::Missing
        );
        for status in [
            StatusCode::FORBIDDEN,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::NOT_IMPLEMENTED,
        ] {
            assert_eq!(
                classify_xorb_head(status, None).unwrap(),
                XorbHeadState::Unsupported
            );
        }
        assert!(classify_xorb_head(StatusCode::BAD_GATEWAY, None).is_err());
    }

    #[test]
    fn lfs_batch_url_matches_hub_repo_route() {
        assert_eq!(
            lfs_batch_url("https://huggingface.co/", "alice/data", "dataset")
                .unwrap()
                .as_str(),
            "https://huggingface.co/datasets/alice/data.git/info/lfs/objects/batch"
        );
        assert_eq!(
            lfs_batch_url("https://hub.example/base", "alice/model", "model")
                .unwrap()
                .as_str(),
            "https://hub.example/base/alice/model.git/info/lfs/objects/batch"
        );
    }

    #[test]
    fn upload_shard_roundtrips_with_official_reader() {
        let chunk_hash = hash::chunk_hash(b"protocol-compatible chunk");
        let xorb_hash = hash::xorb_hash(&[(chunk_hash, 25)]);
        let file_hash = hash::file_hash(&[(chunk_hash, 25)]);
        let sha256 = std::array::from_fn(|index| index as u8);
        let manifest = FileManifest {
            file_hash,
            sha256,
            size: 25,
            terms: vec![FileTerm {
                xorb_hash,
                unpacked_bytes: 25,
                chunk_start: 0,
                chunk_end: 1,
                verification_hash: hash::verification_hash(&[chunk_hash]),
            }],
        };
        let cas = CasXorbInfo {
            xorb_hash,
            raw_bytes: 25,
            disk_bytes: 33,
            chunks: vec![CasChunkInfo {
                chunk_hash,
                offset: 0,
                len: 25,
            }],
        };
        let shard = serialize_upload_shard(&manifest, &[cas]).unwrap();
        let mut input = Cursor::new(shard);
        let parsed =
            xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard::from_reader(
                &mut input, true, true,
            )
            .unwrap();
        assert_eq!(parsed.num_files(), 1);
        assert_eq!(parsed.num_xorb(), 1);
        assert!(parsed.has_file_verification());
        assert_eq!(
            parsed.file(0).unwrap().metadata_ext().unwrap().sha256,
            xet_core_structures::metadata_shard::Sha256::from_be_bytes(sha256)
        );
    }

    #[test]
    fn multi_xorb_upload_shard_matches_official_serialization() {
        use xet_core_structures::metadata_shard::{
            file_structs::{
                FileDataSequenceEntry, FileDataSequenceHeader, FileMetadataExt,
                FileVerificationEntry,
            },
            shard_format::MDBShardFileHeader,
            xorb_structs::{MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader},
        };

        let first_chunks = [
            (hash::chunk_hash(b"first xorb chunk one"), 20u64),
            (hash::chunk_hash(b"first xorb chunk two"), 21u64),
        ];
        let second_chunks = [(hash::chunk_hash(b"second xorb chunk"), 18u64)];
        let first_hash = hash::xorb_hash(&first_chunks);
        let second_hash = hash::xorb_hash(&second_chunks);
        let file_chunks = [first_chunks[0], first_chunks[1], second_chunks[0]];
        let manifest = FileManifest {
            file_hash: hash::file_hash(&file_chunks),
            sha256: std::array::from_fn(|index| (31 - index) as u8),
            size: file_chunks.iter().map(|(_, len)| len).sum(),
            terms: vec![
                FileTerm {
                    xorb_hash: first_hash,
                    unpacked_bytes: 41,
                    chunk_start: 0,
                    chunk_end: 2,
                    verification_hash: hash::verification_hash(&[
                        first_chunks[0].0,
                        first_chunks[1].0,
                    ]),
                },
                FileTerm {
                    xorb_hash: second_hash,
                    unpacked_bytes: 18,
                    chunk_start: 0,
                    chunk_end: 1,
                    verification_hash: hash::verification_hash(&[second_chunks[0].0]),
                },
            ],
        };
        let mut first_offset = 0;
        let first = CasXorbInfo {
            xorb_hash: first_hash,
            raw_bytes: 41,
            disk_bytes: 57,
            chunks: first_chunks
                .iter()
                .map(|(chunk_hash, len)| {
                    let chunk = CasChunkInfo {
                        chunk_hash: *chunk_hash,
                        offset: first_offset,
                        len: *len as u32,
                    };
                    first_offset += *len as u32;
                    chunk
                })
                .collect(),
        };
        let second = CasXorbInfo {
            xorb_hash: second_hash,
            raw_bytes: 18,
            disk_bytes: 34,
            chunks: vec![CasChunkInfo {
                chunk_hash: second_chunks[0].0,
                offset: 0,
                len: 18,
            }],
        };
        let xorbs = vec![first.clone(), second.clone()];

        let actual = serialize_upload_shard(&manifest, &xorbs).unwrap();
        let mut expected = Vec::new();
        let header = MDBShardFileHeader {
            footer_size: 0,
            ..MDBShardFileHeader::default()
        };
        header.serialize(&mut expected).unwrap();

        let file_header = FileDataSequenceHeader::new(manifest.file_hash.into(), 2, true, true);
        file_header.serialize(&mut expected).unwrap();
        for term in &manifest.terms {
            FileDataSequenceEntry::new(
                term.xorb_hash.into(),
                term.unpacked_bytes,
                term.chunk_start,
                term.chunk_end,
            )
            .serialize(&mut expected)
            .unwrap();
        }
        for term in &manifest.terms {
            FileVerificationEntry::new(term.verification_hash.into())
                .serialize(&mut expected)
                .unwrap();
        }
        FileMetadataExt::new(xet_core_structures::metadata_shard::Sha256::from_be_bytes(
            manifest.sha256,
        ))
        .serialize(&mut expected)
        .unwrap();
        FileDataSequenceHeader::bookend()
            .serialize(&mut expected)
            .unwrap();
        for xorb in &xorbs {
            let mut metadata = XorbChunkSequenceHeader::new(
                xorb.xorb_hash.into(),
                xorb.chunks.len(),
                xorb.raw_bytes,
            );
            metadata.num_bytes_on_disk = xorb.disk_bytes;
            MDBXorbInfo {
                metadata,
                chunks: xorb
                    .chunks
                    .iter()
                    .map(|chunk| {
                        XorbChunkSequenceEntry::new(
                            chunk.chunk_hash.into(),
                            chunk.len,
                            chunk.offset,
                        )
                    })
                    .collect(),
            }
            .serialize(&mut expected)
            .unwrap();
        }
        XorbChunkSequenceHeader::bookend()
            .serialize(&mut expected)
            .unwrap();

        assert_eq!(actual, expected);
        let mut input = Cursor::new(actual);
        let parsed =
            xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard::from_reader(
                &mut input, true, true,
            )
            .unwrap();
        assert_eq!(parsed.num_files(), 1);
        assert_eq!(parsed.num_xorb(), 2);
        let file = parsed.file(0).unwrap();
        assert_eq!(file.num_entries(), 2);
        assert_eq!(file.entry(0).xorb_hash, first_hash.into());
        assert_eq!(file.entry(0).chunk_index_start, 0);
        assert_eq!(file.entry(0).chunk_index_end, 2);
        assert_eq!(file.entry(1).xorb_hash, second_hash.into());
        assert_eq!(
            file.metadata_ext().unwrap().sha256,
            xet_core_structures::metadata_shard::Sha256::from_be_bytes(manifest.sha256)
        );
    }

    #[test]
    fn upload_shard_rejects_duplicate_or_overlong_entries() {
        let hash = [1; 32];
        let manifest = FileManifest {
            file_hash: hash,
            sha256: [2; 32],
            size: 1,
            terms: vec![FileTerm {
                xorb_hash: hash,
                unpacked_bytes: 1,
                chunk_start: 0,
                chunk_end: 1,
                verification_hash: hash,
            }],
        };
        let cas = CasXorbInfo {
            xorb_hash: hash,
            raw_bytes: 1,
            disk_bytes: 9,
            chunks: vec![CasChunkInfo {
                chunk_hash: hash,
                offset: 0,
                len: 1,
            }],
        };
        assert!(serialize_upload_shard(&manifest, &[cas.clone(), cas]).is_err());
        assert!(serialize_upload_shard(&manifest, &[]).is_err());
    }
}
