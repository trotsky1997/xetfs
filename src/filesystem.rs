use crate::{
    DirEntry, Error, GcReport, Kind, Node, Repository, Result,
    error::errno,
    repository::{UploadSnapshot, now_ns},
};
use rusqlite::{OptionalExtension, params};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const NODE_SELECT: &str = "SELECT i.*,CASE WHEN i.kind=2 THEN CASE WHEN i.ino=1 OR EXISTS(SELECT 1 FROM entries WHERE ino=i.ino) THEN 2+(SELECT COUNT(*) FROM entries e JOIN inodes d ON d.ino=e.ino WHERE e.parent=i.ino AND d.kind=2) ELSE 0 END ELSE (SELECT COUNT(*) FROM entries WHERE ino=i.ino) END FROM inodes i WHERE i.ino=?1";

#[derive(Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime_ns: Option<i64>,
    pub mtime_ns: Option<i64>,
    pub birthtime_ns: Option<i64>,
    pub flags: Option<u32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XattrPolicy {
    Set,
    Create,
    Replace,
    Remove,
}

#[cfg(target_os = "macos")]
const NO_XATTR: i32 = libc::ENOATTR;
#[cfg(not(target_os = "macos"))]
const NO_XATTR: i32 = libc::ENODATA;

fn xattr_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.contains(&0) {
        return errno(libc::EINVAL);
    }
    if name.len() > 127 {
        return errno(libc::ENAMETOOLONG);
    }
    Ok(())
}
struct Handle {
    ino: u64,
    flags: i32,
}
struct Stage {
    file: tempfile::NamedTempFile,
    dirty: bool,
    mtime: i64,
}

/// Namespace and shared per-inode write staging. FUSE serializes callbacks.
pub struct FileSystem {
    repo: Repository,
    handles: HashMap<u64, Handle>,
    stages: HashMap<u64, Stage>,
    pins: HashMap<u64, u64>,
    next_handle: u64,
}
impl FileSystem {
    pub fn new(repo: Repository) -> Self {
        Self {
            repo,
            handles: HashMap::new(),
            stages: HashMap::new(),
            pins: HashMap::new(),
            next_handle: 1,
        }
    }
    pub fn preload_small_files(&mut self, files: &[Vec<u8>]) -> Result<()> {
        self.repo.preload_small_files(files)
    }
    pub fn repository(&self) -> &Repository {
        &self.repo
    }
    pub fn get_xattr(&self, ino: u64, name: &[u8]) -> Result<Vec<u8>> {
        xattr_name(name)?;
        self.node(ino)?;
        self.repo
            .conn
            .query_row(
                "SELECT value FROM inode_xattrs WHERE ino=?1 AND name=?2",
                params![ino, name],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(Error::Errno(NO_XATTR))
    }
    pub fn list_xattrs(&self, ino: u64) -> Result<Vec<Vec<u8>>> {
        self.node(ino)?;
        Ok(self
            .repo
            .conn
            .prepare("SELECT name FROM inode_xattrs WHERE ino=?1 ORDER BY name")?
            .query_map([ino], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn set_xattr(
        &mut self,
        ino: u64,
        name: &[u8],
        value: Option<&[u8]>,
        policy: XattrPolicy,
    ) -> Result<()> {
        xattr_name(name)?;
        if (policy == XattrPolicy::Remove) != value.is_none() {
            return errno(libc::EINVAL);
        }
        self.node(ino)?;
        let tx = self.repo.conn.transaction()?;
        let exists = tx
            .query_row(
                "SELECT 1 FROM inode_xattrs WHERE ino=?1 AND name=?2",
                params![ino, name],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists && policy == XattrPolicy::Create {
            return errno(libc::EEXIST);
        }
        if !exists && matches!(policy, XattrPolicy::Replace | XattrPolicy::Remove) {
            return errno(NO_XATTR);
        }
        if let Some(value) = value {
            tx.execute("INSERT INTO inode_xattrs(ino,name,value) VALUES(?1,?2,?3) ON CONFLICT(ino,name) DO UPDATE SET value=excluded.value", params![ino, name, value])?;
        } else {
            tx.execute(
                "DELETE FROM inode_xattrs WHERE ino=?1 AND name=?2",
                params![ino, name],
            )?;
        }
        tx.execute(
            "UPDATE inodes SET ctime=?1 WHERE ino=?2",
            params![now_ns(), ino],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Pin an already verified recipe without reading or rechunking its bytes.
    pub fn pin_content(&mut self, path: impl AsRef<Path>, id: &str) -> Result<Node> {
        let size: u64 =
            self.repo
                .conn
                .query_row("SELECT size FROM contents WHERE id=?1", [id], |r| r.get(0))?;
        if let Ok(existing) = self.resolve_nofollow(&path) {
            if existing.kind != Kind::File || existing.content.as_deref() != Some(id) {
                return Err(Error::Invalid(
                    "immutable CAS path already has different content".into(),
                ));
            }
            return Ok(existing);
        }
        let (parent, name) = self.path_parent(path)?;
        self.create_node(
            parent,
            &name,
            Kind::File,
            0o400,
            crate::platform::owner().0,
            crate::platform::owner().1,
            Some(id),
            size,
            None,
            None,
            None,
            0,
            None,
        )
    }
    pub(crate) fn cas_repository_mut(&mut self) -> &mut Repository {
        &mut self.repo
    }
    pub fn retain_inode(&mut self, ino: u64, count: u64) {
        *self.pins.entry(ino).or_default() += count;
    }
    pub fn forget_inode(&mut self, ino: u64, count: u64) -> Result<()> {
        if let Some(n) = self.pins.get_mut(&ino) {
            *n = n.saturating_sub(count);
            if *n == 0 {
                self.pins.remove(&ino);
            }
        }
        self.purge_unlinked(ino)
    }
    pub fn node(&self, ino: u64) -> Result<Node> {
        let mut node = self
            .repo
            .conn
            .query_row(NODE_SELECT, [ino], Node::row)
            .optional()?
            .ok_or(Error::Errno(libc::ENOENT))?;
        if let Some(stage) = self.stages.get(&ino) {
            node.size = stage.file.as_file().metadata()?.len();
            node.mtime_ns = stage.mtime;
        }
        Ok(node)
    }
    fn directory(&self, ino: u64) -> Result<Node> {
        let node = self.node(ino)?;
        if node.kind != Kind::Directory {
            return errno(libc::ENOTDIR);
        }
        Ok(node)
    }
    fn name(name: &[u8]) -> Result<()> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return errno(libc::EINVAL);
        }
        if name.len() > 255 {
            return errno(libc::ENAMETOOLONG);
        }
        Ok(())
    }
    pub fn parent(&self, ino: u64) -> Result<u64> {
        if ino == 1 {
            return Ok(1);
        }
        Ok(self
            .repo
            .conn
            .query_row(
                "SELECT parent FROM entries WHERE ino=?1 LIMIT 1",
                [ino],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(1))
    }
    pub fn lookup(&self, parent: u64, name: &OsStr) -> Result<Node> {
        self.directory(parent)?;
        let name = name.as_encoded_bytes();
        if name == b"." {
            return self.node(parent);
        }
        if name == b".." {
            return self.node(self.parent(parent)?);
        }
        Self::name(name)?;
        let ino = self
            .repo
            .conn
            .query_row(
                "SELECT ino FROM entries WHERE parent=?1 AND name=?2",
                params![parent, name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::Errno(libc::ENOENT))?;
        self.node(ino)
    }
    pub fn list(&self, ino: u64) -> Result<Vec<DirEntry>> {
        self.directory(ino)?;
        let entries: Vec<(Vec<u8>, u64)> = self
            .repo
            .conn
            .prepare("SELECT name,ino FROM entries WHERE parent=?1 ORDER BY name")?
            .query_map([ino], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        entries
            .into_iter()
            .map(|(name, ino)| {
                Ok(DirEntry {
                    name,
                    node: self.node(ino)?,
                })
            })
            .collect()
    }
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<Node> {
        self.resolve_bytes(path.as_ref().as_os_str().as_encoded_bytes(), true, None)
    }
    pub fn resolve_nofollow(&self, path: impl AsRef<Path>) -> Result<Node> {
        self.resolve_bytes(path.as_ref().as_os_str().as_encoded_bytes(), false, None)
    }
    fn view_node(&self, ino: u64, snapshot: Option<&str>) -> Result<Node> {
        match snapshot {
            None => self.node(ino),
            Some(s) => Ok(self.repo.conn.query_row("SELECT ino,kind,mode,uid,gid,size,atime,mtime,ctime,birthtime,flags,content,target,1 FROM snapshot_nodes WHERE snapshot=?1 AND ino=?2",params![s,ino],Node::row).optional()?.ok_or(Error::Errno(libc::ENOENT))?),
        }
    }
    fn resolve_bytes(
        &self,
        path: &[u8],
        follow_final: bool,
        snapshot: Option<&str>,
    ) -> Result<Node> {
        if path.is_empty() || path.contains(&0) {
            return errno(libc::ENOENT);
        }
        let mut queue: VecDeque<Vec<u8>> = path
            .split(|b| *b == b'/')
            .filter(|p| !p.is_empty())
            .map(Vec::from)
            .collect();
        let mut ino = 1;
        let mut links = 0;
        while let Some(name) = queue.pop_front() {
            let parent_node = self.view_node(ino, snapshot)?;
            if parent_node.kind != Kind::Directory {
                return errno(libc::ENOTDIR);
            }
            if name == b"." {
                continue;
            }
            if name == b".." {
                ino = match snapshot {
                    None => self.parent(ino)?,
                    Some(s) => self.repo.conn.query_row("SELECT parent FROM snapshot_entries WHERE snapshot=?1 AND ino=?2 LIMIT 1",params![s,ino],|r| r.get(0)).optional()?.unwrap_or(1),
                };
                continue;
            }
            let child: u64 = match snapshot {
                None => self.repo.conn.query_row("SELECT ino FROM entries WHERE parent=?1 AND name=?2",params![ino,name],|r| r.get(0)).optional()?,
                Some(s) => self.repo.conn.query_row("SELECT ino FROM snapshot_entries WHERE snapshot=?1 AND parent=?2 AND name=?3",params![s,ino,name],|r| r.get(0)).optional()?,
            }.ok_or(Error::Errno(libc::ENOENT))?;
            let node = self.view_node(child, snapshot)?;
            if node.kind == Kind::Symlink && (follow_final || !queue.is_empty()) {
                links += 1;
                if links > 40 {
                    return errno(libc::ELOOP);
                }
                let target = node
                    .target
                    .ok_or_else(|| Error::Corrupt("symlink has no target".into()))?;
                if target.starts_with(b"/") {
                    ino = 1;
                }
                let mut parts: VecDeque<Vec<u8>> = target
                    .split(|b| *b == b'/')
                    .filter(|p| !p.is_empty())
                    .map(Vec::from)
                    .collect();
                parts.append(&mut queue);
                queue = parts;
            } else {
                ino = child;
            }
        }
        let node = self.view_node(ino, snapshot)?;
        if path.ends_with(b"/") && node.kind != Kind::Directory {
            return errno(libc::ENOTDIR);
        }
        Ok(node)
    }
    pub fn path_parent(&self, path: impl AsRef<Path>) -> Result<(u64, Vec<u8>)> {
        let bytes = path.as_ref().as_os_str().as_encoded_bytes();
        let end = bytes
            .iter()
            .rposition(|b| *b != b'/')
            .map(|i| i + 1)
            .ok_or(Error::Errno(libc::EBUSY))?;
        let bytes = &bytes[..end];
        let split = bytes.iter().rposition(|b| *b == b'/');
        let (parent, name) = match split {
            Some(i) => (&bytes[..=i], &bytes[i + 1..]),
            None => (b"/".as_slice(), bytes),
        };
        Self::name(name)?;
        let parent = self.resolve_bytes(parent, true, None)?;
        if parent.kind != Kind::Directory {
            return errno(libc::ENOTDIR);
        }
        Ok((parent.ino, name.to_vec()))
    }
    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &mut self,
        parent: u64,
        name: &[u8],
        kind: Kind,
        mode: u32,
        uid: u32,
        gid: u32,
        content: Option<&str>,
        size: u64,
        target: Option<&[u8]>,
        copy_xattrs: Option<u64>,
        birthtime: Option<i64>,
        flags: u32,
        initial_xattr: Option<(&[u8], &[u8])>,
    ) -> Result<Node> {
        let dir = self.directory(parent)?;
        Self::name(name)?;
        if self
            .lookup(parent, crate::platform::name_from_bytes(name)?)
            .is_ok()
        {
            return errno(libc::EEXIST);
        }
        let gid = if dir.mode & 0o2000 != 0 { dir.gid } else { gid };
        let mode = mode & 0o7777
            | if kind == Kind::Directory {
                dir.mode & 0o2000
            } else {
                0
            };
        let tx = self.repo.conn.transaction()?;
        let now = now_ns();
        tx.execute("INSERT INTO inodes(kind,mode,uid,gid,size,atime,mtime,ctime,birthtime,content,target,flags) VALUES (?1,?2,?3,?4,?5,?6,?6,?6,?7,?8,?9,?10)",params![kind as i64,mode,uid,gid,size,now,birthtime.unwrap_or(now),content,target,flags])?;
        let ino = tx.last_insert_rowid() as u64;
        tx.execute(
            "INSERT INTO entries VALUES (?1,?2,?3)",
            params![parent, name, ino],
        )?;
        if let Some((name, value)) = initial_xattr {
            tx.execute(
                "INSERT INTO inode_xattrs(ino,name,value) VALUES(?1,?2,?3)",
                params![ino, name, value],
            )?;
        }
        if let Some(source) = copy_xattrs {
            tx.execute(
                "INSERT INTO inode_xattrs SELECT ?1,name,value FROM inode_xattrs WHERE ino=?2",
                params![ino, source],
            )?;
        }
        tx.execute(
            "UPDATE inodes SET mtime=?1,ctime=?1 WHERE ino=?2",
            params![now, parent],
        )?;
        tx.commit()?;
        self.node(ino)
    }
    pub fn create(
        &mut self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Node> {
        let (empty, _) = self.repo.ingest(std::io::empty())?;
        self.create_node(
            parent,
            name.as_encoded_bytes(),
            Kind::File,
            mode,
            uid,
            gid,
            Some(&empty),
            0,
            None,
            None,
            None,
            0,
            None,
        )
    }
    #[cfg(windows)]
    pub(crate) fn create_windows(
        &mut self,
        parent: u64,
        name: &OsStr,
        directory: bool,
        security: &[u8],
    ) -> Result<Node> {
        let content = if directory {
            None
        } else {
            Some(self.repo.ingest(std::io::empty())?.0)
        };
        self.create_node(
            parent,
            name.as_encoded_bytes(),
            if directory {
                Kind::Directory
            } else {
                Kind::File
            },
            if directory { 0o755 } else { 0o644 },
            0,
            0,
            content.as_deref(),
            0,
            None,
            None,
            None,
            0,
            Some((b"user.xetfs.windows.security", security)),
        )
    }

    #[cfg(windows)]
    pub(crate) fn set_windows_symlink(
        &mut self,
        ino: u64,
        target: &[u8],
        raw: &[u8],
    ) -> Result<()> {
        let node = self.node(ino)?;
        if target.is_empty()
            || target.len() > 4096
            || target.contains(&0)
            || node.size != 0
            || node.kind == Kind::Symlink
        {
            return errno(libc::EINVAL);
        }
        if node.kind == Kind::Directory && !self.list(ino)?.is_empty() {
            return errno(libc::ENOTEMPTY);
        }
        let mut value = vec![u8::from(node.kind == Kind::Directory)];
        value.extend_from_slice(raw);
        let tx = self.repo.conn.transaction()?;
        tx.execute(
            "UPDATE inodes SET kind=3,content=NULL,target=?1,size=?2,ctime=?3 WHERE ino=?4",
            params![target, target.len(), now_ns(), ino],
        )?;
        tx.execute(
            "INSERT INTO inode_xattrs(ino,name,value) VALUES(?1,?2,?3)",
            params![ino, b"user.xetfs.windows.reparse".as_slice(), value],
        )?;
        tx.commit()?;
        Ok(())
    }
    #[cfg(windows)]
    pub(crate) fn delete_windows_symlink(&mut self, ino: u64) -> Result<()> {
        let node = self.node(ino)?;
        if node.kind != Kind::Symlink {
            return errno(libc::EINVAL);
        }
        let value = self.get_xattr(ino, b"user.xetfs.windows.reparse")?;
        let directory = value.first() == Some(&1);
        let content = if directory {
            None
        } else {
            Some(self.repo.ingest(std::io::empty())?.0)
        };
        let tx = self.repo.conn.transaction()?;
        tx.execute(
            "UPDATE inodes SET kind=?1,content=?2,target=NULL,size=0,ctime=?3 WHERE ino=?4",
            params![if directory { 2 } else { 1 }, content, now_ns(), ino],
        )?;
        tx.execute(
            "DELETE FROM inode_xattrs WHERE ino=?1 AND name=?2",
            params![ino, b"user.xetfs.windows.reparse".as_slice()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mkdir(
        &mut self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Node> {
        self.create_node(
            parent,
            name.as_encoded_bytes(),
            Kind::Directory,
            mode,
            uid,
            gid,
            None,
            0,
            None,
            None,
            None,
            0,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_birthtime(
        &mut self,
        parent: u64,
        name: &OsStr,
        kind: Kind,
        mode: u32,
        uid: u32,
        gid: u32,
        target: Option<&[u8]>,
        birthtime: Option<i64>,
        flags: u32,
    ) -> Result<Node> {
        if flags & !0x8000 != 0 {
            return errno(libc::EOPNOTSUPP);
        }
        let empty = if kind == Kind::File {
            Some(self.repo.ingest(std::io::empty())?.0)
        } else {
            None
        };
        let size = if kind == Kind::Symlink {
            let target = target.ok_or(Error::Errno(libc::EINVAL))?;
            if target.is_empty() || target.contains(&0) {
                return errno(libc::EINVAL);
            }
            if target.len() > 4096 {
                return errno(libc::ENAMETOOLONG);
            }
            target.len() as u64
        } else {
            if target.is_some() {
                return errno(libc::EINVAL);
            }
            0
        };
        self.create_node(
            parent,
            name.as_encoded_bytes(),
            kind,
            mode,
            uid,
            gid,
            empty.as_deref(),
            size,
            target,
            None,
            birthtime,
            flags,
            None,
        )
    }
    pub fn mkdir_all(&mut self, path: impl AsRef<Path>) -> Result<Node> {
        let mut ino = 1;
        for name in path
            .as_ref()
            .as_os_str()
            .as_encoded_bytes()
            .split(|b| *b == b'/')
            .filter(|n| !n.is_empty())
        {
            let name = crate::platform::name_from_bytes(name)?;
            let node = match self.lookup(ino, name) {
                Ok(node) => node,
                Err(Error::Errno(libc::ENOENT)) => self.mkdir(
                    ino,
                    name,
                    0o755,
                    crate::platform::owner().0,
                    crate::platform::owner().1,
                )?,
                Err(e) => return Err(e),
            };
            if node.kind != Kind::Directory {
                return errno(libc::ENOTDIR);
            }
            ino = node.ino;
        }
        self.node(ino)
    }
    pub fn symlink(
        &mut self,
        parent: u64,
        name: &OsStr,
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Node> {
        if target.is_empty() || target.contains(&0) {
            return errno(libc::EINVAL);
        }
        if target.len() > 4096 {
            return errno(libc::ENAMETOOLONG);
        }
        self.create_node(
            parent,
            name.as_encoded_bytes(),
            Kind::Symlink,
            0o777,
            uid,
            gid,
            None,
            target.len() as u64,
            Some(target),
            None,
            None,
            0,
            None,
        )
    }
    pub fn link(&mut self, ino: u64, parent: u64, name: &OsStr) -> Result<Node> {
        let node = self.node(ino)?;
        if node.kind == Kind::Directory {
            return errno(libc::EPERM);
        }
        self.directory(parent)?;
        Self::name(name.as_encoded_bytes())?;
        if self.lookup(parent, name).is_ok() {
            return errno(libc::EEXIST);
        }
        let tx = self.repo.conn.transaction()?;
        let now = now_ns();
        tx.execute(
            "INSERT INTO entries VALUES (?1,?2,?3)",
            params![parent, name.as_encoded_bytes(), ino],
        )?;
        tx.execute("UPDATE inodes SET ctime=?1 WHERE ino=?2", params![now, ino])?;
        tx.execute(
            "UPDATE inodes SET mtime=?1,ctime=?1 WHERE ino=?2",
            params![now, parent],
        )?;
        tx.commit()?;
        self.node(ino)
    }
    fn purge_unlinked(&mut self, ino: u64) -> Result<()> {
        if ino == 1 || self.pins.contains_key(&ino) || self.handles.values().any(|h| h.ino == ino) {
            return Ok(());
        }
        self.repo.conn.execute("DELETE FROM inodes WHERE ino=?1 AND NOT EXISTS(SELECT 1 FROM entries WHERE entries.ino=inodes.ino)",[ino])?;
        self.stages.remove(&ino);
        Ok(())
    }
    pub fn unlink(&mut self, parent: u64, name: &OsStr, directory: bool) -> Result<()> {
        let node = self.lookup(parent, name)?;
        if directory {
            if node.kind != Kind::Directory {
                return errno(libc::ENOTDIR);
            }
            if !self.list(node.ino)?.is_empty() {
                return errno(libc::ENOTEMPTY);
            }
        } else if node.kind == Kind::Directory {
            return errno(libc::EISDIR);
        }
        Self::name(name.as_encoded_bytes())?;
        let tx = self.repo.conn.transaction()?;
        let now = now_ns();
        tx.execute(
            "DELETE FROM entries WHERE parent=?1 AND name=?2",
            params![parent, name.as_encoded_bytes()],
        )?;
        tx.execute(
            "UPDATE inodes SET ctime=?1 WHERE ino=?2",
            params![now, node.ino],
        )?;
        tx.execute(
            "UPDATE inodes SET mtime=?1,ctime=?1 WHERE ino=?2",
            params![now, parent],
        )?;
        tx.commit()?;
        self.purge_unlinked(node.ino)
    }
    pub fn rename(
        &mut self,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        no_replace: bool,
    ) -> Result<()> {
        Self::name(name.as_encoded_bytes())?;
        Self::name(new_name.as_encoded_bytes())?;
        let src = self.lookup(parent, name)?;
        self.directory(new_parent)?;
        let dest = match self.lookup(new_parent, new_name) {
            Ok(n) => Some(n),
            Err(Error::Errno(libc::ENOENT)) => None,
            Err(e) => return Err(e),
        };
        if let Some(ref dest) = dest {
            if no_replace {
                return errno(libc::EEXIST);
            }
            if src.ino == dest.ino {
                return Ok(());
            }
            if src.kind == Kind::Directory && dest.kind != Kind::Directory {
                return errno(libc::ENOTDIR);
            }
            if src.kind != Kind::Directory && dest.kind == Kind::Directory {
                return errno(libc::EISDIR);
            }
            if dest.kind == Kind::Directory && !self.list(dest.ino)?.is_empty() {
                return errno(libc::ENOTEMPTY);
            }
        }
        if src.kind == Kind::Directory {
            let mut cur = new_parent;
            loop {
                if cur == src.ino {
                    return errno(libc::EINVAL);
                }
                if cur == 1 {
                    break;
                }
                cur = self.parent(cur)?;
            }
        }
        let now = now_ns();
        let tx = self.repo.conn.transaction()?;
        tx.execute(
            "DELETE FROM entries WHERE parent=?1 AND name=?2",
            params![new_parent, new_name.as_encoded_bytes()],
        )?;
        tx.execute(
            "UPDATE entries SET parent=?1,name=?2 WHERE parent=?3 AND name=?4",
            params![
                new_parent,
                new_name.as_encoded_bytes(),
                parent,
                name.as_encoded_bytes()
            ],
        )?;
        tx.execute(
            "UPDATE inodes SET ctime=?1 WHERE ino=?2",
            params![now, src.ino],
        )?;
        tx.execute(
            "UPDATE inodes SET mtime=?1,ctime=?1 WHERE ino IN (?2,?3)",
            params![now, parent, new_parent],
        )?;
        tx.commit()?;
        if let Some(dest) = dest {
            self.purge_unlinked(dest.ino)?;
        }
        Ok(())
    }
    /// Import commits the new immutable content before switching the inode recipe.
    pub fn put_file(&mut self, path: impl AsRef<Path>, input: impl Read) -> Result<Node> {
        let existing = match self.resolve(&path) {
            Ok(n) => Some(n),
            Err(Error::Errno(libc::ENOENT)) => None,
            Err(e) => return Err(e),
        };
        let (parent, name) = self.path_parent(&path)?;
        if let Some(ref n) = existing {
            if n.kind != Kind::File {
                return errno(libc::EISDIR);
            }
            if self.handles.values().any(|h| h.ino == n.ino) {
                return Err(Error::Busy);
            }
        } else if self
            .lookup(parent, crate::platform::name_from_bytes(&name)?)
            .is_ok()
        {
            return errno(libc::EEXIST);
        }
        let (id, size) = self.repo.ingest(input)?;
        if let Some(n) = existing {
            self.repo.conn.execute(
                "UPDATE inodes SET content=?1,size=?2,mtime=?3,ctime=?3,mode=mode&?4 WHERE ino=?5",
                params![id, size, now_ns(), 0o1777, n.ino],
            )?;
            self.node(n.ino)
        } else {
            self.create_node(
                parent,
                &name,
                Kind::File,
                0o644,
                crate::platform::owner().0,
                crate::platform::owner().1,
                Some(&id),
                size,
                None,
                None,
                None,
                0,
                None,
            )
        }
    }
    pub fn copy_file(&mut self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<Node> {
        let node = self.resolve(from)?;
        if node.kind != Kind::File {
            return errno(libc::EISDIR);
        }
        self.sync_inode(node.ino)?;
        let node = self.node(node.ino)?;
        let (parent, name) = self.path_parent(to)?;
        self.create_node(
            parent,
            &name,
            Kind::File,
            node.mode,
            crate::platform::owner().0,
            crate::platform::owner().1,
            node.content.as_deref(),
            node.size,
            None,
            Some(node.ino),
            None,
            node.flags,
            None,
        )
    }
    fn ensure_stage(&mut self, ino: u64) -> Result<&mut Stage> {
        if !self.stages.contains_key(&ino) {
            let node = self.node(ino)?;
            if node.kind != Kind::File {
                return errno(libc::EISDIR);
            }
            let mut file = tempfile::NamedTempFile::new_in(self.repo.root.join("tmp"))?;
            self.repo.export_content(
                node.content
                    .as_deref()
                    .ok_or_else(|| Error::Corrupt("file has no recipe".into()))?,
                file.as_file_mut(),
            )?;
            self.stages.insert(
                ino,
                Stage {
                    file,
                    dirty: false,
                    mtime: node.mtime_ns,
                },
            );
        }
        Ok(self.stages.get_mut(&ino).unwrap())
    }
    pub fn open_handle(&mut self, ino: u64, flags: i32) -> Result<u64> {
        let node = self.node(ino)?;
        if node.kind != Kind::File {
            return errno(if node.kind == Kind::Directory {
                libc::EISDIR
            } else {
                libc::ELOOP
            });
        }
        if flags & libc::O_TRUNC != 0 {
            if flags & crate::platform::ACCESS_MODE == libc::O_RDONLY {
                return errno(libc::EACCES);
            }
            self.setattr(
                ino,
                SetAttr {
                    size: Some(0),
                    ..Default::default()
                },
            )?;
        }
        let fh = self.next_handle;
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .ok_or(Error::Errno(libc::EMFILE))?;
        self.handles.insert(fh, Handle { ino, flags });
        Ok(fh)
    }
    pub fn read_inode(&self, ino: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
        let node = self.node(ino)?;
        if node.kind != Kind::File {
            return errno(libc::EISDIR);
        }
        if offset >= node.size {
            return Ok(Vec::new());
        }
        let size = (node.size - offset).min(size as u64) as usize;
        if let Some(stage) = self.stages.get(&ino) {
            let mut data = vec![0; size];
            crate::platform::read_exact_at(stage.file.as_file(), &mut data, offset)?;
            Ok(data)
        } else {
            self.repo.read_content(
                node.content
                    .as_deref()
                    .ok_or_else(|| Error::Corrupt("file has no recipe".into()))?,
                offset,
                size,
            )
        }
    }

    pub(crate) fn upload_snapshot(&self, ino: u64) -> Result<UploadSnapshot> {
        let node = self.node(ino)?;
        if node.kind != Kind::File {
            return errno(libc::EISDIR);
        }
        if let Some(stage) = self.stages.get(&ino) {
            return Ok(UploadSnapshot::staged(
                stage.file.as_file().try_clone()?,
                node.size,
            ));
        }
        self.repo.upload_snapshot(
            node.content
                .as_deref()
                .ok_or_else(|| Error::Corrupt("file has no recipe".into()))?,
            node.size,
        )
    }

    /// Return the committed recipe only when no unflushed write stage shadows it.
    pub(crate) fn committed_upload_content(&self, ino: u64) -> Result<Option<String>> {
        if self.stages.contains_key(&ino) {
            return Ok(None);
        }
        Ok(self.node(ino)?.content)
    }
    pub fn read_handle(&self, fh: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
        let h = self.handles.get(&fh).ok_or(Error::Errno(libc::EBADF))?;
        if h.flags & crate::platform::ACCESS_MODE == libc::O_WRONLY {
            return errno(libc::EBADF);
        }
        self.read_inode(h.ino, offset, size)
    }
    pub fn write_handle(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let h = self.handles.get(&fh).ok_or(Error::Errno(libc::EBADF))?;
        if h.flags & crate::platform::ACCESS_MODE == libc::O_RDONLY {
            return errno(libc::EBADF);
        }
        let (ino, append, sync) = (
            h.ino,
            h.flags & libc::O_APPEND != 0,
            h.flags & crate::platform::SYNC_FLAGS != 0,
        );
        if data.is_empty() {
            return Ok(0);
        }
        let stage = self.ensure_stage(ino)?;
        let offset = if append {
            stage.file.as_file().metadata()?.len()
        } else {
            offset
        };
        if offset
            .checked_add(data.len() as u64)
            .is_none_or(|n| n > i64::MAX as u64)
        {
            return errno(libc::EFBIG);
        }
        let written = crate::platform::write_at(stage.file.as_file(), data, offset)?;
        if written == 0 {
            return Ok(0);
        }
        stage.dirty = true;
        stage.mtime = now_ns();
        self.repo.conn.execute(
            "UPDATE inodes SET mode=mode&?1 WHERE ino=?2",
            params![0o1777, ino],
        )?;
        if sync {
            self.sync_inode(ino)?;
        }
        Ok(written)
    }
    pub fn sync_inode(&mut self, ino: u64) -> Result<()> {
        let Some(stage) = self.stages.get_mut(&ino) else {
            return Ok(());
        };
        if !stage.dirty {
            return Ok(());
        }
        stage.file.as_file().sync_all()?;
        stage.file.as_file_mut().seek(SeekFrom::Start(0))?;
        let (id, size) = self.repo.ingest(stage.file.as_file_mut())?;
        self.repo.conn.execute(
            "UPDATE inodes SET content=?1,size=?2,mtime=?3,ctime=?4 WHERE ino=?5",
            params![id, size, stage.mtime, now_ns(), ino],
        )?;
        stage.dirty = false;
        Ok(())
    }
    pub fn flush_handle(&mut self, fh: u64) -> Result<()> {
        let ino = self.handles.get(&fh).ok_or(Error::Errno(libc::EBADF))?.ino;
        self.sync_inode(ino)
    }
    pub fn close_handle(&mut self, fh: u64) -> Result<()> {
        let ino = self.handles.get(&fh).ok_or(Error::Errno(libc::EBADF))?.ino;
        self.sync_inode(ino)?;
        self.handles.remove(&fh);
        if !self.handles.values().any(|h| h.ino == ino) {
            self.stages.remove(&ino);
            self.purge_unlinked(ino)?;
        }
        Ok(())
    }
    pub fn sync_all(&mut self) -> Result<()> {
        let inodes: Vec<u64> = self.stages.keys().copied().collect();
        for ino in inodes {
            self.sync_inode(ino)?;
        }
        Ok(())
    }
    pub fn setattr(&mut self, ino: u64, attrs: SetAttr) -> Result<Node> {
        if attrs.flags.is_some_and(|flags| flags & !0x8000 != 0) {
            return errno(libc::EOPNOTSUPP);
        }
        let node = self.node(ino)?;
        if let Some(size) = attrs.size {
            if node.kind != Kind::File {
                return errno(libc::EISDIR);
            }
            if size > i64::MAX as u64 {
                return errno(libc::EFBIG);
            }
            let stage = self.ensure_stage(ino)?;
            stage.file.as_file().set_len(size)?;
            stage.dirty = true;
            stage.mtime = attrs.mtime_ns.unwrap_or_else(now_ns);
            self.repo.conn.execute(
                "UPDATE inodes SET mode=mode&?1 WHERE ino=?2",
                params![0o1777, ino],
            )?;
            if !self.handles.values().any(|h| h.ino == ino) {
                self.sync_inode(ino)?;
                self.stages.remove(&ino);
            }
        }
        if let Some(mtime) = attrs.mtime_ns
            && let Some(stage) = self.stages.get_mut(&ino)
        {
            stage.mtime = mtime;
        }
        self.repo.conn.execute("UPDATE inodes SET mode=COALESCE(?1,mode),uid=COALESCE(?2,uid),gid=COALESCE(?3,gid),atime=COALESCE(?4,atime),mtime=COALESCE(?5,mtime),birthtime=COALESCE(?6,birthtime),flags=COALESCE(?7,flags),ctime=?8 WHERE ino=?9",params![attrs.mode.map(|m| m&0o7777),attrs.uid,attrs.gid,attrs.atime_ns,attrs.mtime_ns,attrs.birthtime_ns,attrs.flags,now_ns(),ino])?;
        self.node(ino)
    }
    pub fn snapshot(&mut self, name: &str) -> Result<()> {
        self.sync_all()?;
        self.repo.snapshot(name)
    }
    pub fn restore(&mut self, name: &str) -> Result<()> {
        if !self.handles.is_empty() || !self.pins.is_empty() {
            return Err(Error::Busy);
        }
        self.sync_all()?;
        self.stages.clear();
        self.repo.restore(name)
    }
    pub fn drop_snapshot(&mut self, name: &str) -> Result<()> {
        self.repo.drop_snapshot(name)
    }
    pub fn read_snapshot(
        &self,
        snapshot: &str,
        path: impl AsRef<Path>,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>> {
        let node = self.resolve_bytes(
            path.as_ref().as_os_str().as_encoded_bytes(),
            true,
            Some(snapshot),
        )?;
        if node.kind != Kind::File {
            return errno(libc::EISDIR);
        }
        self.repo.read_content(
            node.content
                .as_deref()
                .ok_or_else(|| Error::Corrupt("snapshot file has no content".into()))?,
            offset,
            size,
        )
    }
    pub fn gc(&mut self, compact: bool) -> Result<GcReport> {
        if !self.handles.is_empty() {
            return Err(Error::Busy);
        }
        self.sync_all()?;
        self.repo.gc(compact)
    }
}
