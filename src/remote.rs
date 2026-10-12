//! Hugging Face Hub transfers backed by the official Xet client.
//!
//! `hf-hub` delegates Xet files to Hugging Face's `hf-xet` Rust client. This
//! keeps Hub token refresh, Xet shard registration, range downloads, retries,
//! and the Hub Git/LFS commit format in the upstream implementation.

use crate::{Error, FileSystem, Kind, Node, Result};
use anyhow::{Context, bail};
use hf_hub::{HFClientSync, HFRepositorySync, RepoTypeAny, repository::CommitOperation, split_id};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

const IO_BLOCK: usize = 1024 * 1024;

#[derive(Debug, serde::Serialize)]
pub struct PullReport {
    pub repository: String,
    pub revision: String,
    pub files_downloaded: u64,
    pub files_skipped: u64,
    pub bytes_downloaded: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct PushReport {
    pub repository: String,
    pub revision: String,
    pub files_uploaded: u64,
    pub symlinks_skipped: u64,
    pub bytes_uploaded: u64,
    pub commit_oid: Option<String>,
    pub commit_url: Option<String>,
}

fn open_repo(repo_id: &str, repo_type: &str) -> anyhow::Result<HFRepositorySync<RepoTypeAny>> {
    let (owner, name) = split_id(repo_id);
    if name.is_empty()
        || repo_id.contains("//")
        || repo_id.starts_with('/')
        || repo_id.ends_with('/')
    {
        bail!("invalid Hugging Face repository id: {repo_id}");
    }
    let kind: RepoTypeAny = repo_type
        .parse()
        .context("repo type must be model, dataset, space, or kernel")?;
    let client = HFClientSync::new().context("initialize Hugging Face client and token")?;
    Ok(client.repository(kind, owner, name))
}

fn validate_repo_path(path: &str) -> anyhow::Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("Hub returned an unsafe or invalid repository path: {path:?}");
    }
    Ok(())
}

fn validate_prefix(prefix: Option<&str>) -> anyhow::Result<()> {
    if let Some(prefix) = prefix {
        validate_repo_path(prefix)
            .context("prefix must be a non-empty repository-relative directory path")?;
    }
    Ok(())
}

fn virtual_path(repo_path: &str) -> PathBuf {
    Path::new("/").join(repo_path)
}

fn local_collision(fs: &FileSystem, path: &Path) -> Result<Option<Node>> {
    match fs.resolve_nofollow(path) {
        Ok(node) => Ok(Some(node)),
        Err(Error::Errno(libc::ENOENT)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Download a Hub revision through Xet and import its files into the local CAS.
///
/// Existing files require `force`; files with an identical Xet file hash are
/// skipped without rewriting local objects.
pub fn pull(
    fs: &mut FileSystem,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    prefix: Option<&str>,
    force: bool,
) -> anyhow::Result<PullReport> {
    validate_prefix(prefix)?;
    let repo = open_repo(repo_id, repo_type)?;
    let entries = match prefix {
        Some(path) => repo
            .list_tree()
            .revision(revision.to_owned())
            .path_in_repo(path.to_owned())
            .recursive(true)
            .expand(true)
            .send(),
        None => repo
            .list_tree()
            .revision(revision.to_owned())
            .recursive(true)
            .expand(true)
            .send(),
    }
    .context("list Hugging Face repository files")?;

    let mut files = Vec::new();
    for entry in entries {
        if let hf_hub::repository::RepoTreeEntry::File {
            path,
            size,
            xet_hash,
            ..
        } = entry
        {
            validate_repo_path(&path)?;
            files.push((path, size, xet_hash));
        }
    }

    // Validate all destination paths before doing network transfers or local writes.
    let mut planned = Vec::with_capacity(files.len());
    for (remote_path, size, xet_hash) in files {
        let target = virtual_path(&remote_path);
        let existing = local_collision(fs, &target)?;
        let skip = match existing {
            Some(node) if node.kind == Kind::File && !force => {
                if node.size == size && xet_hash.as_deref() == node.content.as_deref() {
                    true
                } else {
                    bail!(
                        "local file {} already exists; pass --force to replace it",
                        target.display()
                    );
                }
            }
            Some(node) if node.kind != Kind::File => {
                bail!("cannot replace non-file local path {}", target.display());
            }
            Some(_) => false,
            None => false,
        };

        // A local file cannot serve as a parent directory of an imported file.
        let mut parent = target.parent();
        while let Some(path) = parent {
            if path == Path::new("/") {
                break;
            }
            if let Some(node) = local_collision(fs, path)?
                && node.kind != Kind::Directory
            {
                bail!("local parent path {} is not a directory", path.display());
            }
            parent = path.parent();
        }
        planned.push((remote_path, target, size, skip));
    }

    let staging = tempfile::tempdir().context("create temporary Hub download directory")?;
    let mut staged = Vec::new();
    let mut files_skipped = 0u64;
    for (remote_path, target, expected_size, skip) in planned {
        if skip {
            files_skipped += 1;
            continue;
        }
        let downloaded = repo
            .download_file()
            .filename(remote_path.clone())
            .local_dir(staging.path().to_path_buf())
            .revision(revision.to_owned())
            .force_download(true)
            .send()
            .with_context(|| format!("download {remote_path} through Hugging Face Xet"))?;
        let actual_size = fs::metadata(&downloaded)
            .with_context(|| format!("inspect downloaded file {remote_path}"))?
            .len();
        if actual_size != expected_size {
            bail!("downloaded {remote_path} has size {actual_size}, expected {expected_size}");
        }
        staged.push((remote_path, target, downloaded, actual_size));
    }

    let mut bytes_downloaded = 0u64;
    for (remote_path, target, downloaded, size) in &staged {
        let parent = target.parent().unwrap_or(Path::new("/"));
        fs.mkdir_all(parent)
            .with_context(|| format!("create local parent {}", parent.display()))?;
        let source = File::open(downloaded)
            .with_context(|| format!("open staged download {remote_path}"))?;
        fs.put_file(target, source)
            .with_context(|| format!("import {remote_path} into XetFS"))?;
        bytes_downloaded = bytes_downloaded.saturating_add(*size);
    }

    Ok(PullReport {
        repository: repo_id.to_owned(),
        revision: revision.to_owned(),
        files_downloaded: staged.len() as u64,
        files_skipped,
        bytes_downloaded,
    })
}

struct LocalFile {
    repo_path: String,
    inode: u64,
    size: u64,
}

fn collect_local_files(
    fs: &FileSystem,
    node: Node,
    path: String,
    files: &mut Vec<LocalFile>,
    skipped_symlinks: &mut u64,
) -> anyhow::Result<()> {
    match node.kind {
        Kind::File => files.push(LocalFile {
            repo_path: path,
            inode: node.ino,
            size: node.size,
        }),
        Kind::Directory => {
            for entry in fs.list(node.ino)? {
                let name = String::from_utf8(entry.name.clone())
                    .context("Hugging Face Hub paths require UTF-8 filenames")?;
                if name == "." || name == ".." || name.contains('/') || name.contains('\0') {
                    bail!("local filename cannot be represented in a Hub path: {name:?}");
                }
                let child_path = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{}/{name}", path.trim_end_matches('/'))
                };
                collect_local_files(fs, entry.node, child_path, files, skipped_symlinks)?;
            }
        }
        Kind::Symlink => *skipped_symlinks += 1,
    }
    Ok(())
}

/// Upload regular files as one Hub commit. `hf-hub` selects Xet LFS transfer
/// where the Hub classifies a file for Xet storage, then commits the matching
/// Git LFS pointers to the requested revision.
pub fn push(
    fs: &FileSystem,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    prefix: Option<&str>,
    commit_message: &str,
) -> anyhow::Result<PushReport> {
    validate_prefix(prefix)?;
    let repo = open_repo(repo_id, repo_type)?;
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

    let staging = tempfile::tempdir().context("create temporary Hub upload directory")?;
    let mut operations = Vec::with_capacity(local_files.len());
    let mut bytes_uploaded = 0u64;
    for (index, entry) in local_files.iter().enumerate() {
        let relative = entry.repo_path.trim_start_matches('/');
        validate_repo_path(relative)?;
        let staged_path = staging.path().join(format!("file-{index:08}"));
        let mut output = File::create(&staged_path)
            .with_context(|| format!("create staged upload for {}", entry.repo_path))?;
        let mut offset = 0u64;
        while offset < entry.size {
            let remaining = (entry.size - offset).min(IO_BLOCK as u64) as usize;
            let bytes = fs.read_inode(entry.inode, offset, remaining)?;
            if bytes.is_empty() {
                bail!(
                    "local file {} ended before its recorded size",
                    entry.repo_path
                );
            }
            output.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
        output.sync_all()?;
        operations.push(CommitOperation::add_file(relative, staged_path));
        bytes_uploaded = bytes_uploaded.saturating_add(entry.size);
    }

    let result = repo
        .create_commit()
        .operations(operations)
        .commit_message(commit_message.to_owned())
        .revision(revision.to_owned())
        .send()
        .with_context(|| format!("upload XetFS files to {repo_id}@{revision}"))?;

    Ok(PushReport {
        repository: repo_id.to_owned(),
        revision: revision.to_owned(),
        files_uploaded: local_files.len() as u64,
        symlinks_skipped: skipped_symlinks,
        bytes_uploaded,
        commit_oid: result.commit_oid,
        commit_url: result.commit_url,
    })
}
