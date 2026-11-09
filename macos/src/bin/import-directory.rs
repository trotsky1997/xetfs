//! Experimental offline importer for a quiescent macOS directory.
use std::{
    collections::HashMap,
    env,
    ffi::{CString, OsStr},
    fs::{self, Metadata, OpenOptions},
    io::{Read, Write},
    os::{
        darwin::fs::MetadataExt as DarwinMetadataExt,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    ptr,
};
use xetfs::{FileSystem, Repository, SetAttr, XattrPolicy};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
}
fn cpath(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}
fn attrs(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let p = cpath(path)?;
    let flags = libc::XATTR_NOFOLLOW;
    let n = unsafe { libc::listxattr(p.as_ptr(), ptr::null_mut(), 0, flags) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = vec![0u8; n as usize];
    if n > 0 {
        let got =
            unsafe { libc::listxattr(p.as_ptr(), names.as_mut_ptr().cast(), names.len(), flags) };
        if got != n {
            return Err(format!("xattrs changed while reading {}", path.display()).into());
        }
    }
    let mut out = Vec::new();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let key = CString::new(name)?;
        let n = unsafe { libc::getxattr(p.as_ptr(), key.as_ptr(), ptr::null_mut(), 0, 0, flags) };
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut value = vec![0u8; n as usize];
        if n > 0 {
            let got = unsafe {
                libc::getxattr(
                    p.as_ptr(),
                    key.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    flags,
                )
            };
            if got != n {
                return Err(format!("xattr changed while reading {}", path.display()).into());
            }
        }
        out.push((name.to_vec(), value));
    }
    out.sort();
    Ok(out)
}
fn reject_acl(path: &Path) -> Result<()> {
    let p = cpath(path)?;
    let acl = unsafe { acl_get_link_np(p.as_ptr(), 0x100) };
    if !acl.is_null() {
        unsafe {
            acl_free(acl);
        }
        return Err(format!("ACL is unsupported: {}", path.display()).into());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOENT) {
        return Err(error.into());
    }
    Ok(())
}
fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.size() == b.size()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
        && a.st_flags() == b.st_flags()
        && a.nlink() == b.nlink()
}
fn ns(sec: i64, nano: i64) -> Result<i64> {
    Ok(sec
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(nano))
        .ok_or("timestamp out of range")?)
}
fn metadata(
    fs: &mut FileSystem,
    ino: u64,
    m: &Metadata,
    xattrs: &[(Vec<u8>, Vec<u8>)],
) -> Result<()> {
    for (name, value) in xattrs {
        fs.set_xattr(ino, name, Some(value), XattrPolicy::Set)?;
    }
    fs.setattr(
        ino,
        SetAttr {
            mode: Some(m.mode()),
            uid: Some(m.uid()),
            gid: Some(m.gid()),
            atime_ns: Some(ns(m.atime(), m.atime_nsec())?),
            mtime_ns: Some(ns(m.mtime(), m.mtime_nsec())?),
            birthtime_ns: Some(ns(m.st_birthtime(), m.st_birthtime_nsec())?),
            flags: Some(m.st_flags()),
            ..Default::default()
        },
    )?;
    Ok(())
}
fn preload_directory(fs: &mut FileSystem, path: &Path, dev: u64) -> Result<()> {
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let before = entry.metadata()?;
        if !before.is_file() || before.dev() != dev || before.size() > 64 * 1024 {
            continue;
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(entry.path())?;
        if !unchanged(&before, &input.metadata()?) {
            return Err("source changed during preload".into());
        }
        let mut data = Vec::new();
        Read::by_ref(&mut input)
            .take(64 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 != before.size() || !unchanged(&before, &input.metadata()?) {
            return Err("source changed during preload".into());
        }
        if batch.len() == 512 || bytes + data.len() > 8 * 1024 * 1024 {
            fs.preload_small_files(&batch)?;
            batch.clear();
            bytes = 0;
        }
        bytes += data.len();
        batch.push(data);
    }
    if !batch.is_empty() {
        fs.preload_small_files(&batch)?;
    }
    Ok(())
}

fn walk(
    fs: &mut FileSystem,
    source: &Path,
    relative: &Path,
    parent: u64,
    dev: u64,
    links: &mut HashMap<(u64, u64), (u64, u64, u64)>,
    count: &mut u64,
) -> Result<()> {
    let path = source.join(relative);
    let before = fs::symlink_metadata(&path)?;
    if before.dev() != dev {
        return Err(format!("nested filesystem: {}", path.display()).into());
    }
    if before.st_flags() & !0x8000 != 0 {
        return Err(format!("unsupported flags: {}", path.display()).into());
    }
    reject_acl(&path)?;
    let xattrs = attrs(&path)?;
    let kind = before.file_type();
    let key = (before.dev(), before.ino());
    let ino = if relative.as_os_str().is_empty() {
        1
    } else {
        let name = relative.file_name().ok_or("missing name")?;
        if !kind.is_dir() && before.nlink() > 1 && links.contains_key(&key) {
            let entry = links.get_mut(&key).unwrap();
            entry.1 += 1;
            fs.link(entry.0, parent, name)?.ino
        } else {
            let node = if kind.is_dir() {
                fs.mkdir(parent, name, before.mode(), before.uid(), before.gid())?
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                fs.symlink(
                    parent,
                    name,
                    target.as_os_str().as_bytes(),
                    before.uid(),
                    before.gid(),
                )?
            } else if kind.is_file() {
                let input = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)?;
                if !unchanged(&before, &input.metadata()?) {
                    return Err(format!("file changed: {}", path.display()).into());
                }
                fs.put_file(Path::new("/").join(relative), input)?
            } else {
                return Err(format!("unsupported file type: {}", path.display()).into());
            };
            if !kind.is_dir() && before.nlink() > 1 {
                links.insert(key, (node.ino, 1, u64::from(before.nlink())));
            }
            metadata(fs, node.ino, &before, &xattrs)?;
            node.ino
        }
    };
    *count += 1;
    if kind.is_dir() {
        preload_directory(fs, &path, dev)?;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            walk(
                fs,
                source,
                &relative.join(entry.file_name()),
                ino,
                dev,
                links,
                count,
            )?;
        }
        metadata(fs, ino, &before, &xattrs)?;
    }
    let after = fs::symlink_metadata(&path)?;
    if !unchanged(&before, &after) || xattrs != attrs(&path)? {
        return Err(format!("source changed: {}", path.display()).into());
    }
    Ok(())
}
fn verify(
    fs: &FileSystem,
    source: &Path,
    relative: &Path,
    ino: u64,
    links: &mut HashMap<(u64, u64), u64>,
) -> Result<()> {
    let path = source.join(relative);
    let before = fs::symlink_metadata(&path)?;
    let node = fs.node(ino)?;
    let invalid = || format!("verification failed: {}", path.display());
    if node.mode != before.mode() & 0o7777
        || node.uid != before.uid()
        || node.gid != before.gid()
        || node.flags != before.st_flags()
        || node.mtime_ns != ns(before.mtime(), before.mtime_nsec())?
        || node.birthtime_ns != ns(before.st_birthtime(), before.st_birthtime_nsec())?
    {
        return Err(invalid().into());
    }
    reject_acl(&path)?;
    let attributes = attrs(&path)?;
    let mut stored = Vec::new();
    for name in fs.list_xattrs(ino)? {
        stored.push((name.clone(), fs.get_xattr(ino, &name)?));
    }
    stored.sort();
    if attributes != stored {
        return Err(invalid().into());
    }
    if before.file_type().is_dir() {
        if node.kind != xetfs::Kind::Directory {
            return Err(invalid().into());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&path)? {
            names.push(entry?.file_name());
        }
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let entries = fs.list(ino)?;
        if names.len() != entries.len() {
            return Err(invalid().into());
        }
        for (name, entry) in names.into_iter().zip(entries) {
            if name.as_bytes() != entry.name {
                return Err(invalid().into());
            }
            verify(fs, source, &relative.join(name), entry.node.ino, links)?;
        }
    } else {
        if u64::from(node.nlink) != u64::from(before.nlink()) {
            return Err(invalid().into());
        }
        let key = (before.dev(), before.ino());
        if let Some(previous) = links.insert(key, ino) {
            if previous != ino {
                return Err(invalid().into());
            }
        }
        if before.file_type().is_symlink() {
            if node.kind != xetfs::Kind::Symlink
                || node.target.as_deref() != Some(fs::read_link(&path)?.as_os_str().as_bytes())
            {
                return Err(invalid().into());
            }
        } else if before.file_type().is_file() {
            if node.kind != xetfs::Kind::File || node.size != before.size() {
                return Err(invalid().into());
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            if !unchanged(&before, &file.metadata()?) {
                return Err(invalid().into());
            }
            let mut offset = 0;
            let mut buffer = vec![0u8; 1024 * 1024];
            while offset < before.size() {
                let count = std::cmp::min(buffer.len() as u64, before.size() - offset) as usize;
                file.read_exact(&mut buffer[..count])?;
                if fs.read_inode(ino, offset, count)? != buffer[..count] {
                    return Err(invalid().into());
                }
                offset += count as u64;
            }
        } else {
            return Err(invalid().into());
        }
    }
    if !unchanged(&before, &fs::symlink_metadata(&path)?) || attributes != attrs(&path)? {
        return Err(invalid().into());
    }
    Ok(())
}

fn preflight(source: &Path) -> Result<u64> {
    let root = fs::symlink_metadata(source)?;
    let mut stack = vec![source.to_path_buf()];
    let mut links: HashMap<(u64, u64), (u64, u64, PathBuf)> = HashMap::new();
    let mut count = 0;
    while let Some(path) = stack.pop() {
        let meta = fs::symlink_metadata(&path)?;
        if meta.dev() != root.dev() {
            return Err(format!("nested filesystem: {}", path.display()).into());
        }
        if meta.st_flags() & !0x8000 != 0 {
            return Err(format!("unsupported flags: {}", path.display()).into());
        }
        if meta.file_type().is_file() {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if !unchanged(&meta, &file.metadata()?)
                || !unchanged(&meta, &fs::symlink_metadata(&path)?)
            {
                return Err(format!("source changed: {}", path.display()).into());
            }
        }
        reject_acl(&path)?;
        for (name, _) in attrs(&path)? {
            if name.len() > 127 {
                return Err(format!("unsupported xattr name: {}", path.display()).into());
            }
        }
        ns(meta.atime(), meta.atime_nsec())?;
        ns(meta.mtime(), meta.mtime_nsec())?;
        ns(meta.st_birthtime(), meta.st_birthtime_nsec())?;
        let kind = meta.file_type();
        if kind.is_dir() {
            for entry in fs::read_dir(&path)? {
                let entry = entry?;
                if entry.file_name().as_bytes().len() > 255 {
                    return Err(format!("unsupported name: {}", entry.path().display()).into());
                }
                stack.push(entry.path());
            }
        } else if kind.is_symlink() {
            let target = fs::read_link(&path)?;
            if target.as_os_str().as_bytes().len() > 4096 {
                return Err(format!("symlink target too long: {}", path.display()).into());
            }
        } else if !kind.is_file() || meta.size() > i64::MAX as u64 {
            return Err(format!("unsupported file type or size: {}", path.display()).into());
        }
        if !kind.is_dir() && meta.nlink() > 1 {
            let value = links.entry((meta.dev(), meta.ino())).or_insert((
                0,
                u64::from(meta.nlink()),
                path.clone(),
            ));
            if value.1 != u64::from(meta.nlink()) {
                return Err(format!("link count changed: {}", path.display()).into());
            }
            value.0 += 1;
        }
        count += 1;
        if count % 100_000 == 0 {
            eprintln!("Checked {count} entries...");
        }
    }
    for (seen, total, path) in links.values() {
        if seen != total {
            return Err(format!(
                "hard link outside source or source changed: {} ({seen}/{total})",
                path.display()
            )
            .into());
        }
    }
    Ok(count)
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let first = args
        .next()
        .ok_or("usage: import-directory --check SOURCE | [--resume] SOURCE REPOSITORY")?;
    if first == "--check" {
        let source = fs::canonicalize(args.next().ok_or("missing source")?)?;
        if args.next().is_some() || !source.is_dir() {
            return Err("expected one source directory".into());
        }
        let count = preflight(&source)?;
        println!("Checked {count} entries; source must remain quiescent during import");
        return Ok(());
    }
    let resuming = first == "--resume";
    let source = PathBuf::from(if resuming {
        args.next().ok_or("missing source")?
    } else {
        first
    });
    let repo = PathBuf::from(
        args.next()
            .ok_or("usage: import-directory SOURCE NEW_REPOSITORY")?,
    );
    if args.next().is_some() {
        return Err("usage: import-directory SOURCE NEW_REPOSITORY".into());
    }
    let source = fs::canonicalize(source)?;
    if !source.is_dir() {
        return Err("source must be a directory".into());
    }
    let repo = if resuming {
        fs::canonicalize(&repo)?
    } else {
        if repo.exists() {
            return Err("repository path must not exist".into());
        }
        let parent = fs::canonicalize(repo.parent().unwrap_or(Path::new(".")))?;
        parent.join(repo.file_name().ok_or("missing repository name")?)
    };
    if repo.starts_with(&source) || source.starts_with(&repo) {
        return Err("repository and source must be separate".into());
    }
    let root = fs::symlink_metadata(&source)?;
    let mut repository = if resuming {
        Repository::open_import(&repo)?
    } else {
        Repository::init_import(&repo)?
    };
    let marker = repo.join("IMPORT_INCOMPLETE");
    let mut identity = b"XETFS-IMPORT\x01".to_vec();
    identity.extend_from_slice(&root.dev().to_le_bytes());
    identity.extend_from_slice(&root.ino().to_le_bytes());
    identity.extend_from_slice(&root.st_birthtime().to_le_bytes());
    identity.extend_from_slice(&root.st_birthtime_nsec().to_le_bytes());
    if resuming {
        let mut stored = Vec::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker)?
            .take(128)
            .read_to_end(&mut stored)?;
        if stored != identity {
            return Err("source does not match incomplete import".into());
        }
        repository.reset_import()?;
    } else {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker)?;
        file.write_all(&identity)?;
        file.sync_all()?;
    }
    let mut fs = FileSystem::new(repository);
    let mut count = 0;
    let mut links = HashMap::new();
    walk(
        &mut fs,
        &source,
        Path::new(OsStr::new("")),
        1,
        root.dev(),
        &mut links,
        &mut count,
    )?;
    if links.values().any(|(_, seen, total)| seen != total) {
        return Err("source has hard links outside the imported directory".into());
    }
    fs.gc(false)?;
    fs.repository().fsck()?;
    verify(
        &fs,
        &source,
        Path::new(OsStr::new("")),
        1,
        &mut HashMap::new(),
    )?;
    if !unchanged(&root, &fs::symlink_metadata(&source)?) {
        return Err("source root changed during import".into());
    }
    fs.repository().finish_import()?;
    drop(fs);
    println!("Imported {count} entries into {}", repo.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_detects_corrupted_import() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"correct bytes").unwrap();
        let root = fs::symlink_metadata(&source).unwrap();
        let mut imported =
            FileSystem::new(Repository::init_import(temp.path().join("repo")).unwrap());
        walk(
            &mut imported,
            &source,
            Path::new(""),
            1,
            root.dev(),
            &mut HashMap::new(),
            &mut 0,
        )
        .unwrap();
        verify(&imported, &source, Path::new(""), 1, &mut HashMap::new()).unwrap();
        let original = imported.resolve("/file").unwrap();
        imported.put_file("/file", &b"wrong bytes!!"[..]).unwrap();
        imported
            .setattr(
                original.ino,
                SetAttr {
                    mode: Some(original.mode),
                    mtime_ns: Some(original.mtime_ns),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(verify(&imported, &source, Path::new(""), 1, &mut HashMap::new()).is_err());
    }
}
