use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use xetfs::{FileSystem, Kind, Repository, SetAttr, chunker::Chunker, hash};

#[derive(Parser)]
#[command(version, about = "XetFS: content-addressed deduplicating filesystem")]
struct Cli {
    #[arg(long, global = true, default_value = ".xetfs")]
    repo: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a repository in an empty directory.
    Init,
    /// Import a host file; atomically replace an existing regular file.
    Put {
        source: PathBuf,
        path: PathBuf,
    },
    /// Export a file to the host (or use cat for stdout).
    Get {
        path: PathBuf,
        destination: PathBuf,
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// Stream all or a byte range of a file to stdout.
    Cat {
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: Option<u64>,
        #[arg(long)]
        snapshot: Option<String>,
    },
    Ls {
        #[arg(default_value = "/")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Stat {
        path: PathBuf,
    },
    /// Inspect Xorb ranges and verification hashes for a file recipe.
    Manifest {
        path: PathBuf,
    },
    Mkdir {
        path: PathBuf,
        #[arg(short = 'p', long)]
        parents: bool,
    },
    Rm {
        path: PathBuf,
        #[arg(short = 'r', long)]
        recursive: bool,
    },
    Mv {
        from: PathBuf,
        to: PathBuf,
        #[arg(long)]
        no_replace: bool,
    },
    /// Copy by sharing the immutable recipe, with a distinct inode.
    Cp {
        from: PathBuf,
        to: PathBuf,
    },
    Ln {
        target: PathBuf,
        path: PathBuf,
        #[arg(short = 's', long)]
        symbolic: bool,
    },
    /// Apply bytes from a host file at a byte offset.
    Patch {
        source: PathBuf,
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: u64,
    },
    Truncate {
        path: PathBuf,
        size: u64,
    },
    Chmod {
        path: PathBuf,
        mode: String,
    },
    Stats,
    Fsck,
    /// Remove unreachable content; compact also reclaims holes in live Xorbs.
    Gc {
        #[arg(long)]
        compact: bool,
    },
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Foreground Linux FUSE mount. Unmount with fusermount3 -u MOUNTPOINT.
    Mount {
        mountpoint: PathBuf,
    },
    /// Compute Xet hashes without opening a repository.
    Hash {
        source: PathBuf,
        #[arg(long)]
        chunks: bool,
    },
    /// Decode and verify the framing of a standalone Xorb.
    InspectXorb {
        source: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Transfer a repository revision with Hugging Face Xet.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
}
#[derive(Subcommand)]
enum RemoteCommand {
    /// Download a Hub revision into this local XetFS repository.
    Pull {
        #[command(flatten)]
        hub: HubArgs,
        /// Replace local files when the Hub file differs.
        #[arg(long)]
        force: bool,
    },
    /// Upload this local namespace as one Hub commit.
    Push {
        #[command(flatten)]
        hub: HubArgs,
        /// Commit message on the Hub.
        #[arg(long, default_value = "Sync from XetFS")]
        message: String,
    },
}
#[derive(Args)]
struct HubArgs {
    /// Hugging Face repository id, such as owner/name.
    repo_id: String,
    /// Hugging Face repository kind.
    #[arg(long, value_enum, default_value = "model")]
    repo_type: HubRepoType,
    /// Hub branch, tag, or commit SHA.
    #[arg(long, default_value = "main")]
    revision: String,
    /// Optional repository-relative directory. Pull preserves this prefix locally.
    #[arg(long)]
    prefix: Option<String>,
}
#[derive(Clone, Copy, ValueEnum)]
enum HubRepoType {
    Model,
    Dataset,
    Space,
    Kernel,
}
impl HubRepoType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Dataset => "dataset",
            Self::Space => "space",
            Self::Kernel => "kernel",
        }
    }
}
#[derive(Subcommand)]
enum SnapshotCommand {
    Create { name: String },
    List,
    Drop { name: String },
    Restore { name: String },
}
fn json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn stream(
    fs: &FileSystem,
    path: &Path,
    snapshot: Option<&str>,
    mut offset: u64,
    length: Option<u64>,
    mut out: impl Write,
) -> Result<()> {
    let mut remaining = length.unwrap_or(u64::MAX);
    let ino = if snapshot.is_none() {
        Some(fs.resolve(path)?.ino)
    } else {
        None
    };
    while remaining > 0 {
        let n = remaining.min(1024 * 1024) as usize;
        let data = match snapshot {
            Some(s) => fs.read_snapshot(s, path, offset, n)?,
            None => fs.read_inode(ino.unwrap(), offset, n)?,
        };
        if data.is_empty() {
            break;
        }
        out.write_all(&data)?;
        offset += data.len() as u64;
        remaining -= data.len() as u64;
    }
    out.flush()?;
    Ok(())
}
fn remove(fs: &mut FileSystem, parent: u64, name: &OsStr, recursive: bool) -> Result<()> {
    let node = fs.lookup(parent, name)?;
    if node.kind == Kind::Directory && recursive {
        for entry in fs.list(node.ino)? {
            remove(fs, node.ino, OsStr::from_bytes(&entry.name), true)?;
        }
    }
    fs.unlink(parent, name, node.kind == Kind::Directory)?;
    Ok(())
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Hash { source, chunks } => {
            let mut records = Vec::new();
            for c in Chunker::new(File::open(source)?) {
                let data = c?;
                records.push((hash::chunk_hash(&data), data.len() as u64));
            }
            let mut result = serde_json::json!({"file_hash":hash::to_hex(&hash::file_hash(&records)),"xorb_hash":hash::to_hex(&hash::xorb_hash(&records)),"bytes":records.iter().map(|(_,n)|n).sum::<u64>(),"chunk_count":records.len()});
            if *chunks {
                result["chunks"] = serde_json::json!(
                    records
                        .iter()
                        .map(|(h, n)| serde_json::json!({"hash":hash::to_hex(h),"length":n}))
                        .collect::<Vec<_>>()
                );
            }
            return json(&result);
        }
        Command::InspectXorb { source, output } => {
            let mut file = io::BufReader::new(File::open(source)?);
            let mut out = output.as_ref().map(File::create).transpose()?;
            let mut records = Vec::new();
            let mut stored = 0;
            while let Some((data, n)) = xetfs::xorb::decode(&mut file)? {
                records.push((hash::chunk_hash(&data), data.len() as u64));
                stored += n;
                if let Some(ref mut out) = out {
                    out.write_all(&data)?;
                }
            }
            return json(
                &serde_json::json!({"xorb_hash":hash::to_hex(&hash::xorb_hash(&records)),"chunks":records.len(),"stored_bytes":std::fs::metadata(source)?.len(),"chunk_record_bytes":stored,"raw_bytes":records.iter().map(|(_,n)|n).sum::<u64>()}),
            );
        }
        Command::Init => {
            let repo = Repository::init(&cli.repo)?;
            return json(&serde_json::json!({"repository":repo.path(),"format_version":1}));
        }
        _ => (),
    }
    let mut fs = FileSystem::new(
        Repository::open(&cli.repo)
            .with_context(|| format!("open repository {}", cli.repo.display()))?,
    );
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    match cli.command {
        Command::Put { source, path } => json(&fs.put_file(path, File::open(source)?)?)?,
        Command::Get {
            path,
            destination,
            snapshot,
        } => {
            let parent = destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            stream(&fs, &path, snapshot.as_deref(), 0, None, temp.as_file_mut())?;
            temp.as_file().sync_all()?;
            temp.persist(&destination)?;
            File::open(parent)?.sync_all()?;
        }
        Command::Cat {
            path,
            offset,
            length,
            snapshot,
        } => stream(
            &fs,
            &path,
            snapshot.as_deref(),
            offset,
            length,
            io::BufWriter::new(io::stdout().lock()),
        )?,
        Command::Ls {
            path,
            json: as_json,
        } => {
            let entries = fs.list(fs.resolve(path)?.ino)?;
            if as_json {
                json(&entries)?;
            } else {
                for e in entries {
                    println!(
                        "{:?}\t{:04o}\t{}\t{}\t{}",
                        e.node.kind,
                        e.node.mode,
                        e.node.nlink,
                        e.node.size,
                        String::from_utf8_lossy(&e.name)
                    );
                }
            }
        }
        Command::Stat { path } => json(&fs.resolve_nofollow(path)?)?,
        Command::Manifest { path } => {
            let node = fs.resolve(path)?;
            let id = node
                .content
                .as_deref()
                .context("manifest requires a regular file")?;
            json(&fs.repository().reconstruction(id)?)?;
        }
        Command::Mkdir { path, parents } => {
            let n = if parents {
                fs.mkdir_all(path)?
            } else {
                let (p, n) = fs.path_parent(path)?;
                fs.mkdir(p, OsStr::from_bytes(&n), 0o755, uid, gid)?
            };
            json(&n)?;
        }
        Command::Rm { path, recursive } => {
            let (p, n) = fs.path_parent(path)?;
            remove(&mut fs, p, OsStr::from_bytes(&n), recursive)?;
        }
        Command::Mv {
            from,
            to,
            no_replace,
        } => {
            let (p, n) = fs.path_parent(from)?;
            let (q, m) = fs.path_parent(to)?;
            fs.rename(
                p,
                OsStr::from_bytes(&n),
                q,
                OsStr::from_bytes(&m),
                no_replace,
            )?;
        }
        Command::Cp { from, to } => json(&fs.copy_file(from, to)?)?,
        Command::Ln {
            target,
            path,
            symbolic,
        } => {
            let (p, n) = fs.path_parent(path)?;
            let node = if symbolic {
                fs.symlink(
                    p,
                    OsStr::from_bytes(&n),
                    target.as_os_str().as_bytes(),
                    uid,
                    gid,
                )?
            } else {
                fs.link(fs.resolve_nofollow(target)?.ino, p, OsStr::from_bytes(&n))?
            };
            json(&node)?;
        }
        Command::Patch {
            source,
            path,
            mut offset,
        } => {
            let h = fs.open_handle(fs.resolve(path)?.ino, libc::O_WRONLY)?;
            let mut input = File::open(source)?;
            let mut buf = vec![0; 1024 * 1024];
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let mut used = 0;
                while used < n {
                    let written = fs.write_handle(h, offset, &buf[used..n])?;
                    anyhow::ensure!(written > 0, "write made no progress");
                    used += written;
                    offset += written as u64;
                }
            }
            fs.flush_handle(h)?;
            fs.close_handle(h)?;
        }
        Command::Truncate { path, size } => {
            fs.setattr(
                fs.resolve(path)?.ino,
                SetAttr {
                    size: Some(size),
                    ..Default::default()
                },
            )?;
        }
        Command::Chmod { path, mode } => {
            let mode = u32::from_str_radix(&mode, 8).context("mode must be octal")?;
            anyhow::ensure!(mode <= 0o7777, "mode exceeds 7777");
            fs.setattr(
                fs.resolve(path)?.ino,
                SetAttr {
                    mode: Some(mode),
                    ..Default::default()
                },
            )?;
        }
        Command::Stats => json(&fs.repository().stats()?)?,
        Command::Fsck => json(&fs.repository().fsck()?)?,
        Command::Gc { compact } => json(&fs.gc(compact)?)?,
        Command::Snapshot { command } => match command {
            SnapshotCommand::Create { name } => fs.snapshot(&name)?,
            SnapshotCommand::List => json(&fs.repository().snapshots()?)?,
            SnapshotCommand::Drop { name } => fs.drop_snapshot(&name)?,
            SnapshotCommand::Restore { name } => fs.restore(&name)?,
        },
        Command::Remote { command } => match command {
            RemoteCommand::Pull { hub, force } => json(&xetfs::remote::pull(
                &mut fs,
                &hub.repo_id,
                hub.repo_type.as_str(),
                &hub.revision,
                hub.prefix.as_deref(),
                force,
            )?)?,
            RemoteCommand::Push { hub, message } => json(&xetfs::remote::push(
                &fs,
                &hub.repo_id,
                hub.repo_type.as_str(),
                &hub.revision,
                hub.prefix.as_deref(),
                &message,
            )?)?,
        },
        Command::Mount { mountpoint } => {
            #[cfg(all(target_os = "linux", feature = "fuse"))]
            xetfs::fuse::mount(fs, &mountpoint)?;
            #[cfg(not(all(target_os = "linux", feature = "fuse")))]
            {
                let _ = mountpoint;
                anyhow::bail!(
                    "FUSE mounting requires Linux and the fuse Cargo feature; repository commands work on macOS"
                );
            }
        }
        Command::Init | Command::Hash { .. } | Command::InspectXorb { .. } => unreachable!(),
    }
    Ok(())
}
