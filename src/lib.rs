//! Local CAS filesystem with Xet-compatible CDC, hashes and Xorb chunk records.
pub mod chunker;
mod error;
mod filesystem;
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "fuse"))]
pub mod fuse;
pub mod hash;
pub mod remote;
pub mod remote_cas;
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod remote_fuse;
mod repository;
mod s3_backend;
pub mod s3_server;
pub mod xorb;

pub use error::{Error, Result};
pub use filesystem::{FileSystem, SetAttr, XattrPolicy};
pub use repository::{
    DirEntry, FsckReport, GcReport, Kind, Node, Reconstruction, Repository, Snapshot, Stats, Term,
};
pub use s3_backend::S3BackendConfig;

pub mod cas;
pub mod memory;
pub mod transfer;
