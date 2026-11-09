//! XetFS state used by the macOS FSKit volume.
use std::{
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::{Mutex, MutexGuard},
};
use xetfs::{DirEntry, Error, FileSystem, Node, Repository, Result, SetAttr};

/// Serializes FSKit callbacks around the single repository owner.
pub struct Volume(
    Mutex<FileSystem>,
    Mutex<std::collections::HashMap<u64, objc2::rc::Retained<XetItem>>>,
    Option<ScopedUrl>,
);
struct ScopedUrl {
    url: objc2::rc::Retained<objc2_foundation::NSURL>,
    active: bool,
}
impl ScopedUrl {
    fn new(url: objc2::rc::Retained<objc2_foundation::NSURL>) -> Self {
        let active = unsafe { url.startAccessingSecurityScopedResource() };
        Self { url, active }
    }
}
impl Drop for ScopedUrl {
    fn drop(&mut self) {
        if self.active {
            unsafe { self.url.stopAccessingSecurityScopedResource() };
        }
    }
}

impl Volume {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self(
            Mutex::new(FileSystem::new(Repository::open(path)?)),
            Mutex::new(std::collections::HashMap::new()),
            None,
        ))
    }
    fn items(
        &self,
    ) -> Result<MutexGuard<'_, std::collections::HashMap<u64, objc2::rc::Retained<XetItem>>>> {
        self.1.lock().map_err(|_| Error::Errno(libc::EIO))
    }
    fn return_pinned<F>(&self, ino: u64, parent: u64, reply: F) -> Result<()>
    where
        F: FnOnce(&XetItem),
    {
        let mut items = self.items()?;
        let item = if let Some(item) = items.get(&ino) {
            self.fs()?.forget_inode(ino, 1)?;
            item.clone()
        } else {
            let item = XetItem::with_parent(ino, parent);
            items.insert(ino, item.clone());
            item
        };
        reply(&item);
        Ok(())
    }
    fn fs(&self) -> Result<MutexGuard<'_, FileSystem>> {
        self.0.lock().map_err(|_| Error::Errno(libc::EIO))
    }
    pub fn get_xattr(&self, ino: u64, name: &[u8]) -> Result<Vec<u8>> {
        self.fs()?.get_xattr(ino, name)
    }
    pub fn list_xattrs(&self, ino: u64) -> Result<Vec<Vec<u8>>> {
        self.fs()?.list_xattrs(ino)
    }
    pub fn set_xattr(
        &self,
        ino: u64,
        name: &[u8],
        value: Option<&[u8]>,
        policy: xetfs::XattrPolicy,
    ) -> Result<()> {
        self.fs()?.set_xattr(ino, name, value, policy)
    }
    pub fn item(&self, ino: u64) -> Result<Node> {
        self.fs()?.node(ino)
    }
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<Node> {
        self.fs()?.lookup(parent, OsStr::from_bytes(name))
    }
    pub fn lookup_pinned(&self, parent: u64, name: &[u8]) -> Result<Node> {
        let mut fs = self.fs()?;
        let node = fs.lookup(parent, OsStr::from_bytes(name))?;
        fs.retain_inode(node.ino, 1);
        Ok(node)
    }
    pub fn create_pinned(
        &self,
        parent: u64,
        name: &[u8],
        kind: xetfs::Kind,
        mode: u32,
        uid: u32,
        gid: u32,
        target: Option<&[u8]>,
        birthtime: Option<i64>,
        flags: u32,
    ) -> Result<Node> {
        let mut fs = self.fs()?;
        let name = OsStr::from_bytes(name);
        let node =
            fs.create_with_birthtime(parent, name, kind, mode, uid, gid, target, birthtime, flags)?;
        fs.retain_inode(node.ino, 1);
        Ok(node)
    }
    pub fn remove_checked(&self, parent: u64, name: &[u8], ino: u64) -> Result<()> {
        let mut fs = self.fs()?;
        let name = OsStr::from_bytes(name);
        let node = fs.lookup(parent, name)?;
        if node.ino != ino {
            return Err(Error::Errno(libc::ESTALE));
        }
        fs.unlink(parent, name, node.kind == xetfs::Kind::Directory)
    }
    pub fn rename_checked(
        &self,
        parent: u64,
        name: &[u8],
        ino: u64,
        target_parent: u64,
        target_name: &[u8],
        over: Option<u64>,
    ) -> Result<()> {
        let mut fs = self.fs()?;
        let name = OsStr::from_bytes(name);
        let target_name = OsStr::from_bytes(target_name);
        if fs.lookup(parent, name)?.ino != ino {
            return Err(Error::Errno(libc::ESTALE));
        }
        if parent == target_parent && name == target_name {
            return Ok(());
        }
        match (fs.lookup(target_parent, target_name), over) {
            (Ok(n), Some(expected)) if n.ino == expected => (),
            (Err(Error::Errno(libc::ENOENT)), None) => (),
            (Err(e), _) if e.errno() != libc::ENOENT => return Err(e),
            _ => return Err(Error::Errno(libc::ESTALE)),
        }
        fs.rename(parent, name, target_parent, target_name, false)
    }
    pub fn parent(&self, ino: u64) -> Result<u64> {
        self.fs()?.parent(ino)
    }
    pub fn retain(&self, ino: u64) -> Result<()> {
        let mut fs = self.fs()?;
        fs.node(ino)?;
        fs.retain_inode(ino, 1);
        Ok(())
    }
    pub fn reclaim(&self, ino: u64) -> Result<()> {
        self.fs()?.forget_inode(ino, 1)
    }
    pub fn list(&self, ino: u64) -> Result<Vec<DirEntry>> {
        self.fs()?.list(ino)
    }
    pub fn directory_snapshot(&self, ino: u64) -> Result<(u64, Vec<DirEntry>)> {
        use std::hash::{Hash, Hasher};
        let fs = self.fs()?;
        let parent = fs.parent(ino)?;
        let entries = fs.list(ino)?;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        ino.hash(&mut hash);
        parent.hash(&mut hash);
        for entry in &entries {
            entry.name.hash(&mut hash);
            entry.node.ino.hash(&mut hash);
        }
        Ok((hash.finish().max(1), entries))
    }
    pub fn create(&self, parent: u64, name: &[u8], mode: u32, uid: u32, gid: u32) -> Result<Node> {
        self.fs()?
            .create(parent, OsStr::from_bytes(name), mode, uid, gid)
    }
    pub fn mkdir(&self, parent: u64, name: &[u8], mode: u32, uid: u32, gid: u32) -> Result<Node> {
        self.fs()?
            .mkdir(parent, OsStr::from_bytes(name), mode, uid, gid)
    }
    pub fn symlink(
        &self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<Node> {
        self.fs()?
            .symlink(parent, OsStr::from_bytes(name), target, uid, gid)
    }
    pub fn link(&self, ino: u64, parent: u64, name: &[u8]) -> Result<Node> {
        self.fs()?.link(ino, parent, OsStr::from_bytes(name))
    }
    pub fn remove(&self, parent: u64, name: &[u8], directory: bool) -> Result<()> {
        self.fs()?
            .unlink(parent, OsStr::from_bytes(name), directory)
    }
    pub fn rename(&self, parent: u64, name: &[u8], new_parent: u64, new_name: &[u8]) -> Result<()> {
        self.fs()?.rename(
            parent,
            OsStr::from_bytes(name),
            new_parent,
            OsStr::from_bytes(new_name),
            false,
        )
    }
    pub fn setattr(&self, ino: u64, attrs: SetAttr) -> Result<Node> {
        self.fs()?.setattr(ino, attrs)
    }
    pub fn read_inode(&self, ino: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
        self.fs()?.read_inode(ino, offset, size)
    }
    pub fn write_inode(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let mut fs = self.fs()?;
        let handle = fs.open_handle(ino, libc::O_WRONLY)?;
        let result = fs.write_handle(handle, offset, data);
        let closed = fs.close_handle(handle);
        match result {
            Ok(n) => {
                closed?;
                Ok(n)
            }
            Err(error) => {
                let _ = closed;
                Err(error)
            }
        }
    }
    pub fn open_file(&self, ino: u64, flags: i32) -> Result<u64> {
        self.fs()?.open_handle(ino, flags)
    }
    pub fn read(&self, handle: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
        self.fs()?.read_handle(handle, offset, size)
    }
    pub fn write(&self, handle: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.fs()?.write_handle(handle, offset, data)
    }
    pub fn flush(&self, handle: u64) -> Result<()> {
        self.fs()?.flush_handle(handle)
    }
    pub fn close(&self, handle: u64) -> Result<()> {
        self.fs()?.close_handle(handle)
    }
    pub fn sync(&self) -> Result<()> {
        self.fs()?.sync_all()
    }
}

mod item {
    use objc2::{AnyThread, DefinedClass, define_class, msg_send, rc::Retained};
    use objc2_foundation::NSObjectProtocol;
    use objc2_fs_kit::FSItem;

    pub struct ItemIdentity {
        ino: u64,
        parent: std::sync::atomic::AtomicU64,
    }
    define_class!(
        #[unsafe(super(FSItem))]
        #[ivars = ItemIdentity]
        pub struct XetItem;

        unsafe impl NSObjectProtocol for XetItem {}
    );

    impl XetItem {
        pub fn new(ino: u64) -> Retained<Self> {
            Self::with_parent(ino, 1)
        }
        pub fn with_parent(ino: u64, parent: u64) -> Retained<Self> {
            let this = Self::alloc().set_ivars(ItemIdentity {
                ino,
                parent: std::sync::atomic::AtomicU64::new(parent),
            });
            unsafe { msg_send![super(this), init] }
        }
        pub fn inode(&self) -> u64 {
            self.ivars().ino
        }
        pub fn parent(&self) -> u64 {
            self.ivars()
                .parent
                .load(std::sync::atomic::Ordering::Relaxed)
        }
        pub fn set_parent(&self, parent: u64) {
            self.ivars()
                .parent
                .store(parent, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
pub use item::XetItem;

mod native_volume {
    use crate::{Volume, XetItem};
    use block2::DynBlock;
    use objc2::{AnyThread, DefinedClass, define_class, msg_send, rc::Retained};
    use objc2_foundation::{NSArray, NSData, NSError, NSObjectProtocol, NSString};
    use objc2_fs_kit::{
        FSDeactivateOptions, FSDirectoryCookie, FSDirectoryEntryPacker, FSDirectoryVerifier,
        FSErrorCode, FSFileName, FSItem, FSItemAttribute, FSItemAttributes,
        FSItemGetAttributesRequest, FSItemID, FSItemSetAttributesRequest, FSItemType,
        FSMutableFileDataBuffer, FSSetXattrPolicy, FSStatFSResult, FSSyncFlags, FSTaskOptions,
        FSVolume, FSVolumeIdentifier, FSVolumeOperations, FSVolumePathConfOperations,
        FSVolumeReadWriteOperations, FSVolumeSupportedCapabilities, FSVolumeXattrOperations,
        fs_errorForPOSIXError,
    };
    use std::{path::Path, ptr};

    define_class!(
        #[unsafe(super(FSVolume))]
        #[ivars = Volume]
        pub struct XetNativeVolume;
        unsafe impl NSObjectProtocol for XetNativeVolume {}
        #[allow(non_snake_case)]
        unsafe impl FSVolumeOperations for XetNativeVolume {
            #[unsafe(method_id(supportedVolumeCapabilities))]
            unsafe fn supportedVolumeCapabilities(
                &self,
            ) -> Retained<FSVolumeSupportedCapabilities> {
                let caps = unsafe { FSVolumeSupportedCapabilities::new() };
                unsafe {
                    caps.setSupportsPersistentObjectIDs(true);
                    caps.setSupportsSymbolicLinks(true);
                    caps.setSupportsHardLinks(true);
                    caps.setSupportsHiddenFiles(true);
                    caps.setDoesNotSupportImmutableFiles(true);
                    caps.setSupportsSparseFiles(true);
                    caps.setSupports2TBFiles(true);
                    caps.setSupports64BitObjectIDs(true);
                }
                caps
            }
            #[unsafe(method_id(volumeStatistics))]
            unsafe fn volumeStatistics(&self) -> Retained<FSStatFSResult> {
                let name = NSString::from_str("xetfs");
                let stats = unsafe {
                    FSStatFSResult::initWithFileSystemTypeName(FSStatFSResult::alloc(), &name)
                };
                if let Ok(fs) = self.ivars().fs() {
                    use std::os::unix::ffi::OsStrExt;
                    if let Ok(path) =
                        std::ffi::CString::new(fs.repository().path().as_os_str().as_bytes())
                    {
                        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
                        if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } == 0 {
                            let st = unsafe { st.assume_init() };
                            unsafe {
                                stats.setBlockSize(if st.f_frsize == 0 {
                                    st.f_bsize
                                } else {
                                    st.f_frsize
                                } as isize);
                                stats.setIoSize(st.f_bsize as isize);
                                stats.setTotalBlocks(st.f_blocks as u64);
                                stats.setFreeBlocks(st.f_bfree as u64);
                                stats.setAvailableBlocks(st.f_bavail as u64);
                                stats.setTotalFiles(st.f_files as u64);
                                stats.setFreeFiles(st.f_ffree as u64);
                            }
                        }
                    }
                }
                stats
            }
            #[unsafe(method(mountWithOptions:replyHandler:))]
            unsafe fn mountWithOptions_replyHandler(
                &self,
                _options: &FSTaskOptions,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                reply.call((ptr::null_mut(),));
            }
            #[unsafe(method(unmountWithReplyHandler:))]
            unsafe fn unmountWithReplyHandler(&self, reply: &DynBlock<dyn Fn()>) {
                if let Err(e) = self.ivars().sync() {
                    eprintln!("XetFS unmount sync: {e}");
                }
                reply.call(());
            }
            #[unsafe(method(deactivateWithOptions:replyHandler:))]
            unsafe fn deactivateWithOptions_replyHandler(
                &self,
                _options: FSDeactivateOptions,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                reply.call((ptr::null_mut(),));
            }
            #[unsafe(method(createLinkToItem:named:inDirectory:replyHandler:))]
            unsafe fn createLinkToItem_named_inDirectory_replyHandler(
                &self,
                item: &FSItem,
                name: &FSFileName,
                directory: &FSItem,
                reply: &DynBlock<dyn Fn(*mut FSFileName, *mut NSError)>,
            ) {
                let result = (|| {
                    let ino = identity(item)?.inode();
                    let parent = identity(directory)?.inode();
                    let bytes = unsafe { name.data() };
                    self.ivars()
                        .link(ino, parent, unsafe { bytes.as_bytes_unchecked() })?;
                    Ok(())
                })();
                match result {
                    Ok(()) => reply.call((name as *const _ as *mut _, ptr::null_mut())),
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(activateWithOptions:replyHandler:))]
            unsafe fn activateWithOptions_replyHandler(
                &self,
                _options: &FSTaskOptions,
                reply: &DynBlock<dyn Fn(*mut FSItem, *mut NSError)>,
            ) {
                match self.ivars().retain(1) {
                    Ok(()) => {
                        if let Err(e) = self.ivars().return_pinned(1, 1, |root| {
                            reply.call((root as *const _ as *mut FSItem, ptr::null_mut()))
                        }) {
                            reply_item_error(reply, e);
                        }
                    }
                    Err(e) => reply_item_error(reply, e),
                }
            }
            #[unsafe(method(lookupItemNamed:inDirectory:replyHandler:))]
            unsafe fn lookupItemNamed_inDirectory_replyHandler(
                &self,
                name: &FSFileName,
                directory: &FSItem,
                reply: &DynBlock<dyn Fn(*mut FSItem, *mut FSFileName, *mut NSError)>,
            ) {
                let result = (|| {
                    let parent = identity(directory)?.inode();
                    let data = unsafe { name.data() };
                    let node = self
                        .ivars()
                        .lookup_pinned(parent, unsafe { data.as_bytes_unchecked() })?;
                    Ok((node.ino, parent))
                })();
                reply_named_item(self.ivars(), reply, name, result);
            }
            #[unsafe(method(reclaimItem:replyHandler:))]
            unsafe fn reclaimItem_replyHandler(
                &self,
                item: &FSItem,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                let result = (|| {
                    let item = identity(item)?;
                    let mut items = self.ivars().items()?;
                    let failure = std::cell::RefCell::new(None);
                    let block = block2::RcBlock::new(|| {
                        if let Err(e) = self.ivars().reclaim(item.inode()) {
                            *failure.borrow_mut() = Some(e);
                        }
                    });
                    let reclaimed: bool = unsafe { msg_send![item, tryReclaimWithBlock: &*block] };
                    drop(block);
                    if let Some(error) = failure.into_inner() {
                        return Err(error);
                    }
                    if reclaimed {
                        items.remove(&item.inode());
                    }
                    Ok(())
                })();
                reply_empty(reply, result);
            }
            #[unsafe(method(getAttributes:ofItem:replyHandler:))]
            unsafe fn getAttributes_ofItem_replyHandler(
                &self,
                _request: &FSItemGetAttributesRequest,
                item: &FSItem,
                reply: &DynBlock<dyn Fn(*mut FSItemAttributes, *mut NSError)>,
            ) {
                let result = identity(item)
                    .and_then(|item| self.ivars().item(item.inode()).map(|node| (item, node)));
                match result {
                    Ok((item, node)) => {
                        let attr = node_attributes(&node, item.parent());
                        reply.call((Retained::as_ptr(&attr) as *mut _, ptr::null_mut()));
                    }
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(setAttributes:onItem:replyHandler:))]
            unsafe fn setAttributes_onItem_replyHandler(
                &self,
                request: &FSItemSetAttributesRequest,
                item: &FSItem,
                reply: &DynBlock<dyn Fn(*mut FSItemAttributes, *mut NSError)>,
            ) {
                let result = (|| {
                    let item = identity(item)?;
                    let unsupported = [
                        FSItemAttribute::Type,
                        FSItemAttribute::LinkCount,
                        FSItemAttribute::AllocSize,
                        FSItemAttribute::FileID,
                        FSItemAttribute::ParentID,
                        FSItemAttribute::ChangeTime,
                        FSItemAttribute::BackupTime,
                        FSItemAttribute::AddedTime,
                        FSItemAttribute::SupportsLimitedXAttrs,
                        FSItemAttribute::InhibitKernelOffloadedIO,
                    ];
                    if unsupported
                        .into_iter()
                        .any(|attr| unsafe { request.isValid(attr) })
                    {
                        return Err(xetfs::Error::Errno(libc::EOPNOTSUPP));
                    }
                    let mut consumed = FSItemAttribute::empty();
                    let mut changes = xetfs::SetAttr::default();
                    unsafe {
                        if request.isValid(FSItemAttribute::Flags) {
                            changes.flags = Some(request.flags());
                            consumed |= FSItemAttribute::Flags;
                        }
                        if request.isValid(FSItemAttribute::Mode) {
                            changes.mode = Some(request.mode());
                            consumed |= FSItemAttribute::Mode;
                        }
                        if request.isValid(FSItemAttribute::UID) {
                            changes.uid = Some(request.uid());
                            consumed |= FSItemAttribute::UID;
                        }
                        if request.isValid(FSItemAttribute::GID) {
                            changes.gid = Some(request.gid());
                            consumed |= FSItemAttribute::GID;
                        }
                        if request.isValid(FSItemAttribute::Size) {
                            changes.size = Some(request.size());
                            consumed |= FSItemAttribute::Size;
                        }
                        if request.isValid(FSItemAttribute::AccessTime) {
                            let time: DarwinTimespec = msg_send![request, accessTime];
                            changes.atime_ns = Some(time.to_ns()?);
                            consumed |= FSItemAttribute::AccessTime;
                        }
                        if request.isValid(FSItemAttribute::ModifyTime) {
                            let time: DarwinTimespec = msg_send![request, modifyTime];
                            changes.mtime_ns = Some(time.to_ns()?);
                            consumed |= FSItemAttribute::ModifyTime;
                        }
                        if request.isValid(FSItemAttribute::BirthTime) {
                            let time: DarwinTimespec = msg_send![request, birthTime];
                            changes.birthtime_ns = Some(time.to_ns()?);
                            consumed |= FSItemAttribute::BirthTime;
                        }
                    }
                    let node = self.ivars().setattr(item.inode(), changes)?;
                    unsafe {
                        request.setConsumedAttributes(consumed);
                    }
                    Ok(node_attributes(&node, item.parent()))
                })();
                match result {
                    Ok(attr) => reply.call((Retained::as_ptr(&attr) as *mut _, ptr::null_mut())),
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(createItemNamed:type:inDirectory:attributes:replyHandler:))]
            unsafe fn createItemNamed_type_inDirectory_attributes_replyHandler(
                &self,
                name: &FSFileName,
                kind: FSItemType,
                directory: &FSItem,
                attrs: &FSItemSetAttributesRequest,
                reply: &DynBlock<dyn Fn(*mut FSItem, *mut FSFileName, *mut NSError)>,
            ) {
                let result = (|| {
                    let parent = identity(directory)?.inode();
                    let kind = if kind == FSItemType::File {
                        xetfs::Kind::File
                    } else if kind == FSItemType::Directory {
                        xetfs::Kind::Directory
                    } else {
                        return Err(xetfs::Error::Errno(libc::EOPNOTSUPP));
                    };
                    if unsafe {
                        !attrs.isValid(FSItemAttribute::Mode)
                            || !attrs.isValid(FSItemAttribute::UID)
                            || !attrs.isValid(FSItemAttribute::GID)
                    } {
                        return Err(xetfs::Error::Errno(libc::EINVAL));
                    }
                    validate_create_attributes(attrs, false)?;
                    let birthtime = requested_birthtime(attrs)?;
                    let flags = if unsafe { attrs.isValid(FSItemAttribute::Flags) } {
                        unsafe { attrs.flags() }
                    } else {
                        0
                    };
                    let (mode, uid, gid) = unsafe { (attrs.mode(), attrs.uid(), attrs.gid()) };
                    let data = unsafe { name.data() };
                    let node = self.ivars().create_pinned(
                        parent,
                        unsafe { data.as_bytes_unchecked() },
                        kind,
                        mode,
                        uid,
                        gid,
                        None,
                        birthtime,
                        flags,
                    )?;
                    let mut consumed =
                        FSItemAttribute::Mode | FSItemAttribute::UID | FSItemAttribute::GID;
                    if unsafe { attrs.isValid(FSItemAttribute::Flags) } {
                        consumed |= FSItemAttribute::Flags;
                    }
                    if birthtime.is_some() {
                        consumed |= FSItemAttribute::BirthTime;
                    }
                    unsafe {
                        attrs.setConsumedAttributes(consumed);
                    }
                    Ok((node.ino, parent))
                })();
                reply_named_item(self.ivars(), reply, name, result);
            }
            #[unsafe(method(createSymbolicLinkNamed:inDirectory:attributes:linkContents:replyHandler:))]
            unsafe fn createSymbolicLinkNamed_inDirectory_attributes_linkContents_replyHandler(
                &self,
                name: &FSFileName,
                directory: &FSItem,
                attrs: &FSItemSetAttributesRequest,
                target: &FSFileName,
                reply: &DynBlock<dyn Fn(*mut FSItem, *mut FSFileName, *mut NSError)>,
            ) {
                let result = (|| {
                    let parent = identity(directory)?.inode();
                    if unsafe {
                        !attrs.isValid(FSItemAttribute::UID) || !attrs.isValid(FSItemAttribute::GID)
                    } {
                        return Err(xetfs::Error::Errno(libc::EINVAL));
                    }
                    validate_create_attributes(attrs, true)?;
                    let birthtime = requested_birthtime(attrs)?;
                    let flags = if unsafe { attrs.isValid(FSItemAttribute::Flags) } {
                        unsafe { attrs.flags() }
                    } else {
                        0
                    };
                    let (uid, gid) = unsafe { (attrs.uid(), attrs.gid()) };
                    let name_data = unsafe { name.data() };
                    let target_data = unsafe { target.data() };
                    let node = self.ivars().create_pinned(
                        parent,
                        unsafe { name_data.as_bytes_unchecked() },
                        xetfs::Kind::Symlink,
                        0o777,
                        uid,
                        gid,
                        Some(unsafe { target_data.as_bytes_unchecked() }),
                        birthtime,
                        flags,
                    )?;
                    let mut consumed = FSItemAttribute::UID | FSItemAttribute::GID;
                    if unsafe { attrs.isValid(FSItemAttribute::Mode) } {
                        consumed |= FSItemAttribute::Mode;
                    }
                    if unsafe { attrs.isValid(FSItemAttribute::Flags) } {
                        consumed |= FSItemAttribute::Flags;
                    }
                    if birthtime.is_some() {
                        consumed |= FSItemAttribute::BirthTime;
                    }
                    unsafe {
                        attrs.setConsumedAttributes(consumed);
                    }
                    Ok((node.ino, parent))
                })();
                reply_named_item(self.ivars(), reply, name, result);
            }
            #[unsafe(method(removeItem:named:fromDirectory:replyHandler:))]
            unsafe fn removeItem_named_fromDirectory_replyHandler(
                &self,
                item: &FSItem,
                name: &FSFileName,
                directory: &FSItem,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                let result = (|| {
                    let ino = identity(item)?.inode();
                    let parent = identity(directory)?.inode();
                    let data = unsafe { name.data() };
                    self.ivars()
                        .remove_checked(parent, unsafe { data.as_bytes_unchecked() }, ino)
                })();
                reply_empty(reply, result);
            }
            #[unsafe(method(renameItem:inDirectory:named:toNewName:inDirectory:overItem:replyHandler:))]
            unsafe fn renameItem_inDirectory_named_toNewName_inDirectory_overItem_replyHandler(
                &self,
                item: &FSItem,
                source: &FSItem,
                name: &FSFileName,
                destination_name: &FSFileName,
                destination: &FSItem,
                over: Option<&FSItem>,
                reply: &DynBlock<dyn Fn(*mut FSFileName, *mut NSError)>,
            ) {
                let result = (|| {
                    let ino = identity(item)?.inode();
                    let src = identity(source)?.inode();
                    let dest = identity(destination)?.inode();
                    let over = over.map(identity).transpose()?.map(|item| item.inode());
                    let old = unsafe { name.data() };
                    let new = unsafe { destination_name.data() };
                    self.ivars().rename_checked(
                        src,
                        unsafe { old.as_bytes_unchecked() },
                        ino,
                        dest,
                        unsafe { new.as_bytes_unchecked() },
                        over,
                    )?;
                    identity(item)?.set_parent(dest);
                    Ok(())
                })();
                match result {
                    Ok(()) => reply.call((destination_name as *const _ as *mut _, ptr::null_mut())),
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(readSymbolicLink:replyHandler:))]
            unsafe fn readSymbolicLink_replyHandler(
                &self,
                item: &FSItem,
                reply: &DynBlock<dyn Fn(*mut FSFileName, *mut NSError)>,
            ) {
                let result = identity(item)
                    .and_then(|item| self.ivars().item(item.inode()))
                    .and_then(|node| node.target.ok_or(xetfs::Error::Errno(libc::EINVAL)));
                match result {
                    Ok(target) => {
                        let data = NSData::with_bytes(&target);
                        let name = unsafe { FSFileName::initWithData(FSFileName::alloc(), &data) };
                        reply.call((Retained::as_ptr(&name) as *mut _, ptr::null_mut()));
                    }
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(enumerateDirectory:startingAtCookie:verifier:providingAttributes:usingPacker:replyHandler:))]
            unsafe fn enumerateDirectory_startingAtCookie_verifier_providingAttributes_usingPacker_replyHandler(
                &self,
                directory: &FSItem,
                cookie: FSDirectoryCookie,
                verifier: FSDirectoryVerifier,
                requested: Option<&FSItemGetAttributesRequest>,
                packer: &FSDirectoryEntryPacker,
                reply: &DynBlock<dyn Fn(FSDirectoryVerifier, *mut NSError)>,
            ) {
                let result = (|| {
                    let ino = identity(directory)?.inode();
                    let (current, entries) = self.ivars().directory_snapshot(ino)?;
                    if cookie != 0 && verifier != current {
                        return Err(xetfs::Error::Errno(libc::ESTALE));
                    }
                    let index = usize::try_from(cookie).map_err(|_| {
                        xetfs::Error::Errno(FSErrorCode::InvalidDirectoryCookie.0 as i32)
                    })?;
                    let offset = if requested.is_none() { 2 } else { 0 };
                    let total = entries.len() + offset;
                    if index > total {
                        return Err(xetfs::Error::Errno(
                            FSErrorCode::InvalidDirectoryCookie.0 as i32,
                        ));
                    }
                    for position in index..total {
                        let (name, node) = if offset > 0 && position < 2 {
                            let parent = if position == 0 {
                                ino
                            } else {
                                self.ivars().parent(ino)?
                            };
                            (
                                if position == 0 {
                                    b".".as_slice()
                                } else {
                                    b"..".as_slice()
                                },
                                self.ivars().item(parent)?,
                            )
                        } else {
                            let entry = &entries[position - offset];
                            (entry.name.as_slice(), entry.node.clone())
                        };
                        let data = NSData::with_bytes(name);
                        let name = unsafe { FSFileName::initWithData(FSFileName::alloc(), &data) };
                        let attrs = requested.map(|_| node_attributes(&node, ino));
                        let packed = unsafe {
                            packer.packEntryWithName_itemType_itemID_nextCookie_attributes(
                                &name,
                                item_type(node.kind),
                                FSItemID(node.ino + 1),
                                (position + 1) as u64,
                                attrs.as_deref(),
                            )
                        };
                        if !packed {
                            break;
                        }
                    }
                    Ok(current)
                })();
                match result {
                    Ok(current) => reply.call((current, ptr::null_mut())),
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((0, Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(synchronizeWithFlags:replyHandler:))]
            unsafe fn synchronizeWithFlags_replyHandler(
                &self,
                _flags: FSSyncFlags,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                reply_empty(reply, self.ivars().sync());
            }
        }
        #[allow(non_snake_case)]
        unsafe impl FSVolumePathConfOperations for XetNativeVolume {
            #[unsafe(method(maximumLinkCount))]
            unsafe fn maximumLinkCount(&self) -> isize {
                i32::MAX as isize
            }
            #[unsafe(method(maximumNameLength))]
            unsafe fn maximumNameLength(&self) -> isize {
                255
            }
            #[unsafe(method(restrictsOwnershipChanges))]
            unsafe fn restrictsOwnershipChanges(&self) -> bool {
                true
            }
            #[unsafe(method(truncatesLongNames))]
            unsafe fn truncatesLongNames(&self) -> bool {
                false
            }
            #[unsafe(method(maximumXattrSize))]
            unsafe fn maximumXattrSize(&self) -> isize {
                1_000_000_000
            }
            #[unsafe(method(maximumFileSize))]
            unsafe fn maximumFileSize(&self) -> u64 {
                i64::MAX as u64
            }
        }
        #[allow(non_snake_case)]
        unsafe impl FSVolumeXattrOperations for XetNativeVolume {
            #[unsafe(method(getXattrNamed:ofItem:replyHandler:))]
            unsafe fn getXattrNamed_ofItem_replyHandler(
                &self,
                name: &FSFileName,
                item: &FSItem,
                reply: &DynBlock<dyn Fn(*mut NSData, *mut NSError)>,
            ) {
                let result = (|| {
                    let ino = identity(item)?.inode();
                    let name = unsafe { name.data() };
                    self.ivars()
                        .get_xattr(ino, unsafe { name.as_bytes_unchecked() })
                })();
                match result {
                    Ok(value) => {
                        let data = NSData::with_bytes(&value);
                        reply.call((Retained::as_ptr(&data) as *mut _, ptr::null_mut()));
                    }
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(setXattrNamed:toData:onItem:policy:replyHandler:))]
            unsafe fn setXattrNamed_toData_onItem_policy_replyHandler(
                &self,
                name: &FSFileName,
                value: Option<&NSData>,
                item: &FSItem,
                policy: FSSetXattrPolicy,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                let result = (|| {
                    let ino = identity(item)?.inode();
                    let name = unsafe { name.data() };
                    let policy = if policy == FSSetXattrPolicy::AlwaysSet {
                        xetfs::XattrPolicy::Set
                    } else if policy == FSSetXattrPolicy::MustCreate {
                        xetfs::XattrPolicy::Create
                    } else if policy == FSSetXattrPolicy::MustReplace {
                        xetfs::XattrPolicy::Replace
                    } else if policy == FSSetXattrPolicy::Delete {
                        xetfs::XattrPolicy::Remove
                    } else {
                        return Err(xetfs::Error::Errno(libc::EINVAL));
                    };
                    self.ivars().set_xattr(
                        ino,
                        unsafe { name.as_bytes_unchecked() },
                        value.map(|v| unsafe { v.as_bytes_unchecked() }),
                        policy,
                    )
                })();
                reply_empty(reply, result);
            }
            #[unsafe(method(listXattrsOfItem:replyHandler:))]
            unsafe fn listXattrsOfItem_replyHandler(
                &self,
                item: &FSItem,
                reply: &DynBlock<dyn Fn(*mut NSArray<FSFileName>, *mut NSError)>,
            ) {
                let result = identity(item).and_then(|item| self.ivars().list_xattrs(item.inode()));
                match result {
                    Ok(names) => {
                        let names: Vec<_> = names
                            .iter()
                            .map(|name| unsafe {
                                FSFileName::initWithData(
                                    FSFileName::alloc(),
                                    &NSData::with_bytes(name),
                                )
                            })
                            .collect();
                        let array = NSArray::from_retained_slice(&names);
                        reply.call((Retained::as_ptr(&array) as *mut _, ptr::null_mut()));
                    }
                    Err(e) => {
                        let err = posix_error(e);
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
        }
        #[allow(non_snake_case)]
        unsafe impl FSVolumeReadWriteOperations for XetNativeVolume {
            #[unsafe(method(readFromFile:offset:length:intoBuffer:replyHandler:))]
            unsafe fn readFromFile_offset_length_intoBuffer_replyHandler(
                &self,
                item: &FSItem,
                offset: libc::off_t,
                length: usize,
                buffer: &FSMutableFileDataBuffer,
                reply: &DynBlock<dyn Fn(usize, *mut NSError)>,
            ) {
                let result = (|| {
                    let ino = item
                        .downcast_ref::<XetItem>()
                        .ok_or(xetfs::Error::Errno(libc::EINVAL))?
                        .inode();
                    let offset =
                        u64::try_from(offset).map_err(|_| xetfs::Error::Errno(libc::EINVAL))?;
                    let length = length.min(unsafe { buffer.length() });
                    let data = self.ivars().read_inode(ino, offset, length)?;
                    unsafe {
                        ptr::copy_nonoverlapping(
                            data.as_ptr(),
                            buffer.mutableBytes().as_ptr().cast(),
                            data.len(),
                        );
                    }
                    Ok(data.len())
                })();
                reply_result(reply, result);
            }
            #[unsafe(method(writeContents:toFile:atOffset:replyHandler:))]
            unsafe fn writeContents_toFile_atOffset_replyHandler(
                &self,
                data: &NSData,
                item: &FSItem,
                offset: libc::off_t,
                reply: &DynBlock<dyn Fn(usize, *mut NSError)>,
            ) {
                let result = (|| {
                    let ino = item
                        .downcast_ref::<XetItem>()
                        .ok_or(xetfs::Error::Errno(libc::EINVAL))?
                        .inode();
                    let offset =
                        u64::try_from(offset).map_err(|_| xetfs::Error::Errno(libc::EINVAL))?;
                    self.ivars()
                        .write_inode(ino, offset, unsafe { data.as_bytes_unchecked() })
                })();
                reply_result(reply, result);
            }
        }
    );
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct DarwinTimespec {
        sec: i64,
        nsec: i64,
    }
    unsafe impl objc2::Encode for DarwinTimespec {
        const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
            "timespec",
            &[objc2::Encoding::LongLong, objc2::Encoding::LongLong],
        );
    }
    impl DarwinTimespec {
        fn from_ns(ns: i64) -> Self {
            Self {
                sec: ns.div_euclid(1_000_000_000),
                nsec: ns.rem_euclid(1_000_000_000),
            }
        }
        fn to_ns(self) -> xetfs::Result<i64> {
            if !(0..1_000_000_000).contains(&self.nsec) {
                return Err(xetfs::Error::Errno(libc::EINVAL));
            }
            i64::try_from(i128::from(self.sec) * 1_000_000_000 + i128::from(self.nsec))
                .map_err(|_| xetfs::Error::Errno(libc::EOVERFLOW))
        }
    }
    fn requested_birthtime(attrs: &FSItemSetAttributesRequest) -> xetfs::Result<Option<i64>> {
        if unsafe { attrs.isValid(FSItemAttribute::BirthTime) } {
            let time: DarwinTimespec = unsafe { msg_send![attrs, birthTime] };
            Ok(Some(time.to_ns()?))
        } else {
            Ok(None)
        }
    }
    fn validate_create_attributes(
        attrs: &FSItemSetAttributesRequest,
        symlink: bool,
    ) -> xetfs::Result<()> {
        let unsupported = [
            FSItemAttribute::Type,
            FSItemAttribute::LinkCount,
            FSItemAttribute::Size,
            FSItemAttribute::AllocSize,
            FSItemAttribute::FileID,
            FSItemAttribute::ParentID,
            FSItemAttribute::AccessTime,
            FSItemAttribute::ModifyTime,
            FSItemAttribute::ChangeTime,
            FSItemAttribute::BackupTime,
            FSItemAttribute::AddedTime,
            FSItemAttribute::SupportsLimitedXAttrs,
            FSItemAttribute::InhibitKernelOffloadedIO,
        ];
        if unsupported
            .into_iter()
            .any(|attr| unsafe { attrs.isValid(attr) })
        {
            return Err(xetfs::Error::Errno(libc::EOPNOTSUPP));
        }
        if symlink
            && unsafe { attrs.isValid(FSItemAttribute::Mode) && attrs.mode() & 0o7777 != 0o777 }
        {
            return Err(xetfs::Error::Errno(libc::EOPNOTSUPP));
        }
        Ok(())
    }
    fn item_type(kind: xetfs::Kind) -> FSItemType {
        match kind {
            xetfs::Kind::File => FSItemType::File,
            xetfs::Kind::Directory => FSItemType::Directory,
            xetfs::Kind::Symlink => FSItemType::Symlink,
        }
    }
    fn node_attributes(node: &xetfs::Node, parent: u64) -> Retained<FSItemAttributes> {
        let attr = unsafe { FSItemAttributes::new() };
        unsafe {
            attr.setUid(node.uid);
            attr.setGid(node.gid);
            attr.setMode(node.mode);
            attr.setFlags(node.flags);
            attr.setType(item_type(node.kind));
            attr.setLinkCount(node.nlink);
            attr.setSize(node.size);
            // shortcut: logical allocation estimate; report shared physical allocation when accounting is available.
            attr.setAllocSize(node.size);
            attr.setFileID(FSItemID(node.ino + 1));
            attr.setParentID(FSItemID(parent + 1));
            let _: () = msg_send![&*attr, setAccessTime: DarwinTimespec::from_ns(node.atime_ns)];
            let _: () = msg_send![&*attr, setModifyTime: DarwinTimespec::from_ns(node.mtime_ns)];
            let _: () = msg_send![&*attr, setChangeTime: DarwinTimespec::from_ns(node.ctime_ns)];
            let _: () = msg_send![&*attr, setBirthTime: DarwinTimespec::from_ns(node.birthtime_ns)];
        }
        attr
    }
    fn reply_named_item(
        volume: &Volume,
        reply: &DynBlock<dyn Fn(*mut FSItem, *mut FSFileName, *mut NSError)>,
        name: &FSFileName,
        result: xetfs::Result<(u64, u64)>,
    ) {
        let result = result.and_then(|(ino, parent)| {
            volume.return_pinned(ino, parent, |item| {
                reply.call((
                    item as *const _ as *mut _,
                    name as *const _ as *mut _,
                    ptr::null_mut(),
                ))
            })
        });
        match result {
            Ok(()) => (),
            Err(e) => {
                let err = posix_error(e);
                reply.call((
                    ptr::null_mut(),
                    ptr::null_mut(),
                    Retained::as_ptr(&err) as *mut _,
                ));
            }
        }
    }
    fn identity(item: &FSItem) -> xetfs::Result<&XetItem> {
        item.downcast_ref::<XetItem>()
            .ok_or(xetfs::Error::Errno(libc::EINVAL))
    }
    fn posix_error(e: xetfs::Error) -> Retained<NSError> {
        unsafe { fs_errorForPOSIXError(e.errno()) }
    }
    fn reply_empty(reply: &DynBlock<dyn Fn(*mut NSError)>, result: xetfs::Result<()>) {
        match result {
            Ok(()) => reply.call((ptr::null_mut(),)),
            Err(e) => {
                let err = posix_error(e);
                reply.call((Retained::as_ptr(&err) as *mut _,));
            }
        }
    }
    fn reply_item_error(reply: &DynBlock<dyn Fn(*mut FSItem, *mut NSError)>, e: xetfs::Error) {
        let err = posix_error(e);
        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
    }
    fn reply_result(reply: &DynBlock<dyn Fn(usize, *mut NSError)>, result: xetfs::Result<usize>) {
        match result {
            Ok(n) => reply.call((n, ptr::null_mut())),
            Err(e) => {
                let error = unsafe { fs_errorForPOSIXError(e.errno()) };
                reply.call((0, Retained::as_ptr(&error) as *mut NSError));
            }
        }
    }
    impl XetNativeVolume {
        pub fn new(
            path: &Path,
            id: &FSVolumeIdentifier,
            name: &FSFileName,
        ) -> xetfs::Result<Retained<Self>> {
            Self::new_with_scope(path, id, name, None)
        }
        pub(crate) fn new_with_scope(
            path: &Path,
            id: &FSVolumeIdentifier,
            name: &FSFileName,
            scope: Option<crate::ScopedUrl>,
        ) -> xetfs::Result<Retained<Self>> {
            let mut volume = Volume::open(path)?;
            volume.2 = scope;
            let this = Self::alloc().set_ivars(volume);
            Ok(unsafe { msg_send![super(this), initWithVolumeID: id, volumeName: name] })
        }
    }
}
pub use native_volume::XetNativeVolume;

mod unary {
    use crate::XetNativeVolume;
    use block2::DynBlock;
    use objc2::{AnyThread, define_class, msg_send, rc::Retained};
    use objc2_foundation::{NSData, NSError, NSObjectProtocol, NSString, NSUUID};
    use objc2_fs_kit::{
        FSContainerIdentifier, FSFileName, FSPathURLResource, FSProbeResult, FSResource,
        FSTaskOptions, FSUnaryFileSystem, FSUnaryFileSystemOperations, FSVolume,
        fs_errorForPOSIXError,
    };
    use std::{ffi::CStr, os::unix::ffi::OsStrExt, path::PathBuf, ptr};

    fn resource_path(resource: &FSResource) -> xetfs::Result<PathBuf> {
        let path = resource
            .downcast_ref::<FSPathURLResource>()
            .ok_or(xetfs::Error::Errno(libc::EINVAL))?;
        let url = unsafe { path.url() };
        if !url.isFileURL() {
            return Err(xetfs::Error::Errno(libc::EINVAL));
        }
        let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
        Ok(std::fs::canonicalize(std::ffi::OsStr::from_bytes(bytes))?)
    }
    fn identifier(path: &std::path::Path) -> xetfs::Result<Retained<FSContainerIdentifier>> {
        let bytes = xetfs::Repository::identifier(path)?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let uuid = format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        );
        let uuid = NSUUID::initWithUUIDString(NSUUID::alloc(), &NSString::from_str(&uuid))
            .ok_or(xetfs::Error::Errno(libc::EINVAL))?;
        Ok(unsafe { FSContainerIdentifier::initWithUUID(FSContainerIdentifier::alloc(), &uuid) })
    }
    define_class!(
        #[unsafe(super(FSUnaryFileSystem))]
        #[ivars = ()]
        pub struct XetUnary;
        unsafe impl NSObjectProtocol for XetUnary {}
        #[allow(non_snake_case)]
        unsafe impl FSUnaryFileSystemOperations for XetUnary {
            #[unsafe(method(probeResource:replyHandler:))]
            unsafe fn probeResource_replyHandler(
                &self,
                resource: &FSResource,
                reply: &DynBlock<dyn Fn(*mut FSProbeResult, *mut NSError)>,
            ) {
                let _scope = resource
                    .downcast_ref::<FSPathURLResource>()
                    .map(|r| crate::ScopedUrl::new(unsafe { r.url() }));
                let result = resource_path(resource).and_then(|path| {
                    if path.join("metadata.sqlite").is_file() && path.join("objects").is_dir() {
                        let id = identifier(&path)?;
                        Ok(unsafe {
                            FSProbeResult::usableProbeResultWithName_containerID(
                                &NSString::from_str("XetFS"),
                                &id,
                            )
                        })
                    } else {
                        Ok(unsafe { FSProbeResult::notRecognizedProbeResult() })
                    }
                });
                match result {
                    Ok(result) => {
                        reply.call((Retained::as_ptr(&result) as *mut _, ptr::null_mut()))
                    }
                    Err(e) => {
                        let err = unsafe { fs_errorForPOSIXError(e.errno()) };
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(loadResource:options:replyHandler:))]
            unsafe fn loadResource_options_replyHandler(
                &self,
                resource: &FSResource,
                options: &FSTaskOptions,
                reply: &DynBlock<dyn Fn(*mut FSVolume, *mut NSError)>,
            ) {
                let result = (|| {
                    let resource = resource
                        .downcast_ref::<FSPathURLResource>()
                        .ok_or(xetfs::Error::Errno(libc::EINVAL))?;
                    if !unsafe { resource.isWritable() } {
                        return Err(xetfs::Error::Errno(libc::EROFS));
                    }
                    let scope = crate::ScopedUrl::new(unsafe { resource.url() });
                    let opts: Option<Retained<objc2_foundation::NSArray<NSString>>> =
                        unsafe { msg_send![options, taskOptions] };
                    if opts
                        .as_ref()
                        .is_some_and(|opts| opts.iter().any(|s| s.to_string() == "--rdonly"))
                    {
                        return Err(xetfs::Error::Errno(libc::EOPNOTSUPP));
                    }
                    let path = resource_path(resource)?;
                    let id = identifier(&path)?;
                    let id = unsafe { id.volumeIdentifier() };
                    let name = unsafe {
                        FSFileName::initWithData(FSFileName::alloc(), &NSData::with_bytes(b"XetFS"))
                    };
                    XetNativeVolume::new_with_scope(&path, &id, &name, Some(scope))
                })();
                match result {
                    Ok(v) => reply.call((Retained::as_ptr(&v) as *mut _, ptr::null_mut())),
                    Err(e) => {
                        let err = unsafe { fs_errorForPOSIXError(e.errno()) };
                        reply.call((ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
                    }
                }
            }
            #[unsafe(method(unloadResource:options:replyHandler:))]
            unsafe fn unloadResource_options_replyHandler(
                &self,
                _resource: &FSResource,
                _options: &FSTaskOptions,
                reply: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                reply.call((ptr::null_mut(),));
            }
        }
    );
    impl XetUnary {
        pub fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }
    #[unsafe(no_mangle)]
    pub extern "C" fn xetfs_unary_new() -> *mut FSUnaryFileSystem {
        Retained::into_raw(XetUnary::new()).cast()
    }
}
pub use unary::XetUnary;
