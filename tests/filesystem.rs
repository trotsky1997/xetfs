use std::{
    ffi::OsStr,
    io::{self, Read},
    os::unix::ffi::OsStrExt,
};
use xetfs::{Error, FileSystem, Kind, Repository, SetAttr, chunker::Chunker};

fn setup() -> (tempfile::TempDir, FileSystem) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path().join("repo")).unwrap();
    (dir, FileSystem::new(repo))
}
fn random(n: usize) -> Vec<u8> {
    let mut s = 4815162342u64;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}
fn is_errno<T>(result: xetfs::Result<T>, code: i32) {
    assert!(matches!(result,Err(Error::Errno(n)) if n==code));
}
#[test]
fn persisted_import_range_reads_and_duplicate_files() {
    let (dir, mut fs) = setup();
    let data = random(2 * 1024 * 1024);
    fs.mkdir_all("/models/weights").unwrap();
    let a = fs.put_file("/models/weights/a", data.as_slice()).unwrap();
    let before = fs.repository().stats().unwrap();
    let b = fs.put_file("/models/weights/b", data.as_slice()).unwrap();
    assert_ne!(a.ino, b.ino);
    assert_eq!(a.content, b.content);
    assert_eq!(
        fs.repository().stats().unwrap().xorb_bytes,
        before.xorb_bytes
    );
    assert_eq!(
        fs.read_inode(a.ino, 65432, 150000).unwrap(),
        data[65432..215432]
    );
    assert!(fs.read_inode(a.ino, u64::MAX, 10).unwrap().is_empty());
    drop(fs);
    let fs = FileSystem::new(Repository::open(dir.path().join("repo")).unwrap());
    let n = fs.resolve("/models/weights/a").unwrap();
    assert_eq!(fs.read_inode(n.ino, 0, usize::MAX).unwrap(), data);
    fs.repository().fsck().unwrap();
}
#[test]
fn shared_handles_random_writes_holes_append_and_truncate() {
    let (_dir, mut fs) = setup();
    let n = fs.put_file("/file", b"hello".as_slice()).unwrap();
    let a = fs.open_handle(n.ino, libc::O_RDWR).unwrap();
    let b = fs
        .open_handle(n.ino, libc::O_RDWR | libc::O_APPEND)
        .unwrap();
    fs.write_handle(a, 10, b"world").unwrap();
    assert_eq!(fs.read_handle(b, 0, 100).unwrap(), b"hello\0\0\0\0\0world");
    fs.write_handle(b, 0, b"!").unwrap();
    assert_eq!(fs.node(n.ino).unwrap().size, 16);
    fs.setattr(
        n.ino,
        SetAttr {
            size: Some(3),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.read_handle(a, 0, 100).unwrap(), b"hel");
    fs.flush_handle(a).unwrap();
    fs.close_handle(a).unwrap();
    fs.close_handle(b).unwrap();
    fs.repository().fsck().unwrap();
}
#[test]
fn open_unlink_and_rename_replacement_preserve_old_handles() {
    let (_dir, mut fs) = setup();
    let a = fs.put_file("/a", b"old".as_slice()).unwrap();
    let h = fs.open_handle(a.ino, libc::O_RDWR).unwrap();
    let b = fs.put_file("/b", b"new".as_slice()).unwrap();
    fs.rename(1, OsStr::new("b"), 1, OsStr::new("a"), false)
        .unwrap();
    assert_eq!(fs.resolve("/a").unwrap().ino, b.ino);
    assert_eq!(fs.read_handle(h, 0, 100).unwrap(), b"old");
    assert_eq!(fs.node(a.ino).unwrap().nlink, 0);
    fs.write_handle(h, 3, b" but open").unwrap();
    assert_eq!(fs.read_handle(h, 0, 100).unwrap(), b"old but open");
    fs.close_handle(h).unwrap();
    is_errno(fs.node(a.ino), libc::ENOENT);
    fs.repository().fsck().unwrap();
}
#[test]
fn hard_links_share_mutations_but_recipe_copies_do_not() {
    let (_dir, mut fs) = setup();
    let a = fs.put_file("/a", b"abc".as_slice()).unwrap();
    fs.link(a.ino, 1, OsStr::new("hard")).unwrap();
    let copy = fs.copy_file("/a", "/copy").unwrap();
    assert_eq!(fs.node(a.ino).unwrap().nlink, 2);
    assert_ne!(a.ino, copy.ino);
    let h = fs.open_handle(a.ino, libc::O_RDWR).unwrap();
    fs.write_handle(h, 0, b"xyz").unwrap();
    fs.close_handle(h).unwrap();
    assert_eq!(
        fs.read_inode(fs.resolve("/hard").unwrap().ino, 0, 10)
            .unwrap(),
        b"xyz"
    );
    assert_eq!(fs.read_inode(copy.ino, 0, 10).unwrap(), b"abc");
    fs.rename(1, OsStr::new("hard"), 1, OsStr::new("a"), false)
        .unwrap();
    assert_eq!(fs.node(a.ino).unwrap().nlink, 2);
}
#[test]
fn snapshots_preserve_namespace_links_and_content_across_gc() {
    let (_dir, mut fs) = setup();
    fs.mkdir_all("/dir/sub").unwrap();
    let a = fs
        .put_file("/dir/sub/a", b"version one".as_slice())
        .unwrap();
    fs.link(a.ino, 1, OsStr::new("hard")).unwrap();
    fs.symlink(1, OsStr::new("sym"), b"dir/sub/a", 1, 2)
        .unwrap();
    let h = fs.open_handle(a.ino, libc::O_RDWR).unwrap();
    fs.write_handle(h, 0, b"VERSION").unwrap();
    fs.snapshot("v1").unwrap();
    assert_eq!(
        fs.read_snapshot("v1", "/sym", 0, 100).unwrap(),
        b"VERSION one"
    );
    assert!(matches!(fs.restore("v1"), Err(Error::Busy)));
    fs.close_handle(h).unwrap();
    fs.put_file("/dir/sub/a", b"two".as_slice()).unwrap();
    fs.unlink(1, OsStr::new("sym"), false).unwrap();
    fs.gc(true).unwrap();
    assert_eq!(
        fs.read_snapshot("v1", "/dir/sub/a", 0, 100).unwrap(),
        b"VERSION one"
    );
    fs.restore("v1").unwrap();
    assert_eq!(
        fs.read_inode(fs.resolve("/hard").unwrap().ino, 0, 100)
            .unwrap(),
        b"VERSION one"
    );
    assert_eq!(
        fs.resolve("/sym").unwrap().ino,
        fs.resolve("/hard").unwrap().ino
    );
    fs.drop_snapshot("v1").unwrap();
    fs.gc(true).unwrap();
    fs.repository().fsck().unwrap();
}
#[test]
fn gc_compacts_partially_live_xorbs_without_changing_recipes() {
    let (_dir, mut fs) = setup();
    let data = random(2 * 1024 * 1024);
    let first = Chunker::new(data.as_slice()).next().unwrap().unwrap();
    fs.put_file("/large", data.as_slice()).unwrap();
    let small = fs.put_file("/small", first.as_slice()).unwrap();
    let before = fs.repository().stats().unwrap().xorb_bytes;
    fs.unlink(1, OsStr::new("large"), false).unwrap();
    fs.gc(false).unwrap();
    assert_eq!(fs.repository().stats().unwrap().xorb_bytes, before);
    fs.repository().fsck().unwrap();
    let report = fs.gc(true).unwrap();
    assert!(report.reclaimed_bytes > 0);
    assert!(fs.repository().stats().unwrap().xorb_bytes < before / 2);
    assert_eq!(fs.read_inode(small.ino, 0, usize::MAX).unwrap(), first);
    fs.repository().fsck().unwrap();
}
#[test]
fn directory_rename_cycles_type_checks_and_link_counts() {
    let (_dir, mut fs) = setup();
    let a = fs.mkdir_all("/a/b/c").unwrap();
    fs.mkdir_all("/dest").unwrap();
    assert_eq!(fs.node(1).unwrap().nlink, 4);
    is_errno(
        fs.rename(1, OsStr::new("a"), a.ino, OsStr::new("cycle"), false),
        libc::EINVAL,
    );
    is_errno(fs.unlink(1, OsStr::new("a"), true), libc::ENOTEMPTY);
    is_errno(fs.link(a.ino, 1, OsStr::new("bad")), libc::EPERM);
    fs.put_file("/file", b"f".as_slice()).unwrap();
    is_errno(
        fs.rename(1, OsStr::new("file"), 1, OsStr::new("dest"), false),
        libc::EISDIR,
    );
    is_errno(
        fs.rename(1, OsStr::new("a"), 1, OsStr::new("file"), false),
        libc::ENOTDIR,
    );
    is_errno(
        fs.rename(1, OsStr::new("file"), 1, OsStr::new("a"), true),
        libc::EEXIST,
    );
    fs.rename(1, OsStr::new("a"), 1, OsStr::new("dest"), false)
        .unwrap();
    assert_eq!(fs.resolve("/dest/b/c").unwrap().ino, a.ino);
    fs.repository().fsck().unwrap();
}
#[test]
fn symlink_resolution_and_non_utf8_names() {
    let (_dir, mut fs) = setup();
    let dir = fs.mkdir_all("/dir").unwrap();
    let f = fs.put_file("/dir/a", b"a".as_slice()).unwrap();
    fs.symlink(1, OsStr::new("relative"), b"dir/a", 1, 1)
        .unwrap();
    fs.symlink(dir.ino, OsStr::new("parent"), b"../relative", 1, 1)
        .unwrap();
    assert_eq!(fs.resolve("/dir/parent").unwrap().ino, f.ino);
    assert_eq!(fs.resolve("/dir/../relative").unwrap().ino, f.ino);
    is_errno(fs.resolve("/relative/"), libc::ENOTDIR);
    fs.symlink(1, OsStr::new("loop"), b"loop", 1, 1).unwrap();
    is_errno(fs.resolve("/loop"), libc::ELOOP);
    let raw = OsStr::from_bytes(b"raw-\xff");
    let n = fs.create(1, raw, 0o600, 1, 1).unwrap();
    assert_eq!(fs.lookup(1, raw).unwrap().ino, n.ino);
    assert!(fs.list(1).unwrap().iter().any(|e| e.name == b"raw-\xff"));
}
#[test]
fn handle_access_modes_and_gc_exclusion() {
    let (_dir, mut fs) = setup();
    let a = fs.put_file("/a", b"a".as_slice()).unwrap();
    let r = fs.open_handle(a.ino, libc::O_RDONLY).unwrap();
    is_errno(fs.write_handle(r, 0, b"x"), libc::EBADF);
    assert!(matches!(fs.gc(true), Err(Error::Busy)));
    fs.close_handle(r).unwrap();
    let w = fs
        .open_handle(a.ino, libc::O_WRONLY | libc::O_SYNC)
        .unwrap();
    is_errno(fs.read_handle(w, 0, 1), libc::EBADF);
    fs.write_handle(w, 0, b"b").unwrap();
    fs.repository().fsck().unwrap();
    fs.close_handle(w).unwrap();
    is_errno(fs.close_handle(w), libc::EBADF);
}
#[test]
fn repository_lock_rejects_second_owner() {
    let (dir, fs) = setup();
    assert!(matches!(
        Repository::open(dir.path().join("repo")),
        Err(Error::Busy)
    ));
    drop(fs);
    Repository::open(dir.path().join("repo")).unwrap();
}
#[test]
fn missing_and_corrupt_objects_are_detected() {
    let (_dir, mut fs) = setup();
    let n = fs.put_file("/a", b"integrity".as_slice()).unwrap();
    let object = std::fs::read_dir(fs.repository().path().join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = std::fs::read(&object).unwrap();
    let mut bad = bytes.clone();
    *bad.last_mut().unwrap() ^= 1;
    std::fs::write(&object, bad).unwrap();
    assert!(fs.read_inode(n.ino, 0, 100).is_err());
    assert!(fs.repository().fsck().is_err());
    std::fs::write(&object, bytes).unwrap();
    fs.repository().fsck().unwrap();
    std::fs::remove_file(object).unwrap();
    assert!(fs.repository().fsck().is_err());
}
struct FailingStream {
    left: usize,
    state: u64,
    fail_at_end: bool,
}
impl Read for FailingStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return if self.fail_at_end {
                Err(io::Error::other("injected read failure"))
            } else {
                Ok(0)
            };
        }
        let n = self.left.min(out.len());
        self.left -= n;
        for b in &mut out[..n] {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            *b = (self.state >> 24) as u8;
        }
        Ok(n)
    }
}
#[test]
fn failed_multi_pack_import_keeps_old_file_and_gc_removes_orphans() {
    let (dir, mut fs) = setup();
    fs.put_file("/a", b"old durable bytes".as_slice()).unwrap();
    assert!(
        fs.put_file(
            "/a",
            FailingStream {
                left: 70 * 1024 * 1024,
                state: 12345,
                fail_at_end: true
            }
        )
        .is_err()
    );
    assert_eq!(
        fs.read_inode(fs.resolve("/a").unwrap().ino, 0, 100)
            .unwrap(),
        b"old durable bytes"
    );
    fs.repository().fsck().unwrap();
    let report = fs.gc(false).unwrap();
    assert!(report.removed_objects > 0);
    drop(fs);
    FileSystem::new(Repository::open(dir.path().join("repo")).unwrap())
        .repository()
        .fsck()
        .unwrap();
}

#[test]
fn successful_large_import_splits_xorbs_and_reads_across_the_pack_boundary() {
    use std::io::{Seek, SeekFrom, Write};
    let (_dir, mut fs) = setup();
    let mut input = tempfile::NamedTempFile::new().unwrap();
    io::copy(
        &mut FailingStream {
            left: 68 * 1024 * 1024,
            state: 1234567,
            fail_at_end: false,
        },
        input.as_file_mut(),
    )
    .unwrap();
    input.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
    let n = fs.put_file("/large", input.as_file_mut()).unwrap();
    assert!(fs.repository().stats().unwrap().xorbs >= 2);
    let offset = 64 * 1024 * 1024 - 128;
    input.as_file_mut().seek(SeekFrom::Start(offset)).unwrap();
    let mut expected = vec![0; 65536];
    input.as_file_mut().read_exact(&mut expected).unwrap();
    assert_eq!(fs.read_inode(n.ino, offset, 65536).unwrap(), expected);
    struct Digest(blake3::Hasher);
    impl Write for Digest {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut original = Digest(blake3::Hasher::new());
    input.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
    io::copy(input.as_file_mut(), &mut original).unwrap();
    let mut reconstructed = Digest(blake3::Hasher::new());
    fs.repository()
        .export_content(n.content.as_deref().unwrap(), &mut reconstructed)
        .unwrap();
    assert_eq!(original.0.finalize(), reconstructed.0.finalize());
    fs.repository().fsck().unwrap();
}
#[test]
fn seeded_random_writes_and_truncation_match_a_byte_vector_after_reopens() {
    let (dir, mut fs) = setup();
    let n = fs.put_file("/file", b"".as_slice()).unwrap();
    let mut h = fs.open_handle(n.ino, libc::O_RDWR).unwrap();
    let mut model = Vec::new();
    let mut state = 998877u64;
    for step in 0..200 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        if state.is_multiple_of(5) {
            let len = (state as usize >> 8) % 32768;
            model.resize(len, 0);
            fs.setattr(
                n.ino,
                SetAttr {
                    size: Some(len as u64),
                    ..Default::default()
                },
            )
            .unwrap();
        } else {
            let at = (state as usize >> 8) % 32768;
            let bytes = random((state as usize % 1000) + 1);
            if at + bytes.len() > model.len() {
                model.resize(at + bytes.len(), 0);
            }
            model[at..at + bytes.len()].copy_from_slice(&bytes);
            fs.write_handle(h, at as u64, &bytes).unwrap();
        }
        assert_eq!(fs.read_handle(h, 0, 65536).unwrap(), model);
        if step % 40 == 39 {
            fs.close_handle(h).unwrap();
            drop(fs);
            fs = FileSystem::new(Repository::open(dir.path().join("repo")).unwrap());
            h = fs.open_handle(n.ino, libc::O_RDWR).unwrap();
            assert_eq!(fs.read_handle(h, 0, 65536).unwrap(), model);
        }
    }
    fs.close_handle(h).unwrap();
    fs.gc(true).unwrap();
    fs.repository().fsck().unwrap();
}
#[test]
fn kernel_lookup_pins_keep_unlinked_inode_until_forget() {
    let (_dir, mut fs) = setup();
    let n = fs.put_file("/a", b"a".as_slice()).unwrap();
    fs.retain_inode(n.ino, 1);
    fs.unlink(1, OsStr::new("a"), false).unwrap();
    assert_eq!(fs.node(n.ino).unwrap().nlink, 0);
    fs.forget_inode(n.ino, 1).unwrap();
    is_errno(fs.node(n.ino), libc::ENOENT);
}
#[test]
fn ownership_modes_and_explicit_timestamps_survive_restore() {
    let (_dir, mut fs) = setup();
    let n = fs.create(1, OsStr::new("a"), 0o6755, 12, 34).unwrap();
    fs.setattr(
        n.ino,
        SetAttr {
            atime_ns: Some(-100),
            mtime_ns: Some(123456),
            ..Default::default()
        },
    )
    .unwrap();
    fs.snapshot("before").unwrap();
    let h = fs.open_handle(n.ino, libc::O_WRONLY).unwrap();
    fs.write_handle(h, 0, b"x").unwrap();
    fs.close_handle(h).unwrap();
    assert_eq!(fs.node(n.ino).unwrap().mode & 0o6000, 0);
    fs.restore("before").unwrap();
    let n = fs.node(n.ino).unwrap();
    assert_eq!(
        (n.mode, n.uid, n.gid, n.atime_ns, n.mtime_ns),
        (0o6755, 12, 34, -100, 123456)
    );
    assert_eq!(n.kind, Kind::File);
}

#[test]
fn reconstruction_terms_cover_the_file_and_remain_valid_after_compaction() {
    let (_dir, mut fs) = setup();
    let data = random(2 * 1024 * 1024);
    let n = fs.put_file("/a", data.as_slice()).unwrap();
    let id = n.content.unwrap();
    let before = fs.repository().reconstruction(&id).unwrap();
    assert_eq!(before.terms.len(), 1);
    assert_eq!(before.terms[0].offset, 0);
    assert_eq!(before.terms[0].size, data.len() as u64);
    assert!(before.terms[0].chunk_end > 1);
    fs.gc(true).unwrap();
    let after = fs.repository().reconstruction(&id).unwrap();
    assert_eq!(after.file_hash, before.file_hash);
    assert_eq!(
        after.terms.iter().map(|t| t.size).sum::<u64>(),
        data.len() as u64
    );
    assert_eq!(
        fs.read_inode(n.ino, 1000000, 77).unwrap(),
        data[1000000..1000077]
    );
}
