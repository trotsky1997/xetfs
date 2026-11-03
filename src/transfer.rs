//! Portable immutable bundles: original Xorb records plus Xet file recipes.
use crate::{FileSystem, Kind, hash, xorb};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Cursor, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
};

// shortcut: monolithic manifests are bounded at 8 MiB; shard recipes before serving larger bundles.
pub const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk {
    xet: String,
    raw: u64,
    stored: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    mode: u32,
    content: Option<String>,
    size: u64,
    chunks: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    protocol: String,
    entries: Vec<Entry>,
    chunks: BTreeMap<String, Chunk>,
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn hex(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v)),
        "invalid object hash"
    );
    Ok(())
}
fn relative(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && !value.contains('\0')
            && value
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != "..")
            && Path::new(value)
                .components()
                .all(|v| matches!(v, Component::Normal(_))),
        "invalid bundle path"
    );
    Ok(())
}
fn durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::options().write(true).create_new(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// The repository lock fixes a consistent namespace while chunks are copied.
pub fn export(fs: &FileSystem, path: &Path, destination: &Path) -> Result<serde_json::Value> {
    ensure!(!destination.exists(), "bundle destination already exists");
    let node = fs.resolve_nofollow(path)?;
    ensure!(
        node.kind == Kind::Directory,
        "bundle source must be a directory"
    );
    let mut pending = vec![node.ino];
    while let Some(ino) = pending.pop() {
        ensure!(
            fs.list_xattrs(ino)?.is_empty(),
            "XetLink bundles cannot preserve extended attributes"
        );
        if fs.node(ino)?.kind == Kind::Directory {
            pending.extend(fs.list(ino)?.into_iter().map(|entry| entry.node.ino));
        }
    }
    fs::create_dir(destination)?;
    fs::create_dir(destination.join("chunks"))?;
    let mut manifest = Manifest {
        protocol: "xetlink/1".into(),
        entries: Vec::new(),
        chunks: BTreeMap::new(),
    };
    let mut queue = vec![(node.ino, String::new())];
    let mut cached: Option<(String, Vec<u8>)> = None;
    while let Some((ino, parent)) = queue.pop() {
        let mut entries = fs.list(ino)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            ensure!(manifest.entries.len() < 20000, "too many bundle files");
            let name = std::str::from_utf8(&entry.name).context("bundle names must be UTF-8")?;
            let path = if parent.is_empty() {
                name.into()
            } else {
                format!("{parent}/{name}")
            };
            relative(&path)?;
            let n = entry.node;
            ensure!(n.kind != Kind::Symlink, "bundle symlinks are unsupported");
            let mut output = Entry {
                path: path.clone(),
                mode: n.mode & 0o777,
                content: n.content.clone(),
                size: n.size,
                chunks: Vec::new(),
            };
            if n.kind == Kind::Directory {
                output.size = 0;
                queue.push((n.ino, path));
            } else {
                let mut stmt = fs.repository().conn.prepare("SELECT fc.hash,fc.len,xc.xorb,xc.offset,xc.stored FROM file_chunks fc JOIN chunks c ON fc.hash=c.hash JOIN xorb_chunks xc ON c.xorb=xc.xorb AND c.idx=xc.idx WHERE fc.content=?1 ORDER BY fc.idx")?;
                let rows = stmt.query_map([n.content.context("missing file recipe")?], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, u64>(3)?,
                        r.get::<_, u64>(4)?,
                    ))
                })?;
                for row in rows {
                    let (xet, raw, object, offset, stored) = row?;
                    if cached.as_ref().map(|c| &c.0) != Some(&object) {
                        cached = Some((
                            object.clone(),
                            fs.repository().read_upload_xorb(&object)?.bytes,
                        ));
                    }
                    let bytes = cached
                        .as_ref()
                        .unwrap()
                        .1
                        .get(offset as usize..(offset + stored) as usize)
                        .context("invalid chunk range")?;
                    let id = sha(bytes);
                    if !manifest.chunks.contains_key(&id) {
                        durable(&destination.join("chunks").join(&id), bytes)?;
                        manifest
                            .chunks
                            .insert(id.clone(), Chunk { xet, raw, stored });
                    }
                    output.chunks.push(id);
                }
            }
            manifest.entries.push(output);
        }
    }
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    let bytes = serde_json::to_vec(&manifest)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST,
        "bundle manifest too large"
    );
    durable(&destination.join("manifest.json"), &bytes)?;
    File::open(destination.join("chunks"))?.sync_all()?;
    File::open(destination)?.sync_all()?;
    Ok(
        serde_json::json!({"bundle":sha(&bytes),"chunks":manifest.chunks.len(),"entries":manifest.entries.len(),"stored_bytes":manifest.chunks.values().map(|c| c.stored).sum::<u64>()}),
    )
}

/// Restore into a new staging directory; caller publishes it after success.
pub fn restore(
    bundle: &Path,
    cache: &Path,
    destination: &Path,
    expected: &str,
) -> Result<serde_json::Value> {
    hex(expected)?;
    let mut bytes = Vec::new();
    File::open(bundle)?
        .take(MAX_MANIFEST + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST && sha(&bytes) == expected,
        "manifest hash mismatch"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.protocol == "xetlink/1" && manifest.entries.len() <= 20000,
        "invalid bundle protocol or bounds"
    );
    for (id, c) in &manifest.chunks {
        hex(id)?;
        hex(&c.xet)?;
        ensure!(
            c.raw > 0 && c.raw <= 128 * 1024 && c.stored >= 8 && c.stored <= 128 * 1024 + 8,
            "invalid chunk bounds"
        );
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &manifest.entries {
        relative(&entry.path)?;
        ensure!(
            seen.insert(&entry.path) && entry.mode <= 0o777,
            "duplicate path or invalid mode"
        );
        if let Some(id) = &entry.content {
            hex(id)?;
        } else {
            ensure!(
                entry.chunks.is_empty() && entry.size == 0,
                "invalid directory entry"
            );
        }
    }
    fs::create_dir(destination)?;
    let mut logical_bytes = 0u64;
    for entry in &manifest.entries {
        let path = destination.join(&entry.path);
        if entry.content.is_none() {
            fs::create_dir_all(&path)?;
            continue;
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let mut output = File::options().create_new(true).write(true).open(&path)?;
        let mut recipe = Vec::new();
        let mut size = 0;
        for id in &entry.chunks {
            let c = manifest
                .chunks
                .get(id)
                .context("chunk absent from manifest")?;
            let mut encoded = Vec::new();
            File::open(cache.join(id))?
                .take(c.stored + 1)
                .read_to_end(&mut encoded)?;
            ensure!(
                encoded.len() as u64 == c.stored && sha(&encoded) == *id,
                "chunk SHA-256 mismatch"
            );
            let mut cursor = Cursor::new(&encoded);
            let (raw, stored) = xorb::decode(&mut cursor)?.context("empty chunk")?;
            let xet = hash::chunk_hash(&raw);
            ensure!(
                stored == c.stored
                    && cursor.position() == c.stored
                    && raw.len() as u64 == c.raw
                    && hash::to_hex(&xet) == c.xet,
                "Xet chunk integrity mismatch"
            );
            output.write_all(&raw)?;
            recipe.push((xet, c.raw));
            size += c.raw;
        }
        ensure!(
            size == entry.size
                && hash::to_hex(&hash::file_hash(&recipe)) == *entry.content.as_ref().unwrap(),
            "file recipe mismatch"
        );
        output.set_permissions(fs::Permissions::from_mode(entry.mode))?;
        output.sync_all()?;
        logical_bytes += size;
    }
    // Apply restrictive directory modes only after descendants are durable.
    for entry in manifest
        .entries
        .iter()
        .rev()
        .filter(|e| e.content.is_none())
    {
        let path = destination.join(&entry.path);
        File::open(&path)?.sync_all()?;
        fs::set_permissions(path, fs::Permissions::from_mode(entry.mode))?;
    }
    File::open(destination)?.sync_all()?;
    Ok(
        serde_json::json!({"bundle":expected,"logical_bytes":logical_bytes,"entries":manifest.entries.len()}),
    )
}
