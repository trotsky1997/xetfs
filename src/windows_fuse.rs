//! Windows mount adapter over WinFsp.
use crate::{Error, FileSystem, Kind, Node, SetAttr};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use winfsp_wrs::{
    CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights, FileAttributes,
    FileInfo, FileSystem as WinFileSystem, FileSystemInterface, NTSTATUS, PSecurityDescriptor,
    Params, STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_END_OF_FILE,
    STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNSUCCESSFUL, SecurityDescriptor, U16CStr, U16CString,
    VolumeInfo, VolumeParams, WriteMode, u16cstr, u16str,
};

struct Handle {
    ino: u64,
    file: Option<u64>,
}
struct Mount {
    fs: Mutex<FileSystem>,
    security: SecurityDescriptor,
    total: u64,
    free: u64,
}

fn status(error: Error) -> NTSTATUS {
    match error.errno() {
        libc::ENOENT => STATUS_OBJECT_NAME_NOT_FOUND,
        libc::EEXIST => STATUS_OBJECT_NAME_COLLISION,
        libc::ENOTEMPTY => STATUS_DIRECTORY_NOT_EMPTY,
        libc::ENOTDIR => STATUS_NOT_A_DIRECTORY,
        libc::EACCES | libc::EPERM => STATUS_ACCESS_DENIED,
        libc::EINVAL => STATUS_INVALID_PARAMETER,
        _ => STATUS_UNSUCCESSFUL,
    }
}
fn path(name: &U16CStr) -> Result<String, NTSTATUS> {
    let value = name.to_string().map_err(|_| STATUS_INVALID_PARAMETER)?;
    Ok(value.replace('\\', "/"))
}
fn attributes(node: &Node) -> FileAttributes {
    match node.kind {
        Kind::Directory => FileAttributes::DIRECTORY,
        _ => FileAttributes::NORMAL,
    }
}
fn time(ns: i64) -> u64 {
    (i128::from(ns) / 100 + 116_444_736_000_000_000).clamp(0, u64::MAX as i128) as u64
}
fn info(node: &Node) -> FileInfo {
    let mut result = FileInfo::default();
    result
        .set_file_attributes(attributes(node))
        .set_file_size(node.size)
        .set_allocation_size(node.size.div_ceil(4096) * 4096)
        .set_creation_time(time(node.birthtime_ns))
        .set_last_access_time(time(node.atime_ns))
        .set_last_write_time(time(node.mtime_ns))
        .set_change_time(time(node.ctime_ns))
        .set_index_number(node.ino)
        .set_hard_links(node.nlink as u32);
    result
}
impl Mount {
    fn open_node(fs: &mut FileSystem, node: Node) -> Result<(Arc<Handle>, FileInfo), NTSTATUS> {
        let file = if node.kind == Kind::File {
            Some(fs.open_handle(node.ino, libc::O_RDWR).map_err(status)?)
        } else {
            None
        };
        fs.retain_inode(node.ino, 1);
        Ok((
            Arc::new(Handle {
                ino: node.ino,
                file,
            }),
            info(&node),
        ))
    }
}
impl FileSystemInterface for Mount {
    type FileContext = Arc<Handle>;
    const GET_VOLUME_INFO_DEFINED: bool = true;
    fn get_volume_info(&self) -> Result<VolumeInfo, NTSTATUS> {
        VolumeInfo::new(self.total, self.free, u16str!("XetFS"))
            .map_err(|_| STATUS_INVALID_PARAMETER)
    }
    const GET_SECURITY_BY_NAME_DEFINED: bool = true;
    fn get_security_by_name(
        &self,
        name: &U16CStr,
        _: impl Fn() -> Option<FileAttributes>,
    ) -> Result<(FileAttributes, PSecurityDescriptor, bool), NTSTATUS> {
        let fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let node = fs.resolve_nofollow(path(name)?).map_err(status)?;
        Ok((attributes(&node), self.security.as_ptr(), false))
    }
    const GET_SECURITY_DEFINED: bool = true;
    fn get_security(&self, _: Self::FileContext) -> Result<PSecurityDescriptor, NTSTATUS> {
        Ok(self.security.as_ptr())
    }
    const OPEN_DEFINED: bool = true;
    fn open(
        &self,
        name: &U16CStr,
        _: CreateOptions,
        _: FileAccessRights,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        let mut fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let node = fs.resolve_nofollow(path(name)?).map_err(status)?;
        Self::open_node(&mut fs, node)
    }
    const CREATE_DEFINED: bool = true;
    fn create(
        &self,
        name: &U16CStr,
        options: CreateFileInfo,
        _: SecurityDescriptor,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        let mut fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let path = path(name)?;
        let (parent, name) = fs.path_parent(&path).map_err(status)?;
        let name = crate::platform::name_from_bytes(&name).map_err(status)?;
        let node = if options
            .create_options
            .is(CreateOptions::FILE_DIRECTORY_FILE)
        {
            fs.mkdir(parent, name, 0o755, 0, 0)
        } else {
            fs.create(parent, name, 0o644, 0, 0)
        }
        .map_err(status)?;
        Self::open_node(&mut fs, node)
    }
    const CLOSE_DEFINED: bool = true;
    fn close(&self, h: Self::FileContext) {
        if let Ok(mut fs) = self.fs.lock() {
            if let Some(file) = h.file {
                if let Err(e) = fs.close_handle(file) {
                    eprintln!("close: {e}");
                }
            }
            if let Err(e) = fs.forget_inode(h.ino, 1) {
                eprintln!("forget: {e}");
            }
        }
    }
    const GET_FILE_INFO_DEFINED: bool = true;
    fn get_file_info(&self, h: Self::FileContext) -> Result<FileInfo, NTSTATUS> {
        let fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        Ok(info(&fs.node(h.ino).map_err(status)?))
    }
    const READ_DEFINED: bool = true;
    fn read(
        &self,
        h: Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<usize, NTSTATUS> {
        let fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let data = fs
            .read_handle(h.file.ok_or(STATUS_ACCESS_DENIED)?, offset, buffer.len())
            .map_err(status)?;
        if data.is_empty() && !buffer.is_empty() {
            return Err(STATUS_END_OF_FILE);
        }
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
    const WRITE_DEFINED: bool = true;
    fn write(
        &self,
        h: Self::FileContext,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, FileInfo), NTSTATUS> {
        let mut fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let node = fs.node(h.ino).map_err(status)?;
        let (offset, len) = match mode {
            WriteMode::Normal { offset } => (offset, buffer.len()),
            WriteMode::WriteToEOF => (node.size, buffer.len()),
            WriteMode::ConstrainedIO { offset } => (
                offset,
                buffer.len().min(node.size.saturating_sub(offset) as usize),
            ),
        };
        let n = fs
            .write_handle(h.file.ok_or(STATUS_ACCESS_DENIED)?, offset, &buffer[..len])
            .map_err(status)?;
        Ok((n, info(&fs.node(h.ino).map_err(status)?)))
    }
    const FLUSH_DEFINED: bool = true;
    fn flush(&self, h: Self::FileContext) -> Result<FileInfo, NTSTATUS> {
        let mut fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        if let Some(file) = h.file {
            fs.flush_handle(file).map_err(status)?;
        }
        Ok(info(&fs.node(h.ino).map_err(status)?))
    }
    const SET_FILE_SIZE_DEFINED: bool = true;
    fn set_file_size(
        &self,
        h: Self::FileContext,
        size: u64,
        allocation: bool,
    ) -> Result<FileInfo, NTSTATUS> {
        let mut fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let old = fs.node(h.ino).map_err(status)?;
        let size = if allocation { old.size.min(size) } else { size };
        Ok(info(
            &fs.setattr(
                h.ino,
                SetAttr {
                    size: Some(size),
                    ..Default::default()
                },
            )
            .map_err(status)?,
        ))
    }
    const OVERWRITE_DEFINED: bool = true;
    fn overwrite(
        &self,
        h: Self::FileContext,
        _: FileAttributes,
        _: bool,
        _: u64,
    ) -> Result<FileInfo, NTSTATUS> {
        self.set_file_size(h, 0, false)
    }
    const READ_DIRECTORY_DEFINED: bool = true;
    fn read_directory(
        &self,
        h: Self::FileContext,
        marker: Option<&U16CStr>,
        mut add: impl FnMut(DirInfo) -> bool,
    ) -> Result<(), NTSTATUS> {
        let fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let marker = marker
            .map(|m| m.to_string())
            .transpose()
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        let mut entries = fs.list(h.ino).map_err(status)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            let name = std::str::from_utf8(&entry.name).map_err(|_| STATUS_INVALID_PARAMETER)?;
            if marker.as_ref().is_some_and(|m| name <= m.as_str()) {
                continue;
            }
            let name = U16CString::from_str(name).map_err(|_| STATUS_INVALID_PARAMETER)?;
            if !add(DirInfo::new(info(&entry.node), &name)) {
                break;
            }
        }
        Ok(())
    }
    const CAN_DELETE_DEFINED: bool = true;
    fn can_delete(&self, h: Self::FileContext, _: &U16CStr) -> Result<(), NTSTATUS> {
        let fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        if fs.node(h.ino).map_err(status)?.kind == Kind::Directory
            && !fs.list(h.ino).map_err(status)?.is_empty()
        {
            return Err(STATUS_DIRECTORY_NOT_EMPTY);
        }
        Ok(())
    }
    const CLEANUP_DEFINED: bool = true;
    fn cleanup(&self, h: Self::FileContext, name: Option<&U16CStr>, flags: CleanupFlags) {
        if flags.is(CleanupFlags::DELETE) {
            if let (Some(name), Ok(mut fs)) = (name, self.fs.lock()) {
                let result = (|| {
                    let name = path(name).map_err(|_| "invalid path".to_string())?;
                    let (parent, base) = fs.path_parent(&name).map_err(|e| e.to_string())?;
                    let base =
                        crate::platform::name_from_bytes(&base).map_err(|e| e.to_string())?;
                    let dir = fs.node(h.ino).map_err(|e| e.to_string())?.kind == Kind::Directory;
                    fs.unlink(parent, base, dir).map_err(|e| e.to_string())
                })();
                if let Err(e) = result {
                    eprintln!("cleanup: {e}");
                }
            }
        }
    }
    const RENAME_DEFINED: bool = true;
    fn rename(
        &self,
        _: Self::FileContext,
        old: &U16CStr,
        new: &U16CStr,
        replace: bool,
    ) -> Result<(), NTSTATUS> {
        let mut fs = self.fs.lock().map_err(|_| STATUS_UNSUCCESSFUL)?;
        let (p, a) = fs.path_parent(path(old)?).map_err(status)?;
        let (q, b) = fs.path_parent(path(new)?).map_err(status)?;
        fs.rename(
            p,
            crate::platform::name_from_bytes(&a).map_err(status)?,
            q,
            crate::platform::name_from_bytes(&b).map_err(status)?,
            !replace,
        )
        .map_err(status)
    }
}

fn user_security() -> anyhow::Result<SecurityDescriptor> {
    let output = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "whoami could not identify the user"
    );
    let text = String::from_utf8(output.stdout)?;
    let sid = text
        .split('"')
        .find(|part| part.starts_with("S-1-"))
        .ok_or_else(|| anyhow::anyhow!("user SID missing"))?;
    anyhow::ensure!(
        sid.chars()
            .all(|c| c.is_ascii_digit() || c == 'S' || c == '-'),
        "invalid user SID"
    );
    let sddl = U16CString::from_str(format!("O:{sid}G:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;SY)"))?;
    SecurityDescriptor::from_wstr(&sddl).map_err(|e| anyhow::anyhow!("security descriptor: {e}"))
}
pub fn mount(fs: FileSystem, mountpoint: &Path) -> anyhow::Result<()> {
    winfsp_wrs::init().map_err(|e| anyhow::anyhow!("WinFsp initialization: {e:?}"))?;
    let target = U16CString::from_os_str(mountpoint.as_os_str())?;
    let mut params = VolumeParams::default();
    params
        .set_sector_size(512)
        .set_sectors_per_allocation_unit(8)
        .set_case_sensitive_search(true)
        .set_case_preserved_names(true)
        .set_unicode_on_disk(true)
        .set_file_info_timeout(0)
        .set_file_system_name(u16cstr!("XetFS"))
        .map_err(|e| anyhow::anyhow!("volume name: {e:?}"))?;
    let total = fs2::total_space(fs.repository().path())?;
    let free = fs2::available_space(fs.repository().path())?;
    let context = Mount {
        fs: Mutex::new(fs),
        security: user_security()?,
        total,
        free,
    };
    let host = WinFileSystem::start(
        Params {
            volume_params: params,
            ..Default::default()
        },
        Some(&target),
        context,
    )
    .map_err(|e| anyhow::anyhow!("WinFsp mount: {e:?}"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })?;
    rx.recv()?;
    host.stop();
    Ok(())
}
