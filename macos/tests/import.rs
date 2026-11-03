use std::{
    ffi::CString,
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, symlink},
    },
    process::Command,
};
use xetfs::{FileSystem, Repository};

#[test]
fn import_preserves_supported_data_and_rejects_incomplete_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let repo = temp.path().join("repo");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("dir")).unwrap();
    let file = source.join("dir/file");
    fs::write(&file, b"content").unwrap();
    for i in 0..20 {
        fs::write(
            source.join("dir").join(format!("small-{i}")),
            format!("unique-{i}"),
        )
        .unwrap();
    }
    fs::hard_link(&file, source.join("hardlink")).unwrap();
    symlink("dir/file", source.join("symlink")).unwrap();
    let path = CString::new(file.as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.example").unwrap();
    assert_eq!(
        unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                b"value".as_ptr().cast(),
                5,
                0,
                0,
            )
        },
        0
    );
    assert_eq!(unsafe { libc::chflags(path.as_ptr(), 0x8000) }, 0);
    let run = |destination: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_import-directory"))
            .args([&source, destination])
            .output()
            .unwrap()
    };
    let check = || {
        Command::new(env!("CARGO_BIN_EXE_import-directory"))
            .args([std::ffi::OsStr::new("--check"), source.as_os_str()])
            .output()
            .unwrap()
    };
    assert!(check().status.success());
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let denied = source.join("denied");
        fs::write(&denied, b"private").unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0)).unwrap();
        let result = check();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(denied).unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("cannot read"));
    }
    let result = run(&repo);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let fs = FileSystem::new(Repository::open(&repo).unwrap());
    let node = fs.resolve("/dir/file").unwrap();
    assert_eq!(node.ino, fs.resolve("/hardlink").unwrap().ino);
    assert_eq!(node.nlink, 2);
    assert_eq!(node.flags, 0x8000);
    assert_eq!(node.uid, fs::metadata(&file).unwrap().uid());
    assert_eq!(fs.get_xattr(node.ino, b"user.example").unwrap(), b"value");
    assert_eq!(fs.read_inode(node.ino, 0, 7).unwrap(), b"content");
    assert_eq!(fs.resolve("/symlink").unwrap().ino, node.ino);
    fs.repository().fsck().unwrap();
    assert!(fs::read_dir(repo.join("objects")).unwrap().count() <= 2);
    for i in 0..20 {
        let node = fs.resolve(format!("/dir/small-{i}")).unwrap();
        assert_eq!(
            fs.read_inode(node.ino, 0, 32).unwrap(),
            format!("unique-{i}").as_bytes()
        );
    }
    drop(fs);
    let fifo = CString::new(source.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let failed = temp.path().join("incomplete");
    assert!(!check().status.success());
    assert!(!run(&failed).status.success());
    assert!(failed.join("IMPORT_INCOMPLETE").exists());
    assert!(Repository::open(&failed).is_err());
    assert!(Repository::identifier(&failed).is_err());
    fs::remove_file(source.join("fifo")).unwrap();
    let other = temp.path().join("other-source");
    fs::create_dir(&other).unwrap();
    let wrong_source = Command::new(env!("CARGO_BIN_EXE_import-directory"))
        .args([
            std::ffi::OsStr::new("--resume"),
            other.as_os_str(),
            failed.as_os_str(),
        ])
        .output()
        .unwrap();
    assert!(!wrong_source.status.success());
    assert!(String::from_utf8_lossy(&wrong_source.stderr).contains("source does not match"));
    assert!(failed.join("IMPORT_INCOMPLETE").exists());
    let resume = Command::new(env!("CARGO_BIN_EXE_import-directory"))
        .args([
            std::ffi::OsStr::new("--resume"),
            source.as_os_str(),
            failed.as_os_str(),
        ])
        .output()
        .unwrap();
    assert!(
        resume.status.success(),
        "{}",
        String::from_utf8_lossy(&resume.stderr)
    );
    let recovered = FileSystem::new(Repository::open(&failed).unwrap());
    let recovered_node = recovered.resolve("/dir/file").unwrap();
    assert_eq!(
        recovered.read_inode(recovered_node.ino, 0, 7).unwrap(),
        b"content"
    );
    assert_eq!(
        recovered_node.ino,
        recovered.resolve("/hardlink").unwrap().ino
    );
    drop(recovered);
    assert!(!failed.join("IMPORT_INCOMPLETE").exists());
    let invalid_resume = Command::new(env!("CARGO_BIN_EXE_import-directory"))
        .args([
            std::ffi::OsStr::new("--resume"),
            source.as_os_str(),
            repo.as_os_str(),
        ])
        .output()
        .unwrap();
    assert!(!invalid_resume.status.success());
    assert!(Repository::open(&repo).is_ok());
    fs::hard_link(&file, temp.path().join("external-hardlink")).unwrap();
    let external = temp.path().join("external");
    assert!(!check().status.success());
    assert!(!run(&external).status.success());
    assert!(Repository::open(&external).is_err());
}
