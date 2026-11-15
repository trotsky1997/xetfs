use crate::{FileSystem, Repository};
use std::{ffi::OsStr, io::Read, path::Path};

fn commits(root: &Path) -> usize {
    let bytes = std::fs::read(root.join("metadata.sqlite-wal")).unwrap_or_default();
    if bytes.is_empty() {
        return 0;
    }
    let page_size = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    bytes[32..]
        .chunks_exact(24 + page_size)
        .filter(|frame| u32::from_be_bytes(frame[4..8].try_into().unwrap()) != 0)
        .count()
}

#[test]
fn many_small_writes_publish_in_one_durable_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let mut fs = FileSystem::new(Repository::init(&root).unwrap());
    let node = fs.create(1, OsStr::new("data"), 0o644, 0, 0).unwrap();
    let handle = fs.open_handle(node.ino, libc::O_RDWR).unwrap();
    fs.repository()
        .conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let data = vec![0x5a; 8192];
    for index in 0..32 {
        fs.write_handle(handle, index * 8192, &data).unwrap();
        assert_eq!(fs.node(node.ino).unwrap().size, (index + 1) * 8192);
    }
    assert_eq!(
        commits(&root),
        0,
        "ordinary staged writes must not commit metadata"
    );
    fs.flush_handle(handle).unwrap();
    assert_eq!(
        commits(&root),
        1,
        "recipe and inode must publish atomically"
    );
    fs.close_handle(handle).unwrap();
    drop(fs);
    let reopened = FileSystem::new(Repository::open(&root).unwrap());
    assert_eq!(
        reopened.read_inode(node.ino, 0, 32 * 8192).unwrap(),
        data.repeat(32)
    );
    reopened.repository().fsck().unwrap();
}

#[test]
fn failed_ingest_does_not_publish_a_new_inode_or_recipe() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected read failure"))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let mut fs = FileSystem::new(Repository::init(&root).unwrap());
    let node = fs
        .put_file("/old", b"previous durable content".as_slice())
        .unwrap();
    let content = node.content.clone().unwrap();
    drop(fs);
    let mut repo = Repository::open(&root).unwrap();
    assert!(repo.ingest_inode(Broken, node.ino, 123).is_err());
    assert_eq!(
        repo.read_content(&content, 0, 100).unwrap(),
        b"previous durable content"
    );
    repo.fsck().unwrap();
    assert_eq!(
        FileSystem::new(repo).node(node.ino).unwrap().content,
        node.content
    );
}
