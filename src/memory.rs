//! C ABI over native CAS. Callers own buffers; no payload temporary files.
use crate::{FileSystem, Kind, Repository};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_char, c_void},
    io::{self, Read},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    ptr,
    sync::{Arc, Mutex},
};

pub struct MemoryHandle(Arc<Mutex<FileSystem>>);
#[repr(C)]
pub struct MemoryInfo {
    pub size: u64,
    pub content: [u8; 65],
}
pub type ReadCallback = unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> i64;
thread_local! { static ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap()); }
fn failure(value: impl ToString) {
    ERROR.with(|s| *s.borrow_mut() = CString::new(value.to_string().replace('\0', "\\0")).unwrap());
}
fn guard(f: impl FnOnce() -> Result<()>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            failure(format!("{e:#}"));
            -1
        }
        Err(_) => {
            failure("native XetFS memory operation panicked");
            -1
        }
    }
}
unsafe fn text<'a>(value: *const c_char) -> Result<&'a str> {
    ensure!(!value.is_null(), "null string");
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .context("memory API paths must be UTF-8")
}
unsafe fn handle<'a>(value: *mut MemoryHandle) -> Result<&'a MemoryHandle> {
    unsafe { value.as_ref() }.context("closed XetFS memory repository")
}
unsafe fn info(out: *mut MemoryInfo, size: u64, content: &str) -> Result<()> {
    ensure!(
        !out.is_null() && content.len() == 64,
        "invalid information output"
    );
    let mut value = MemoryInfo {
        size,
        content: [0; 65],
    };
    value.content[..64].copy_from_slice(content.as_bytes());
    unsafe { out.write(value) };
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn xetfs_memory_abi() -> u32 {
    1
}
#[unsafe(no_mangle)]
pub extern "C" fn xetfs_memory_error() -> *const c_char {
    ERROR.with(|s| s.borrow().as_ptr())
}

/// # Safety
/// `root` must be a live NUL-terminated string and `out` a writable handle pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_open(
    root: *const c_char,
    create: bool,
    out: *mut *mut MemoryHandle,
) -> i32 {
    guard(|| {
        ensure!(!out.is_null(), "null handle output");
        unsafe { out.write(ptr::null_mut()) };
        let root = Path::new(unsafe { text(root)? });
        ensure!(
            root.is_absolute() && !root.is_symlink(),
            "repository must be an absolute ordinary path"
        );
        let repo = if create {
            Repository::init(root)?
        } else {
            Repository::open(root)?
        };
        unsafe {
            out.write(Box::into_raw(Box::new(MemoryHandle(Arc::new(Mutex::new(
                FileSystem::new(repo),
            ))))))
        };
        Ok(())
    })
}
/// # Safety
/// value must remain live through this call; out receives an independently owned handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_clone(
    value: *mut MemoryHandle,
    out: *mut *mut MemoryHandle,
) -> i32 {
    guard(|| {
        ensure!(!out.is_null(), "null handle output");
        unsafe {
            out.write(ptr::null_mut());
        }
        let source = unsafe { handle(value)? };
        unsafe {
            out.write(Box::into_raw(Box::new(MemoryHandle(source.0.clone()))));
        }
        Ok(())
    })
}
/// # Safety
/// A handle returned by open must be closed once, after all operations finish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_close(value: *mut MemoryHandle) {
    if !value.is_null() {
        drop(unsafe { Box::from_raw(value) });
    }
}

fn put(
    value: *mut MemoryHandle,
    path: *const c_char,
    input: impl Read,
    out: *mut MemoryInfo,
) -> Result<()> {
    ensure!(!out.is_null(), "null information output");
    let path = Path::new(unsafe { text(path)? });
    ensure!(path.is_absolute(), "virtual memory paths must be absolute");
    let mut fs = unsafe { handle(value)? }
        .0
        .lock()
        .map_err(|_| anyhow::anyhow!("repository lock poisoned"))?;
    fs.mkdir_all(path.parent().context("missing parent")?)?;
    let n = fs.put_file(path, input)?;
    unsafe {
        info(
            out,
            n.size,
            n.content.as_deref().context("missing file recipe")?,
        )
    }
}

/// # Safety
/// `data` must reference `length` stable readable bytes until this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_put(
    value: *mut MemoryHandle,
    path: *const c_char,
    data: *const u8,
    length: usize,
    out: *mut MemoryInfo,
) -> i32 {
    guard(|| {
        ensure!(
            length <= isize::MAX as usize && (length == 0 || !data.is_null()),
            "invalid input buffer"
        );
        let input = if length == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(data, length) }
        };
        put(value, path, input, out)
    })
}
struct CallbackReader {
    read: ReadCallback,
    user: *mut c_void,
    remaining: u64,
}
impl Read for CallbackReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || out.is_empty() {
            return Ok(0);
        }
        let capacity = (out.len() as u64).min(self.remaining) as usize;
        let n = unsafe { (self.read)(self.user, out.as_mut_ptr(), capacity) };
        if n <= 0 || n as u64 > capacity as u64 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "memory reader failed or returned a short stream",
            ));
        }
        self.remaining -= n as u64;
        Ok(n as usize)
    }
}
/// # Safety
/// Callback and user context must remain valid until the call returns; callback
/// must write at most the supplied capacity and return its actual byte count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_put_reader(
    value: *mut MemoryHandle,
    path: *const c_char,
    read: Option<ReadCallback>,
    user: *mut c_void,
    length: u64,
    out: *mut MemoryInfo,
) -> i32 {
    guard(|| {
        let read = read.context("missing memory reader")?;
        ensure!(length <= i64::MAX as u64, "stream too large");
        put(
            value,
            path,
            CallbackReader {
                read,
                user,
                remaining: length,
            },
            out,
        )
    })
}
/// # Safety
/// Handle/path must remain live and `out` must reference writable MemoryInfo.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_stat(
    value: *mut MemoryHandle,
    path: *const c_char,
    out: *mut MemoryInfo,
) -> i32 {
    guard(|| {
        let fs = unsafe { handle(value)? }
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("repository lock poisoned"))?;
        let n = fs.resolve(unsafe { text(path)? })?;
        ensure!(n.kind == Kind::File, "memory path must name a regular file");
        unsafe {
            info(
                out,
                n.size,
                n.content.as_deref().context("missing file recipe")?,
            )
        }
    })
}

/// # Safety
/// Paths must remain live; publishes a complete virtual directory without replacement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_publish(
    value: *mut MemoryHandle,
    source: *const c_char,
    destination: *const c_char,
) -> i32 {
    guard(|| {
        let mut fs = unsafe { handle(value)? }
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("repository lock poisoned"))?;
        let source = Path::new(unsafe { text(source)? });
        let destination = Path::new(unsafe { text(destination)? });
        ensure!(
            source.is_absolute() && destination.is_absolute(),
            "publication paths must be absolute"
        );
        let parent = fs.resolve(source.parent().context("missing source parent")?)?;
        let target = fs.mkdir_all(destination.parent().context("missing destination parent")?)?;
        fs.rename(
            parent.ino,
            source.file_name().context("missing source name")?,
            target.ino,
            destination
                .file_name()
                .context("missing destination name")?,
            true,
        )?;
        Ok(())
    })
}

/// # Safety
/// Handle/id must remain live and out must reference writable MemoryInfo.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_xorb_stat(
    value: *mut MemoryHandle,
    id: *const c_char,
    out: *mut MemoryInfo,
) -> i32 {
    guard(|| {
        let fs = unsafe { handle(value)? }
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("repository lock poisoned"))?;
        let id = unsafe { text(id)? };
        let object = fs.repository().read_upload_xorb(id)?;
        unsafe { info(out, object.bytes.len() as u64, id) }
    })
}

/// # Safety
/// The caller must finish DMA and close all writable handles before import.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_import_xorb(
    value: *mut MemoryHandle,
    source: *const c_char,
    id: *const c_char,
    out: *mut MemoryInfo,
) -> i32 {
    guard(|| {
        ensure!(!out.is_null(), "null information output");
        let mut fs = unsafe { handle(value)? }
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("repository lock poisoned"))?;
        let id = unsafe { text(id)? };
        let content = fs
            .cas_repository_mut()
            .import_direct_xorb(Path::new(unsafe { text(source)? }), id)?;
        fs.mkdir_all("/hub/cas/staged")?;
        let node = fs.pin_content(format!("/hub/cas/staged/{id}"), &content)?;
        unsafe { info(out, node.size, &content) }
    })
}
/// # Safety
/// Destination must reference `capacity` writable bytes; no alias may read or
/// mutate that memory until the call completes. On failure it may be partial.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xetfs_memory_read_into(
    value: *mut MemoryHandle,
    content: *const c_char,
    offset: u64,
    destination: *mut u8,
    capacity: usize,
    written: *mut u64,
) -> i32 {
    guard(|| {
        ensure!(
            !written.is_null()
                && capacity <= isize::MAX as usize
                && (capacity == 0 || !destination.is_null()),
            "invalid output buffer"
        );
        unsafe { written.write(0) };
        let fs = unsafe { handle(value)? }
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("repository lock poisoned"))?;
        let content = unsafe { text(content)? };
        crate::hash::from_hex(content)?;
        let size: u64 = fs.repository().conn.query_row(
            "SELECT size FROM contents WHERE id=?1",
            [content],
            |r| r.get(0),
        )?;
        let mut copied = 0usize;
        let amount = size.saturating_sub(offset).min(capacity as u64) as usize;
        while copied < amount {
            let data = fs.repository().read_content(
                content,
                offset + copied as u64,
                (amount - copied).min(1024 * 1024),
            )?;
            ensure!(!data.is_empty(), "incomplete memory reconstruction");
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), destination.add(copied), data.len()) };
            copied += data.len();
            unsafe { written.write(copied as u64) };
        }
        Ok(())
    })
}
