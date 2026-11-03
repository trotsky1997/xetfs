use xetfs::{Error, Repository};
use xetfs_fskit::Volume;

#[test]
fn independent_repository_retains_open_unlinked_file_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let volume = Volume::open(&repo).unwrap();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let item = volume.create(1, b"file", 0o600, uid, gid).unwrap();
    volume.retain(item.ino).unwrap();
    let fh = volume.open_file(item.ino, libc::O_RDWR).unwrap();
    volume.write(fh, 0, b"hello").unwrap();
    volume.flush(fh).unwrap();
    volume.remove(1, b"file", false).unwrap();
    assert_eq!(volume.read(fh, 0, 5).unwrap(), b"hello");
    assert!(matches!(
        volume.lookup(1, b"file"),
        Err(Error::Errno(libc::ENOENT))
    ));
    volume.close(fh).unwrap();
    assert_eq!(volume.item(item.ino).unwrap().nlink, 0);
    volume.reclaim(item.ino).unwrap();
    drop(volume);
    let volume = Volume::open(&repo).unwrap();
    assert!(volume.list(1).unwrap().is_empty());
}

#[test]
fn native_fskit_item_keeps_inode() {
    let item = xetfs_fskit::XetItem::new(42);
    assert_eq!(item.inode(), 42);
}

#[test]
fn native_fskit_write_callback_persists_in_an_independent_repository() {
    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2_foundation::NSData;
    use objc2_fs_kit::{FSFileName, FSVolumeIdentifier, FSVolumeReadWriteOperations};
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let mut fs = xetfs::FileSystem::new(Repository::init(&repo).unwrap());
    let item = fs
        .create(
            1,
            std::ffi::OsStr::new("native"),
            0o600,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
        .unwrap();
    drop(fs);
    let id = unsafe { FSVolumeIdentifier::new() };
    let name =
        unsafe { FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"XetFS")) };
    let volume = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let node = xetfs_fskit::XetItem::new(item.ino);
    let result = Arc::new(Mutex::new(None));
    let result_cb = result.clone();
    let reply = RcBlock::new(move |n: usize, error: *mut objc2_foundation::NSError| {
        *result_cb.lock().unwrap() = Some((n, error.is_null()));
    });
    unsafe {
        volume.writeContents_toFile_atOffset_replyHandler(
            &NSData::with_bytes(b"native"),
            &node,
            0,
            &reply,
        )
    };
    assert_eq!(*result.lock().unwrap(), Some((6, true)));
    drop(volume);
    let volume = Volume::open(&repo).unwrap();
    assert_eq!(volume.read_inode(item.ino, 0, 6).unwrap(), b"native");
}

#[test]
fn native_lookup_pins_unlinked_inode_until_reclaim() {
    use block2::RcBlock;
    use objc2::{AnyThread, msg_send, rc::Retained};
    use objc2_foundation::{NSData, NSError};
    use objc2_fs_kit::{
        FSFileName, FSItem, FSItemAttributes, FSItemGetAttributesRequest, FSVolumeIdentifier,
    };
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let mut fs = xetfs::FileSystem::new(Repository::init(&repo).unwrap());
    let n = fs
        .create(
            1,
            std::ffi::OsStr::new("pinned"),
            0o640,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
        .unwrap();
    drop(fs);
    let id = unsafe { FSVolumeIdentifier::new() };
    let name =
        unsafe { FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"pinned")) };
    let volume = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let root = xetfs_fskit::XetItem::new(1);
    let result: Arc<Mutex<Option<Retained<FSItem>>>> = Arc::new(Mutex::new(None));
    let capture = result.clone();
    let reply = RcBlock::new(
        move |item: *mut FSItem, _: *mut FSFileName, error: *mut NSError| {
            assert!(error.is_null());
            *capture.lock().unwrap() = unsafe { Retained::retain(item) };
        },
    );
    unsafe {
        let _: () = msg_send![&*volume, lookupItemNamed: &*name, inDirectory: &*root, replyHandler: &*reply];
    }
    let item = result.lock().unwrap().take().unwrap();
    unsafe {
        let _: () = msg_send![&*volume, lookupItemNamed: &*name, inDirectory: &*root, replyHandler: &*reply];
    }
    let second = result.lock().unwrap().take().unwrap();
    assert_eq!(Retained::as_ptr(&item), Retained::as_ptr(&second));
    drop(second);
    let removed = Arc::new(Mutex::new(false));
    let capture = removed.clone();
    let remove_reply = RcBlock::new(move |error: *mut NSError| {
        assert!(error.is_null());
        *capture.lock().unwrap() = true;
    });
    unsafe {
        let _: () = msg_send![&*volume, removeItem: &*item, named: &*name, fromDirectory: &*root, replyHandler: &*remove_reply];
    }
    assert!(*removed.lock().unwrap());
    let request = unsafe { FSItemGetAttributesRequest::new() };
    let attrs = Arc::new(Mutex::new(None));
    let capture = attrs.clone();
    let reply = RcBlock::new(move |attr: *mut FSItemAttributes, error: *mut NSError| {
        assert!(error.is_null());
        *capture.lock().unwrap() = Some(unsafe { ((*attr).fileID().0, (*attr).mode()) });
    });
    unsafe {
        let _: () =
            msg_send![&*volume, getAttributes: &*request, ofItem: &*item, replyHandler: &*reply];
    }
    assert_eq!(*attrs.lock().unwrap(), Some((n.ino + 1, 0o640)));
    let done = Arc::new(Mutex::new(false));
    let capture = done.clone();
    let reply = RcBlock::new(move |error: *mut NSError| {
        assert!(error.is_null());
        *capture.lock().unwrap() = true;
    });
    unsafe {
        let _: () = msg_send![&*volume, reclaimItem: &*item, replyHandler: &*reply];
    }
    assert!(*done.lock().unwrap());
}

#[test]
fn directory_snapshot_changes_on_mutation_and_rejects_stale_names() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let v = Volume::open(&repo).unwrap();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let n = v.create(1, b"first", 0o600, uid, gid).unwrap();
    let before = v.directory_snapshot(1).unwrap().0;
    v.rename_checked(1, b"first", n.ino, 1, b"second", None)
        .unwrap();
    assert_ne!(before, v.directory_snapshot(1).unwrap().0);
    assert!(matches!(
        v.remove_checked(1, b"second", n.ino + 1),
        Err(Error::Errno(libc::ESTALE))
    ));
    assert_eq!(v.lookup(1, b"second").unwrap().ino, n.ino);
}

#[test]
fn native_setattr_consumes_requested_mode_and_preserves_timestamp() {
    use block2::RcBlock;
    use objc2::{AnyThread, msg_send};
    use objc2_foundation::{NSData, NSError};
    use objc2_fs_kit::{
        FSFileName, FSItemAttribute, FSItemAttributes, FSItemSetAttributesRequest,
        FSVolumeIdentifier, FSVolumeOperations,
    };
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let mut fs = xetfs::FileSystem::new(Repository::init(&repo).unwrap());
    let n = fs
        .create(
            1,
            std::ffi::OsStr::new("attrs"),
            0o600,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
        .unwrap();
    drop(fs);
    let id = unsafe { FSVolumeIdentifier::new() };
    let name =
        unsafe { FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"XetFS")) };
    let v = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let item = xetfs_fskit::XetItem::new(n.ino);
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TimeSpec {
        sec: i64,
        nsec: i64,
    }
    unsafe impl objc2::Encode for TimeSpec {
        const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
            "timespec",
            &[objc2::Encoding::LongLong, objc2::Encoding::LongLong],
        );
    }
    let attrs = unsafe { FSItemSetAttributesRequest::new() };
    unsafe {
        attrs.setMode(0o640);
        attrs.setFlags(0x8000);
        let _: () = msg_send![&*attrs, setModifyTime: TimeSpec { sec: 123, nsec: 456 }];
    };
    let result = Arc::new(Mutex::new(None));
    let target = result.clone();
    let reply = RcBlock::new(move |returned: *mut FSItemAttributes, err: *mut NSError| {
        assert!(err.is_null());
        let value = unsafe { (*returned).mode() };
        *target.lock().unwrap() = Some(value);
    });
    unsafe { v.setAttributes_onItem_replyHandler(&attrs, &item, &reply) };
    assert_eq!(*result.lock().unwrap(), Some(0o640));
    assert!(unsafe { attrs.wasAttributeConsumed(FSItemAttribute::Mode) });
    assert!(unsafe { attrs.wasAttributeConsumed(FSItemAttribute::Flags) });
    drop(v);
    let node = Volume::open(&repo).unwrap().item(n.ino).unwrap();
    assert_eq!(node.mode, 0o640);
    assert_eq!(node.mtime_ns, 123_000_000_456);
    assert_eq!(node.flags, 0x8000);
}

#[test]
fn unary_probe_and_load_use_a_real_independent_repository() {
    use block2::RcBlock;
    use objc2::{AnyThread, rc::Retained};
    use objc2_foundation::{NSError, NSString, NSURL};
    use objc2_fs_kit::{
        FSMatchResult, FSPathURLResource, FSProbeResult, FSTaskOptions,
        FSUnaryFileSystemOperations, FSVolume,
    };
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let url = NSURL::fileURLWithPath(&NSString::from_str(repo.to_str().unwrap()));
    let resource =
        unsafe { FSPathURLResource::initWithURL_writable(FSPathURLResource::alloc(), &url, true) };
    let unary = xetfs_fskit::XetUnary::new();
    let result = Arc::new(Mutex::new(None));
    let capture = result.clone();
    let reply = RcBlock::new(move |probe: *mut FSProbeResult, err: *mut NSError| {
        assert!(err.is_null());
        *capture.lock().unwrap() = Some(unsafe { (*probe).result() });
    });
    unsafe { unary.probeResource_replyHandler(&resource, &reply) };
    assert_eq!(*result.lock().unwrap(), Some(FSMatchResult::Usable));
    let opts = unsafe { FSTaskOptions::new() };
    let loaded: Arc<Mutex<Option<Retained<FSVolume>>>> = Arc::new(Mutex::new(None));
    let capture = loaded.clone();
    let reply = RcBlock::new(move |volume: *mut FSVolume, err: *mut NSError| {
        assert!(err.is_null());
        *capture.lock().unwrap() = unsafe { Retained::retain(volume) };
    });
    unsafe { unary.loadResource_options_replyHandler(&resource, &opts, &reply) };
    assert!(loaded.lock().unwrap().take().is_some());
}

#[test]
fn native_create_then_lookup_returns_same_item_and_consumes_attributes() {
    use block2::RcBlock;
    use objc2::{AnyThread, msg_send, rc::Retained};
    use objc2_foundation::{NSData, NSError};
    use objc2_fs_kit::{
        FSFileName, FSItem, FSItemAttribute, FSItemSetAttributesRequest, FSItemType,
        FSVolumeIdentifier, FSVolumeOperations,
    };
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let id = unsafe { FSVolumeIdentifier::new() };
    let name =
        unsafe { FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"created")) };
    let v = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let root = xetfs_fskit::XetItem::new(1);
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TimeSpec {
        sec: i64,
        nsec: i64,
    }
    unsafe impl objc2::Encode for TimeSpec {
        const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
            "timespec",
            &[objc2::Encoding::LongLong, objc2::Encoding::LongLong],
        );
    }
    let attrs = unsafe { FSItemSetAttributesRequest::new() };
    unsafe {
        attrs.setMode(0o640);
        attrs.setFlags(0x8000);
        attrs.setUid(libc::getuid());
        attrs.setGid(libc::getgid());
        let _: () = msg_send![&*attrs, setBirthTime: TimeSpec { sec: -1, nsec: 17 }];
    }
    let output: Arc<Mutex<Option<Retained<FSItem>>>> = Arc::new(Mutex::new(None));
    let target = output.clone();
    let reply = RcBlock::new(
        move |item: *mut FSItem, _: *mut FSFileName, err: *mut NSError| {
            assert!(err.is_null());
            *target.lock().unwrap() = unsafe { Retained::retain(item) };
        },
    );
    unsafe {
        v.createItemNamed_type_inDirectory_attributes_replyHandler(
            &name,
            FSItemType::File,
            &root,
            &attrs,
            &reply,
        )
    };
    let created = output.lock().unwrap().take().unwrap();
    assert!(unsafe { attrs.wasAttributeConsumed(FSItemAttribute::Mode) });
    assert!(unsafe { attrs.wasAttributeConsumed(FSItemAttribute::Flags) });
    assert!(unsafe { attrs.wasAttributeConsumed(FSItemAttribute::BirthTime) });
    let ino = created
        .downcast_ref::<xetfs_fskit::XetItem>()
        .unwrap()
        .inode();
    unsafe { v.lookupItemNamed_inDirectory_replyHandler(&name, &root, &reply) };
    let looked_up = output.lock().unwrap().take().unwrap();
    assert_eq!(Retained::as_ptr(&created), Retained::as_ptr(&looked_up));
    drop(v);
    assert_eq!(
        Volume::open(&repo).unwrap().item(ino).unwrap().flags,
        0x8000
    );
    assert_eq!(
        Volume::open(&repo).unwrap().item(ino).unwrap().birthtime_ns,
        -999_999_983
    );
}

#[test]
fn native_directory_callback_paginates_and_checks_verifier() {
    use block2::RcBlock;
    use objc2::{AnyThread, DefinedClass, define_class, msg_send, rc::Retained};
    use objc2_foundation::{NSData, NSError, NSObjectProtocol};
    use objc2_fs_kit::{
        FSDirectoryEntryPacker, FSFileName, FSItemAttributes, FSItemID, FSItemType,
        FSVolumeIdentifier, FSVolumeOperations,
    };
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Packed(Mutex<Vec<(Vec<u8>, u64)>>);
    define_class!(
        #[unsafe(super(FSDirectoryEntryPacker))]
        #[ivars = Packed]
        struct TestPacker;
        unsafe impl NSObjectProtocol for TestPacker {}
        impl TestPacker {
            #[unsafe(method(packEntryWithName:itemType:itemID:nextCookie:attributes:))]
            fn pack(&self, name: &FSFileName, _kind: FSItemType, _id: FSItemID, cookie: u64, _attrs: Option<&FSItemAttributes>) -> bool {
                let data = unsafe { name.data() };
                self.ivars().0.lock().unwrap().push((unsafe { data.as_bytes_unchecked() }.to_vec(), cookie));
                true
            }
        }
    );
    impl TestPacker {
        fn new() -> Retained<Self> {
            unsafe { msg_send![super(Self::alloc().set_ivars(Packed::default())), init] }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let mut fs = xetfs::FileSystem::new(Repository::init(&repo).unwrap());
    fs.create(
        1,
        std::ffi::OsStr::new("a"),
        0o600,
        unsafe { libc::getuid() },
        unsafe { libc::getgid() },
    )
    .unwrap();
    drop(fs);
    let id = unsafe { FSVolumeIdentifier::new() };
    let name =
        unsafe { FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"XetFS")) };
    let volume = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let root = xetfs_fskit::XetItem::new(1);
    let packer = TestPacker::new();
    let result = Arc::new(Mutex::new(None));
    let capture = result.clone();
    let reply = RcBlock::new(move |verifier: u64, error: *mut NSError| {
        *capture.lock().unwrap() = Some((verifier, error.is_null()));
    });
    unsafe {
        volume.enumerateDirectory_startingAtCookie_verifier_providingAttributes_usingPacker_replyHandler(&root, 0, 0, None, &packer, &reply)
    };
    let (verifier, ok) = result.lock().unwrap().take().unwrap();
    assert!(ok && verifier != 0);
    assert_eq!(
        *packer.ivars().0.lock().unwrap(),
        vec![(b".".to_vec(), 1), (b"..".to_vec(), 2), (b"a".to_vec(), 3)]
    );
    unsafe {
        volume.enumerateDirectory_startingAtCookie_verifier_providingAttributes_usingPacker_replyHandler(&root, 1, verifier.wrapping_add(1), None, &packer, &reply)
    };
    assert!(!result.lock().unwrap().take().unwrap().1);
    let invalid = Arc::new(Mutex::new(None));
    let captured = invalid.clone();
    let bad_reply = RcBlock::new(move |_: u64, error: *mut NSError| {
        assert!(!error.is_null());
        *captured.lock().unwrap() =
            Some(unsafe { ((*error).domain().to_string(), (*error).code()) });
    });
    unsafe {
        volume.enumerateDirectory_startingAtCookie_verifier_providingAttributes_usingPacker_replyHandler(&root, 999, verifier, None, &packer, &bad_reply)
    };
    assert_eq!(
        *invalid.lock().unwrap(),
        Some(("NSPOSIXErrorDomain".to_string(), 4506))
    );
}

#[test]
fn native_xattrs_round_trip_and_enforce_create_replace_policy() {
    use block2::RcBlock;
    use objc2::{AnyThread, rc::Retained};
    use objc2_foundation::{NSArray, NSData, NSError};
    use objc2_fs_kit::{FSFileName, FSSetXattrPolicy, FSVolumeIdentifier, FSVolumeXattrOperations};
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let mut fs = xetfs::FileSystem::new(Repository::init(&repo).unwrap());
    let n = fs
        .create(
            1,
            std::ffi::OsStr::new("file"),
            0o600,
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
        .unwrap();
    drop(fs);
    let id = unsafe { FSVolumeIdentifier::new() };
    let name = unsafe {
        FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"com.apple.test"))
    };
    let v = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let item = xetfs_fskit::XetItem::new(n.ino);
    let status = Arc::new(Mutex::new(None));
    let capture = status.clone();
    let set_reply = RcBlock::new(move |error: *mut NSError| {
        *capture.lock().unwrap() = Some(if error.is_null() {
            0
        } else {
            unsafe { (*error).code() as i32 }
        });
    });
    unsafe {
        v.setXattrNamed_toData_onItem_policy_replyHandler(
            &name,
            Some(&NSData::with_bytes(b"payload")),
            &item,
            FSSetXattrPolicy::MustCreate,
            &set_reply,
        )
    };
    assert_eq!(status.lock().unwrap().take(), Some(0));
    unsafe {
        v.setXattrNamed_toData_onItem_policy_replyHandler(
            &name,
            Some(&NSData::with_bytes(b"other")),
            &item,
            FSSetXattrPolicy::MustCreate,
            &set_reply,
        )
    };
    assert_eq!(status.lock().unwrap().take(), Some(libc::EEXIST));
    let value: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let capture = value.clone();
    let get_reply = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
        assert!(error.is_null());
        *capture.lock().unwrap() = Some(unsafe { (*data).as_bytes_unchecked() }.to_vec());
    });
    unsafe { v.getXattrNamed_ofItem_replyHandler(&name, &item, &get_reply) };
    assert_eq!(value.lock().unwrap().take().unwrap(), b"payload");
    let names: Arc<Mutex<Option<Retained<NSArray<FSFileName>>>>> = Arc::new(Mutex::new(None));
    let capture = names.clone();
    let list_reply = RcBlock::new(
        move |array: *mut NSArray<FSFileName>, error: *mut NSError| {
            assert!(error.is_null());
            *capture.lock().unwrap() = unsafe { Retained::retain(array) };
        },
    );
    unsafe { v.listXattrsOfItem_replyHandler(&item, &list_reply) };
    assert_eq!(names.lock().unwrap().as_ref().unwrap().len(), 1);
    unsafe {
        v.setXattrNamed_toData_onItem_policy_replyHandler(
            &name,
            None,
            &item,
            FSSetXattrPolicy::Delete,
            &set_reply,
        )
    };
    assert_eq!(status.lock().unwrap().take(), Some(0));
    unsafe {
        v.setXattrNamed_toData_onItem_policy_replyHandler(
            &name,
            None,
            &item,
            FSSetXattrPolicy::Delete,
            &set_reply,
        )
    };
    assert_eq!(status.lock().unwrap().take(), Some(libc::ENOATTR));
}

#[test]
fn native_probe_identifier_survives_repository_rename() {
    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2_foundation::{NSError, NSString, NSURL};
    use objc2_fs_kit::{FSPathURLResource, FSProbeResult, FSUnaryFileSystemOperations};
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    drop(Repository::init(&first).unwrap());
    let fs = xetfs_fskit::XetUnary::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let reply = RcBlock::new(move |probe: *mut FSProbeResult, err: *mut NSError| {
        assert!(err.is_null());
        let id = unsafe {
            (*probe)
                .containerID()
                .unwrap()
                .uuid()
                .UUIDString()
                .to_string()
        };
        capture.lock().unwrap().push(id);
    });
    let probe = |path: &std::path::Path| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        let res = unsafe {
            FSPathURLResource::initWithURL_writable(FSPathURLResource::alloc(), &url, true)
        };
        unsafe { fs.probeResource_replyHandler(&res, &reply) };
    };
    probe(&first);
    std::fs::rename(&first, &second).unwrap();
    probe(&second);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);
}

#[test]
fn native_volume_statistics_use_backing_filesystem_allocation_units() {
    use objc2::AnyThread;
    use objc2_foundation::NSData;
    use objc2_fs_kit::{FSFileName, FSVolumeIdentifier, FSVolumeOperations};
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    drop(Repository::init(&repo).unwrap());
    let id = unsafe { FSVolumeIdentifier::new() };
    let name =
        unsafe { FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"XetFS")) };
    let volume = xetfs_fskit::XetNativeVolume::new(&repo, &id, &name).unwrap();
    let path = std::ffi::CString::new(repo.as_os_str().as_bytes()).unwrap();
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    assert_eq!(unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) }, 0);
    let st = unsafe { st.assume_init() };
    let stats = unsafe { volume.volumeStatistics() };
    let unit = if st.f_frsize == 0 {
        st.f_bsize
    } else {
        st.f_frsize
    };
    assert_eq!(unsafe { stats.blockSize() }, unit as isize);
    assert_eq!(unsafe { stats.totalBlocks() }, st.f_blocks as u64);
}
