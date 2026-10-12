//! Hugging Face Hub transfers.
//!
//! Pulls and write-stage fallbacks use the official Hub/Xet Rust client.
//! Committed Xet files use XetFS's native CAS and metadata-shard writer.

#[path = "remote/push_native.rs"]
mod push_native;

use crate::{Error, FileSystem, Kind, Node, Result};
use anyhow::{Context, bail};
use futures_util::stream;
use hf_hub::{
    HFClientSync, HFError, HFRepositorySync, RepoTypeAny,
    repository::{AddSource, CommitOperation, SourceByteStream, StreamFactory, StreamSource},
    split_id,
};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Arc,
};

const IO_BLOCK: usize = 1024 * 1024;
const MAX_REMOTE_WORKERS: usize = 64;

#[derive(Debug, serde::Serialize)]
pub struct PullReport {
    pub repository: String,
    pub revision: String,
    pub workers: usize,
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
    workers: usize,
) -> anyhow::Result<PullReport> {
    validate_prefix(prefix)?;
    anyhow::ensure!(
        (1..=MAX_REMOTE_WORKERS).contains(&workers),
        "remote pull workers must be between 1 and {MAX_REMOTE_WORKERS}"
    );
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

    let mut staged_plan = Vec::new();
    let mut files_skipped = 0u64;
    for (remote_path, target, expected_size, skip) in planned {
        if skip {
            files_skipped += 1;
            continue;
        }
        staged_plan.push((remote_path, target, expected_size));
    }

    let mut staged = Vec::with_capacity(staged_plan.len());
    let staging = if staged_plan.is_empty() {
        None
    } else {
        Some(tempfile::tempdir().context("create temporary Hub download directory")?)
    };
    if let Some(staging) = &staging {
        let allow_patterns = staged_plan
            .iter()
            .map(|(remote_path, _, _)| globset::escape(remote_path))
            .collect();
        let snapshot_dir = repo
            .snapshot_download()
            .revision(revision.to_owned())
            .allow_patterns(allow_patterns)
            .local_dir(staging.path().to_path_buf())
            .force_download(true)
            .max_workers(workers)
            .send()
            .context("download selected files through Hugging Face Xet")?;
        for (remote_path, target, expected_size) in staged_plan {
            let downloaded = snapshot_dir.join(&remote_path);
            let metadata = fs::metadata(&downloaded)
                .with_context(|| format!("inspect downloaded file {remote_path}"))?;
            if !metadata.is_file() {
                bail!("downloaded path {remote_path} is not a regular file");
            }
            let actual_size = metadata.len();
            if actual_size != expected_size {
                bail!("downloaded {remote_path} has size {actual_size}, expected {expected_size}");
            }
            staged.push((remote_path, target, downloaded, actual_size));
        }
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
        workers,
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

struct XetFsStreamFactory {
    snapshot: Arc<crate::repository::UploadSnapshot>,
}

impl StreamFactory for XetFsStreamFactory {
    fn open(&self) -> SourceByteStream {
        let snapshot = self.snapshot.clone();
        Box::pin(stream::try_unfold(
            (snapshot, 0u64),
            |(snapshot, offset)| async move {
                if offset >= snapshot.size() {
                    return Ok(None);
                }
                let length = (snapshot.size() - offset).min(IO_BLOCK as u64) as usize;
                let bytes = snapshot
                    .read_at(offset, length)
                    .map_err(|error| HFError::Io(std::io::Error::other(error)))?;
                if bytes.len() != length {
                    return Err(HFError::Io(std::io::Error::other(
                        "local XetFS stream ended before its recorded size",
                    )));
                }
                let next = offset + bytes.len() as u64;
                Ok(Some((bytes::Bytes::from(bytes), (snapshot, next))))
            },
        ))
    }
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

/// Upload regular files as one Hub commit. Committed Xet content is uploaded
/// through the XetFS native CAS path; open write stages use the official client.
pub fn push(
    fs: &FileSystem,
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    prefix: Option<&str>,
    commit_message: &str,
) -> anyhow::Result<PushReport> {
    push_native::push(fs, repo_id, repo_type, revision, prefix, commit_message)
}

fn push_official(
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

    let mut operations = Vec::with_capacity(local_files.len());
    let mut bytes_uploaded = 0u64;
    for entry in &local_files {
        let relative = entry.repo_path.trim_start_matches('/');
        validate_repo_path(relative)?;
        let snapshot = Arc::new(fs.upload_snapshot(entry.inode)?);
        if snapshot.size() != entry.size {
            bail!(
                "local file {} changed while preparing upload",
                entry.repo_path
            );
        }
        let source = StreamSource::new(Arc::new(XetFsStreamFactory { snapshot }), entry.size);
        operations.push(CommitOperation::Add {
            path_in_repo: relative.to_owned(),
            source: AddSource::stream(source),
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Repository;
    use std::{
        io::Cursor,
        task::{Context, Poll, Waker},
    };

    fn collect_ready_stream(mut source: SourceByteStream) -> hf_hub::HFResult<Vec<u8>> {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut output = Vec::new();
        loop {
            match source.as_mut().poll_next(&mut context) {
                Poll::Ready(Some(Ok(bytes))) => output.extend_from_slice(&bytes),
                Poll::Ready(Some(Err(error))) => return Err(error),
                Poll::Ready(None) => return Ok(output),
                Poll::Pending => panic!("local XetFS stream unexpectedly blocked"),
            }
        }
    }

    #[test]
    fn upload_stream_reopens_committed_and_staged_xetfs_files() {
        let temp = tempfile::tempdir().unwrap();
        let repository = Repository::init(temp.path()).unwrap();
        let mut filesystem = FileSystem::new(repository);
        let original: Vec<u8> = (0..350_000)
            .map(|i| ((i * 73 + i / 11) % 251) as u8)
            .collect();
        let committed = filesystem
            .put_file("/committed.bin", Cursor::new(&original))
            .unwrap();
        let committed_factory = XetFsStreamFactory {
            snapshot: Arc::new(filesystem.upload_snapshot(committed.ino).unwrap()),
        };
        for _ in 0..3 {
            assert_eq!(
                collect_ready_stream(committed_factory.open()).unwrap(),
                original
            );
        }

        let staged = filesystem
            .put_file("/staged.bin", Cursor::new(&original))
            .unwrap();
        let handle = filesystem.open_handle(staged.ino, libc::O_RDWR).unwrap();
        filesystem
            .write_handle(handle, 123_456, b"XetFS streaming upload")
            .unwrap();
        let mut expected = original;
        expected[123_456..123_456 + b"XetFS streaming upload".len()]
            .copy_from_slice(b"XetFS streaming upload");
        let staged_factory = XetFsStreamFactory {
            snapshot: Arc::new(filesystem.upload_snapshot(staged.ino).unwrap()),
        };
        for _ in 0..3 {
            assert_eq!(
                collect_ready_stream(staged_factory.open()).unwrap(),
                expected
            );
        }
    }
}
