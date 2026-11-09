use super::XetFuse;
use fuser::{Session, SessionACL};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CString, c_char, c_void},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixDatagram},
    },
    path::Path,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[link(name = "MFMount", kind = "framework")]
unsafe extern "C" {
    fn MFChannelCreate() -> *mut c_void;
    fn MFChannelClose(channel: *mut c_void) -> bool;
    fn MFRelease(object: *mut c_void);
    fn MFMount(
        channel: *mut c_void,
        path: *const c_char,
        options: *const c_char,
        quiet: bool,
    ) -> i32;
    fn MFChannelCopyNextMessage(channel: *mut c_void) -> *mut c_void;
    fn MFMessageGetBodyBuffers(message: *mut c_void, buffers: *mut *const libc::iovec) -> isize;
    fn MFMessageGetReplyBuffer(message: *mut c_void, buffer: *mut *mut c_void) -> isize;
    fn MFChannelSendMessage(
        channel: *mut c_void,
        buffers: *const libc::iovec,
        count: usize,
    ) -> isize;
}
struct Channel {
    raw: *mut c_void,
    closed: AtomicBool,
}
unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}
impl Channel {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            unsafe {
                MFChannelClose(self.raw);
            }
        }
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
        unsafe {
            MFRelease(self.raw);
        }
    }
}
struct Message(*mut c_void);
unsafe impl Send for Message {}
impl Drop for Message {
    fn drop(&mut self) {
        unsafe {
            MFRelease(self.0);
        }
    }
}
type Pending = Arc<Mutex<HashMap<u64, VecDeque<Message>>>>;

fn receive(channel: Arc<Channel>, socket: UnixDatagram, pending: Pending) -> io::Result<()> {
    loop {
        let raw = unsafe { MFChannelCopyNextMessage(channel.raw) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let message = Message(raw);
        let mut iov = ptr::null();
        let count = unsafe { MFMessageGetBodyBuffers(raw, &mut iov) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let mut body = Vec::new();
        for b in unsafe { std::slice::from_raw_parts(iov, count as usize) } {
            body.extend_from_slice(unsafe {
                std::slice::from_raw_parts(b.iov_base as *const u8, b.iov_len)
            });
        }
        if body.len() < 40
            || u32::from_ne_bytes(body[..4].try_into().unwrap()) as usize != body.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FUSE request length",
            ));
        }
        let opcode = u32::from_ne_bytes(body[4..8].try_into().unwrap());
        let id = u64::from_ne_bytes(body[8..16].try_into().unwrap());
        if opcode != 2 && opcode != 42 {
            pending
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .push_back(message);
        }
        socket.send(&body)?;
    }
}
fn reply(channel: Arc<Channel>, socket: UnixDatagram, pending: Pending) -> io::Result<()> {
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    loop {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut header, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if header.msg_flags & libc::MSG_TRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FUSE reply too large",
            ));
        }
        let n = n as usize;
        if n == 0 {
            return Ok(());
        }
        if n < 16 || u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize != n {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid FUSE reply length",
            ));
        }
        let id = u64::from_ne_bytes(buf[8..16].try_into().unwrap());
        let message = {
            let mut map = pending.lock().unwrap();
            let result = map.get_mut(&id).and_then(VecDeque::pop_front);
            if map.get(&id).is_some_and(VecDeque::is_empty) {
                map.remove(&id);
            }
            result
        };
        if let Some(message) = message {
            let mut reply_buf = ptr::null_mut();
            let cap = unsafe { MFMessageGetReplyBuffer(message.0, &mut reply_buf) };
            if cap < 0 {
                return Err(io::Error::last_os_error());
            }
            let error = i32::from_ne_bytes(buf[4..8].try_into().unwrap());
            if cap > 0 && error == 0 {
                let size = n - 16;
                if size > cap as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "reply exceeds shared buffer",
                    ));
                }
                unsafe {
                    ptr::copy_nonoverlapping(buf[16..n].as_ptr(), reply_buf.cast(), size);
                }
                buf[..4].copy_from_slice(&24u32.to_ne_bytes());
                buf[16..20].copy_from_slice(&(size as u32).to_ne_bytes());
                buf[20..24].fill(0);
                send(&channel, &buf[..24])?;
            } else {
                send(&channel, &buf[..n])?;
            }
        } else if id == 0 {
            send(&channel, &buf[..n])?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unmatched FUSE reply",
            ));
        }
    }
}
fn send(channel: &Channel, data: &[u8]) -> io::Result<()> {
    // macFUSE expects the FUSE header and payload in separate buffers.
    let iov = [
        libc::iovec {
            iov_base: data.as_ptr() as *mut c_void,
            iov_len: 16,
        },
        libc::iovec {
            iov_base: unsafe { data.as_ptr().add(16) } as *mut c_void,
            iov_len: data.len() - 16,
        },
    ];
    let count = if data.len() == 16 { 1 } else { 2 };
    let n = unsafe { MFChannelSendMessage(channel.raw, iov.as_ptr(), count) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else if n as usize != data.len() {
        Err(io::Error::new(io::ErrorKind::WriteZero, "short FUSE reply"))
    } else {
        Ok(())
    }
}
pub(super) fn mount(fs: XetFuse, path: &Path) -> anyhow::Result<()> {
    let raw = unsafe { MFChannelCreate() };
    anyhow::ensure!(!raw.is_null(), "MFChannelCreate failed");
    let channel = Arc::new(Channel {
        raw,
        closed: AtomicBool::new(false),
    });
    let (fuse, bridge) = UnixDatagram::pair()?;
    for socket in [&fuse, &bridge] {
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            let size: libc::c_int = 2 * 1024 * 1024;
            let rc = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    &size as *const _ as *const c_void,
                    std::mem::size_of_val(&size) as _,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
    }
    let session_stop = fuse.try_clone()?;
    let session_stop_mount = fuse.try_clone()?;
    let session_stop_writer = fuse.try_clone()?;
    let fd = unsafe { OwnedFd::from_raw_fd(fuse.into_raw_fd()) };
    let mut session = Session::from_fd(fs, fd, SessionACL::RootAndOwner);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let reader = bridge.try_clone()?;
    let writer = bridge.try_clone()?;
    let (r_channel, w_channel) = (channel.clone(), channel.clone());
    let (r_pending, w_pending) = (pending.clone(), pending);
    let reader_stop = bridge.try_clone()?;
    let reader_thread = thread::spawn(move || {
        let result = receive(r_channel, reader, r_pending);
        if let Err(ref e) = result
            && e.raw_os_error() != Some(libc::ENODEV)
        {
            eprintln!("macFUSE receive: {e}");
        }
        let _ = reader_stop.shutdown(std::net::Shutdown::Both);
        let _ = session_stop.shutdown(std::net::Shutdown::Both);
        result
    });
    let writer_stop = bridge.try_clone()?;
    let writer_thread = thread::spawn(move || {
        let result = reply(w_channel, writer, w_pending);
        if let Err(ref e) = result {
            eprintln!("macFUSE reply: {e}");
        }
        let _ = writer_stop.shutdown(std::net::Shutdown::Both);
        let _ = session_stop_writer.shutdown(std::net::Shutdown::Both);
        result
    });
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mount_channel = channel.clone();
    let mount_stop = bridge.try_clone()?;
    let mount = thread::spawn(move || {
        let result = unsafe {
            MFMount(
                mount_channel.raw,
                path.as_ptr(),
                c"fsname=xetfs,volname=XetFS".as_ptr(),
                true,
            )
        };
        if result != 0 {
            let _ = mount_stop.shutdown(std::net::Shutdown::Both);
            let _ = session_stop_mount.shutdown(std::net::Shutdown::Both);
        }
        result
    });
    let result = session.run();
    channel.close();
    let _ = bridge.shutdown(std::net::Shutdown::Both);
    let mount_result = mount
        .join()
        .map_err(|_| anyhow::anyhow!("macFUSE mount thread failed"))?;
    let receive_result = reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("macFUSE receive thread failed"))?;
    let reply_result = writer_thread
        .join()
        .map_err(|_| anyhow::anyhow!("macFUSE reply thread failed"))?;
    anyhow::ensure!(mount_result == 0, "macFUSE mount failed: {mount_result}");
    result?;
    if let Err(error) = receive_result
        && error.raw_os_error() != Some(libc::ENODEV)
    {
        return Err(error.into());
    }
    reply_result?;
    Ok(())
}
