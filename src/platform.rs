use std::{ffi::OsStr, fs::File, io};

/// Persist directory metadata using the host's required directory-open flags.
pub fn sync_directory(path: &std::path::Path) -> crate::Result<()> {
    crate::repository::sync_dir(path)
}

#[cfg(unix)]
pub fn name_from_bytes(bytes: &[u8]) -> crate::Result<&OsStr> {
    use std::os::unix::ffi::OsStrExt;
    Ok(OsStr::from_bytes(bytes))
}

#[cfg(windows)]
pub fn name_from_bytes(bytes: &[u8]) -> crate::Result<&OsStr> {
    let text = std::str::from_utf8(bytes).map_err(|_| crate::Error::Errno(libc::EINVAL))?;
    Ok(OsStr::new(text))
}

#[cfg(unix)]
pub fn owner() -> (u32, u32) {
    unsafe { (libc::getuid(), libc::getgid()) }
}

#[cfg(windows)]
pub fn owner() -> (u32, u32) {
    (0, 0)
}

#[cfg(unix)]
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => {
                offset += n as u64;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
pub fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(unix)]
pub const ACCESS_MODE: i32 = libc::O_ACCMODE;
#[cfg(windows)]
pub const ACCESS_MODE: i32 = 3;
#[cfg(unix)]
pub const SYNC_FLAGS: i32 = libc::O_SYNC | libc::O_DSYNC;
#[cfg(windows)]
pub const SYNC_FLAGS: i32 = 0;
