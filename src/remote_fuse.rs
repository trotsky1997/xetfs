//! Read-only, on-demand FUSE view of Xet-backed files in a Hub revision.

use anyhow::Context;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};
use hf_hub::{HFClientSync, HFRepositorySync, RepoTypeAny, repository::RepoTreeEntry, split_id};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::{OsStr, OsString},
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

const ENTRY_TTL: Duration = Duration::from_secs(2);
const WINDOW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FUSE_READ_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct XetFile {
    path: String,
    size: u64,
    file_id: String,
}

#[derive(Clone)]
struct RemoteContent {
    size: u64,
    file_id: String,
}

struct RemoteNode {
    path: String,
    name: OsString,
    parent: u64,
    kind: FileType,
    attr: FileAttr,
    children: HashMap<OsString, u64>,
    ordered_children: Vec<(u64, FileType, OsString)>,
    content: Option<RemoteContent>,
}

enum NodeSpec {
    Directory,
    File(RemoteContent),
}

fn valid_repo_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn relative_path(path: &str, prefix: Option<&str>) -> Option<String> {
    match prefix {
        Some(prefix) => path
            .strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_prefix('/'))
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_owned),
        None => Some(path.to_owned()),
    }
}

fn load_xet_files(
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    prefix: Option<&str>,
) -> anyhow::Result<(Vec<XetFile>, usize)> {
    if let Some(prefix) = prefix {
        anyhow::ensure!(
            valid_repo_path(prefix),
            "prefix must be a safe repository path"
        );
    }
    let kind: RepoTypeAny = repo_type
        .parse()
        .context("invalid Hugging Face repo type")?;
    let (owner, name) = split_id(repo_id);
    anyhow::ensure!(!name.is_empty(), "invalid Hugging Face repository id");
    let hub = HFClientSync::new().context("initialize Hugging Face client and token")?;
    let repo: HFRepositorySync<RepoTypeAny> = hub.repository(kind, owner, name);
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
    let mut non_xet_files = 0;
    for entry in entries {
        if let RepoTreeEntry::File {
            path,
            size,
            xet_hash,
            ..
        } = entry
        {
            anyhow::ensure!(valid_repo_path(&path), "Hub returned an unsafe file path");
            let Some(path) = relative_path(&path, prefix) else {
                continue;
            };
            match xet_hash {
                Some(file_id) => {
                    let hash = crate::hash::from_hex(&file_id)
                        .context("Hub returned an invalid Xet file id")?;
                    anyhow::ensure!(
                        crate::hash::to_hex(&hash) == file_id,
                        "Hub Xet file ids must be lowercase hex"
                    );
                    files.push(XetFile {
                        path,
                        size,
                        file_id,
                    });
                }
                None => non_xet_files += 1,
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    anyhow::ensure!(
        !files.is_empty(),
        "Hub revision contains no Xet-backed files"
    );
    Ok((files, non_xet_files))
}

fn node_attr(ino: u64, kind: FileType, size: u64, uid: u32, gid: u32, nlink: u32) -> FileAttr {
    let timestamp = UNIX_EPOCH;
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: timestamp,
        mtime: timestamp,
        ctime: timestamp,
        crtime: timestamp,
        kind,
        perm: match kind {
            FileType::Directory => 0o555,
            _ => 0o444,
        },
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn build_tree(files: Vec<XetFile>, uid: u32, gid: u32) -> anyhow::Result<Vec<RemoteNode>> {
    let mut specs = BTreeMap::new();
    specs.insert(String::new(), NodeSpec::Directory);
    for file in files {
        anyhow::ensure!(valid_repo_path(&file.path), "invalid Xet file path");
        let mut parent_path = file.path.as_str();
        while let Some((parent, _)) = parent_path.rsplit_once('/') {
            specs
                .entry(parent.to_owned())
                .or_insert(NodeSpec::Directory);
            parent_path = parent;
        }
        anyhow::ensure!(
            specs
                .insert(
                    file.path,
                    NodeSpec::File(RemoteContent {
                        size: file.size,
                        file_id: file.file_id,
                    }),
                )
                .is_none(),
            "duplicate Hub path"
        );
    }

    let mut ordered: Vec<_> = specs.into_iter().collect();
    ordered.sort_by(|a, b| {
        a.0.split('/')
            .count()
            .cmp(&b.0.split('/').count())
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut nodes = Vec::with_capacity(ordered.len());
    let mut by_path = HashMap::with_capacity(ordered.len());
    for (path, spec) in ordered {
        let ino = nodes.len() as u64 + 1;
        let (kind, size, content) = match spec {
            NodeSpec::Directory => (FileType::Directory, 4096, None),
            NodeSpec::File(content) => (FileType::RegularFile, content.size, Some(content)),
        };
        let name = path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("")
            .into();
        by_path.insert(path.clone(), ino);
        nodes.push(RemoteNode {
            path,
            name,
            parent: 1,
            kind,
            attr: node_attr(ino, kind, size, uid, gid, 1),
            children: HashMap::new(),
            ordered_children: Vec::new(),
            content,
        });
    }

    for index in 1..nodes.len() {
        let path = nodes[index].path.clone();
        let (parent_path, _) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let parent_ino = *by_path
            .get(parent_path)
            .context("remote path has no parent directory")?;
        let ino = index as u64 + 1;
        let name = nodes[index].name.clone();
        let kind = nodes[index].kind;
        let parent = &mut nodes[(parent_ino - 1) as usize];
        anyhow::ensure!(
            parent.kind == FileType::Directory,
            "remote parent is not a directory"
        );
        parent.children.insert(name.clone(), ino);
        parent.ordered_children.push((ino, kind, name));
        nodes[index].parent = parent_ino;
    }
    for node in &mut nodes {
        node.ordered_children.sort_by(|a, b| a.2.cmp(&b.2));
        if node.kind == FileType::Directory {
            let subdirectories = node
                .ordered_children
                .iter()
                .filter(|(_, kind, _)| *kind == FileType::Directory)
                .count() as u32;
            node.attr.nlink = 2 + subdirectories;
        }
    }
    Ok(nodes)
}

pub fn mount(
    repo_id: &str,
    repo_type: &str,
    revision: &str,
    prefix: Option<&str>,
    mountpoint: &Path,
    workers: usize,
    cache_bytes: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=64).contains(&workers),
        "workers must be between 1 and 64"
    );
    anyhow::ensure!(
        cache_bytes >= WINDOW_BYTES as usize,
        "remote mount cache must be at least {WINDOW_BYTES} bytes"
    );
    let mountpoint = std::fs::canonicalize(mountpoint)?;
    anyhow::ensure!(mountpoint.is_dir(), "mountpoint must be a directory");
    anyhow::ensure!(
        std::fs::read_dir(&mountpoint)?.next().is_none(),
        "mountpoint must be empty"
    );
    let (files, skipped) = load_xet_files(repo_id, repo_type, revision, prefix)?;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let nodes = build_tree(files, uid, gid)?;
    let total_bytes: u64 = nodes
        .iter()
        .filter_map(|node| node.content.as_ref())
        .map(|f| f.size)
        .sum();
    eprintln!(
        "XetFS remote mount: {} Xet files, {} bytes, {} non-Xet files omitted, revision {}",
        nodes.iter().filter(|node| node.content.is_some()).count(),
        total_bytes,
        skipped,
        revision
    );
    fuser::mount2(
        RemoteFuse {
            nodes,
            reader: crate::remote_cas::RemoteCasReader::new(repo_id, repo_type, revision)?,
            cache: HashMap::new(),
            lru: VecDeque::new(),
            cache_bytes: 0,
            cache_limit: cache_bytes,
            workers,
        },
        mountpoint,
        &[
            MountOption::FSName("xetfs-remote".into()),
            MountOption::Subtype("xetfs-remote".into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::RO,
        ],
    )?;
    Ok(())
}

struct RemoteFuse {
    nodes: Vec<RemoteNode>,
    reader: crate::remote_cas::RemoteCasReader,
    cache: HashMap<(u64, u64), Vec<u8>>,
    lru: VecDeque<(u64, u64)>,
    cache_bytes: usize,
    cache_limit: usize,
    workers: usize,
}

impl RemoteFuse {
    fn node(&self, ino: u64) -> Option<&RemoteNode> {
        ino.checked_sub(1)
            .and_then(|index| self.nodes.get(index as usize))
    }

    fn touch(&mut self, key: (u64, u64)) {
        if let Some(index) = self.lru.iter().position(|candidate| *candidate == key) {
            self.lru.remove(index);
        }
        self.lru.push_back(key);
    }

    fn cache_window(&mut self, ino: u64, start: u64) -> anyhow::Result<()> {
        let key = (ino, start);
        if self.cache.contains_key(&key) {
            self.touch(key);
            return Ok(());
        }
        let content = self
            .node(ino)
            .and_then(|node| node.content.clone())
            .context("remote inode is not a Xet file")?;
        let length = WINDOW_BYTES.min(content.size.saturating_sub(start));
        let bytes = self.reader.read_file_range_with_workers(
            &content.file_id,
            content.size,
            start,
            length,
            self.workers,
        )?;
        anyhow::ensure!(
            bytes.len() as u64 == length,
            "remote CAS returned a short read"
        );
        loop {
            if let Some(total) = self.cache_bytes.checked_add(bytes.len())
                && total <= self.cache_limit
            {
                self.cache_bytes = total;
                break;
            }
            let Some(oldest) = self.lru.pop_front() else {
                anyhow::bail!("remote window exceeds the configured cache limit");
            };
            if let Some(removed) = self.cache.remove(&oldest) {
                self.cache_bytes = self.cache_bytes.saturating_sub(removed.len());
            }
        }
        self.cache.insert(key, bytes);
        self.touch(key);
        Ok(())
    }

    fn read_cached(&mut self, ino: u64, offset: u64, size: usize) -> anyhow::Result<Vec<u8>> {
        let node = self.node(ino).context("invalid remote inode")?;
        let content = node
            .content
            .as_ref()
            .context("remote inode is not a regular file")?;
        if offset >= content.size || size == 0 {
            return Ok(Vec::new());
        }
        let end = offset
            .saturating_add(size.min(MAX_FUSE_READ_BYTES) as u64)
            .min(content.size);
        let mut output = Vec::with_capacity((end - offset) as usize);
        let mut position = offset;
        while position < end {
            let window_start = position / WINDOW_BYTES * WINDOW_BYTES;
            let key = (ino, window_start);
            self.cache_window(ino, window_start)?;
            let window = self
                .cache
                .get(&key)
                .context("remote cache lost a fetched window")?;
            let within = (position - window_start) as usize;
            let take = ((end - position) as usize).min(window.len() - within);
            output.extend_from_slice(&window[within..within + take]);
            position += take as u64;
        }
        Ok(output)
    }
}

impl Filesystem for RemoteFuse {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let ino = self
            .node(parent)
            .filter(|node| node.kind == FileType::Directory)
            .and_then(|node| {
                if name == OsStr::new(".") {
                    Some(parent)
                } else if name == OsStr::new("..") {
                    Some(node.parent)
                } else {
                    node.children.get(name).copied()
                }
            });
        let result = ino.and_then(|ino| self.node(ino));
        match result {
            Some(node) => reply.entry(&ENTRY_TTL, &node.attr, 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.node(ino) {
            Some(node) => reply.attr(&ENTRY_TTL, &node.attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.node(ino) {
            Some(node) if node.kind == FileType::RegularFile => {
                if flags & libc::O_ACCMODE != libc::O_RDONLY {
                    reply.error(libc::EROFS);
                } else {
                    reply.opened(ino, 0);
                }
            }
            Some(_) => reply.error(libc::EISDIR),
            None => reply.error(libc::ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 || fh != ino {
            reply.error(libc::EINVAL);
            return;
        }
        match self.read_cached(ino, offset as u64, size as usize) {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => {
                let path = self
                    .node(ino)
                    .map_or("<unknown>", |node| node.path.as_str());
                eprintln!("remote Xet read failed for {path}: {error:#}");
                reply.error(libc::EIO);
            }
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.node(ino) {
            Some(node) if node.kind == FileType::Directory => reply.opened(ino, 0),
            Some(_) => reply.error(libc::ENOTDIR),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 || ino != fh {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(node) = self.node(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if node.kind != FileType::Directory {
            reply.error(libc::ENOTDIR);
            return;
        }
        let mut entries = vec![
            (ino, FileType::Directory, OsString::from(".")),
            (node.parent, FileType::Directory, OsString::from("..")),
        ];
        entries.extend(node.ordered_children.iter().cloned());
        for (index, (entry_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*entry_ino, (index + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let files = self.nodes.len() as u64;
        let bytes: u64 = self
            .nodes
            .iter()
            .filter_map(|node| node.content.as_ref())
            .map(|file| file.size)
            .fold(0u64, u64::saturating_add);
        reply.statfs(bytes.div_ceil(4096), 0, 0, files, 0, 4096, 255, 4096);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_tree_builds_implicit_directories_and_stable_file_inodes() {
        let file_id = "ab".repeat(32);
        let nodes = build_tree(
            vec![XetFile {
                path: "models/sub/weights.bin".into(),
                size: 123,
                file_id: file_id.clone(),
            }],
            1000,
            1000,
        )
        .unwrap();
        assert_eq!(nodes[0].kind, FileType::Directory);
        assert_eq!(nodes[0].ordered_children[0].2, "models");
        let file = nodes
            .iter()
            .find(|node| node.path == "models/sub/weights.bin")
            .unwrap();
        assert_eq!(file.attr.size, 123);
        assert_eq!(file.attr.perm, 0o444);
        assert_eq!(file.content.as_ref().unwrap().file_id, file_id);
    }

    #[test]
    fn remote_tree_rejects_unsafe_paths_and_collisions() {
        assert!(
            build_tree(
                vec![XetFile {
                    path: "../escape".into(),
                    size: 1,
                    file_id: "ab".repeat(32),
                }],
                1,
                1,
            )
            .is_err()
        );
        assert!(
            build_tree(
                vec![
                    XetFile {
                        path: "model".into(),
                        size: 1,
                        file_id: "ab".repeat(32),
                    },
                    XetFile {
                        path: "model/child".into(),
                        size: 1,
                        file_id: "cd".repeat(32),
                    },
                ],
                1,
                1,
            )
            .is_err()
        );
    }
}
