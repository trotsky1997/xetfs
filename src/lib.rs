//! Local CAS filesystem with Xet-compatible CDC, hashes and Xorb chunk records.
pub mod chunker;
mod error;
mod filesystem;
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod fuse;
pub mod hash;
pub mod remote;
pub mod remote_cas;
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod remote_fuse;
mod repository;
pub mod xorb;

pub use error::{Error, Result};
pub use filesystem::{FileSystem, SetAttr};
pub use repository::{
    DirEntry, FsckReport, GcReport, Kind, Node, Reconstruction, Repository, Snapshot, Stats, Term,
};
