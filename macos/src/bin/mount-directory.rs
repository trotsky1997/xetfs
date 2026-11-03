//! Mount an XetFS repository at an empty, separate directory.
use std::{
    env, fs,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    process::{Command, ExitCode},
};

fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let repository = PathBuf::from(
        args.next()
            .ok_or("usage: mount-directory REPOSITORY EMPTY_MOUNTPOINT")?,
    );
    let mountpoint = PathBuf::from(
        args.next()
            .ok_or("usage: mount-directory REPOSITORY EMPTY_MOUNTPOINT")?,
    );
    if args.next().is_some() {
        return Err("usage: mount-directory REPOSITORY EMPTY_MOUNTPOINT".into());
    }
    if !fs::symlink_metadata(&mountpoint)?.is_dir() {
        return Err("mountpoint must be a directory, not a symlink".into());
    }
    let repository = fs::canonicalize(repository)?;
    let mountpoint = fs::canonicalize(mountpoint)?;
    if repository.starts_with(&mountpoint) || mountpoint.starts_with(&repository) {
        return Err("repository and mountpoint must be separate".into());
    }
    xetfs::Repository::identifier(&repository)?;
    if fs::read_dir(&mountpoint)?.next().is_some() {
        return Err("mountpoint must be empty".into());
    }
    if let Some(parent) = mountpoint.parent() {
        if fs::metadata(parent)?.dev() != fs::metadata(&mountpoint)?.dev() {
            return Err("mountpoint is already a filesystem root".into());
        }
    }
    let status = Command::new("/sbin/mount")
        .arg("-F")
        .arg("-t")
        .arg("xetfs")
        .arg(&repository)
        .arg(&mountpoint)
        .status()?;
    Ok(ExitCode::from(if status.success() { 0 } else { 1 }))
}
