use crate::{Error, FileSystem, Kind, Node, Result, SetAttr};
use fuser::*;
use std::{
    collections::HashMap,
    ffi::{CString, OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TTL: Duration = Duration::ZERO;
const DIRECT_IO: u32 = 1;
fn time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}
fn ns(value: TimeOrNow) -> i64 {
    let t = match value {
        TimeOrNow::Now => SystemTime::now(),
        TimeOrNow::SpecificTime(t) => t,
    };
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos().min(i64::MAX as u128) as i64,
        Err(e) => -(e.duration().as_nanos().min(i64::MAX as u128) as i64),
    }
}
fn kind(k: Kind) -> FileType {
    match k {
        Kind::File => FileType::RegularFile,
        Kind::Directory => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
    }
}
fn attr(n: &Node) -> FileAttr {
    FileAttr {
        ino: n.ino,
        size: n.size,
        blocks: n.size.div_ceil(512),
        atime: time(n.atime_ns),
        mtime: time(n.mtime_ns),
        ctime: time(n.ctime_ns),
        crtime: time(n.ctime_ns),
        kind: kind(n.kind),
        perm: n.mode as u16,
        nlink: n.nlink,
        uid: n.uid,
        gid: n.gid,
        rdev: 0,
        blksize: 65536,
        flags: 0,
    }
}
fn empty(reply: ReplyEmpty, r: Result<()>) {
    match r {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e.errno()),
    }
}
type DirectoryEntries = Vec<(u64, FileType, OsString)>;
pub struct XetFuse {
    fs: FileSystem,
    dirs: HashMap<u64, DirectoryEntries>,
    next_dir: u64,
}
impl XetFuse {
    fn entry(&mut self, reply: ReplyEntry, r: Result<Node>) {
        match r {
            Ok(n) => {
                self.fs.retain_inode(n.ino, 1);
                reply.entry(&TTL, &attr(&n), 0)
            }
            Err(e) => reply.error(e.errno()),
        }
    }
}
pub fn mount(fs: FileSystem, mountpoint: &Path) -> anyhow::Result<()> {
    let mountpoint = std::fs::canonicalize(mountpoint)?;
    anyhow::ensure!(mountpoint.is_dir(), "mountpoint must be a directory");
    anyhow::ensure!(
        !mountpoint.starts_with(fs.repository().path())
            && !fs.repository().path().starts_with(&mountpoint),
        "repository and mountpoint must be separate directories"
    );
    anyhow::ensure!(
        std::fs::read_dir(&mountpoint)?.next().is_none(),
        "mountpoint must be empty"
    );
    eprintln!("XetFS mounted at {} (foreground)", mountpoint.display());
    fuser::mount2(
        XetFuse {
            fs,
            dirs: HashMap::new(),
            next_dir: 1,
        },
        mountpoint,
        &[
            MountOption::FSName("xetfs".into()),
            MountOption::Subtype("xetfs".into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::RW,
        ],
    )?;
    Ok(())
}
impl Filesystem for XetFuse {
    fn destroy(&mut self) {
        if let Err(e) = self.fs.sync_all() {
            eprintln!("XetFS unmount sync failed: {e}");
        }
    }
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let r = self.fs.lookup(parent, name);
        self.entry(reply, r);
    }
    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        if let Err(e) = self.fs.forget_inode(ino, nlookup) {
            eprintln!("forget inode {ino}: {e}");
        }
    }
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.fs.node(ino) {
            Ok(n) => reply.attr(&TTL, &attr(&n)),
            Err(e) => reply.error(e.errno()),
        }
    }
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        match self.fs.setattr(
            ino,
            SetAttr {
                mode,
                uid,
                gid,
                size,
                atime_ns: atime.map(ns),
                mtime_ns: mtime.map(ns),
            },
        ) {
            Ok(n) => reply.attr(&TTL, &attr(&n)),
            Err(e) => reply.error(e.errno()),
        }
    }
    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.fs.node(ino) {
            Ok(n) if n.kind == Kind::Symlink => reply.data(n.target.as_deref().unwrap_or_default()),
            Ok(_) => reply.error(libc::EINVAL),
            Err(e) => reply.error(e.errno()),
        }
    }
    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & libc::S_IFMT != libc::S_IFREG {
            reply.error(libc::EOPNOTSUPP);
            return;
        }
        let r = self
            .fs
            .create(parent, name, mode & !umask, req.uid(), req.gid());
        self.entry(reply, r);
    }
    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let r = self
            .fs
            .mkdir(parent, name, mode & !umask, req.uid(), req.gid());
        self.entry(reply, r);
    }
    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        empty(reply, self.fs.unlink(parent, name, false));
    }
    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        empty(reply, self.fs.unlink(parent, name, true));
    }
    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let r = self.fs.symlink(
            parent,
            name,
            target.as_os_str().as_bytes(),
            req.uid(),
            req.gid(),
        );
        self.entry(reply, r);
    }
    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags & !libc::RENAME_NOREPLACE != 0 {
            reply.error(libc::EOPNOTSUPP);
            return;
        }
        empty(
            reply,
            self.fs.rename(
                parent,
                name,
                new_parent,
                new_name,
                flags & libc::RENAME_NOREPLACE != 0,
            ),
        );
    }
    fn link(&mut self, _req: &Request<'_>, ino: u64, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let r = self.fs.link(ino, parent, name);
        self.entry(reply, r);
    }
    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.fs.open_handle(ino, flags) {
            Ok(h) => reply.opened(h, DIRECT_IO),
            Err(e) => reply.error(e.errno()),
        }
    }
    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let r = self
            .fs
            .create(parent, name, mode & !umask, req.uid(), req.gid())
            .and_then(|n| self.fs.open_handle(n.ino, flags).map(|h| (n, h)));
        match r {
            Ok((n, h)) => {
                self.fs.retain_inode(n.ino, 1);
                reply.created(&TTL, &attr(&n), 0, h, DIRECT_IO)
            }
            Err(e) => reply.error(e.errno()),
        }
    }
    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.fs.read_handle(fh, offset as u64, size as usize) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e.errno()),
        }
    }
    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.fs.write_handle(fh, offset as u64, data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(e.errno()),
        }
    }
    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        empty(reply, self.fs.flush_handle(fh));
    }
    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        empty(reply, self.fs.close_handle(fh));
    }
    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        empty(reply, self.fs.flush_handle(fh));
    }
    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let r = (|| -> Result<_> {
            let nodes = self.fs.list(ino)?;
            let mut entries = vec![
                (ino, FileType::Directory, OsString::from(".")),
                (
                    self.fs.parent(ino)?,
                    FileType::Directory,
                    OsString::from(".."),
                ),
            ];
            entries.extend(nodes.into_iter().map(|e| {
                (
                    e.node.ino,
                    kind(e.node.kind),
                    OsStr::from_bytes(&e.name).to_os_string(),
                )
            }));
            Ok(entries)
        })();
        match r {
            Ok(entries) => {
                let h = self.next_dir;
                self.next_dir += 1;
                self.dirs.insert(h, entries);
                self.fs.retain_inode(ino, 1);
                reply.opened(h, 0)
            }
            Err(e) => reply.error(e.errno()),
        }
    }
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(entries) = self.dirs.get(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (i + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }
    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        self.dirs.remove(&fh);
        empty(reply, self.fs.forget_inode(ino, 1));
    }
    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let path = match CString::new(self.fs.repository().path().as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            reply.error(Error::Io(std::io::Error::last_os_error()).errno());
            return;
        }
        let s = unsafe { stats.assume_init() };
        reply.statfs(
            s.f_blocks,
            s.f_bfree,
            s.f_bavail,
            s.f_files,
            s.f_ffree,
            s.f_frsize as u32,
            255,
            s.f_frsize as u32,
        );
    }
}
