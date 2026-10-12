#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("metadata: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("filesystem error: {0}")]
    Errno(i32),
    #[error("corrupt repository: {0}")]
    Corrupt(String),
    #[error("S3 backend: {0}")]
    Backend(String),
    #[error("repository is already open (including a mounted filesystem)")]
    Busy,
    #[error("{0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, Error>;
impl Error {
    pub fn errno(&self) -> i32 {
        match self {
            Self::Errno(n) => *n,
            Self::Busy => libc::EBUSY,
            Self::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            Self::Invalid(_) => libc::EINVAL,
            Self::Sql(_) | Self::Corrupt(_) | Self::Backend(_) => libc::EIO,
        }
    }
}
pub(crate) fn errno<T>(n: i32) -> Result<T> {
    Err(Error::Errno(n))
}
