use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    net::SocketAddr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use xetfs::{FileSystem, Kind, Repository, S3BackendConfig, SetAttr, chunker::Chunker, hash};

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
    /// Configure the S3 Xorb backend or serve the XetFS namespace over S3.
    S3 {
        #[command(subcommand)]
        command: S3Command,
    },
}
#[derive(Subcommand)]
enum S3Command {
    /// Store Xorb objects in an S3 bucket; metadata and a local cache stay in this repository.
    ConfigureBackend {
        bucket: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        #[arg(long, default_value = "")]
        prefix: String,
        #[arg(long)]
        endpoint_url: Option<String>,
        #[arg(long, default_value_t = false)]
        path_style: bool,
    },
    /// Run an authenticated S3-compatible API server over the repository namespace.
    Serve {
        #[arg(long, default_value = "127.0.0.1:9000")]
        listen: SocketAddr,
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
        /// Maximum number of concurrent Hub file transfers (1-64).
        #[arg(long, default_value_t = 8, value_parser = parse_workers)]
        workers: usize,
    },
    /// Read a byte range directly from the Hugging Face Xet CAS to stdout.
    Cat {
        #[command(flatten)]
        hub: HubFileArgs,
        /// Start offset in bytes.
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// Number of bytes to read (maximum 64 MiB per request).
        #[arg(long, default_value_t = 1024 * 1024)]
        length: u64,
        /// Maximum number of concurrent signed Xorb range requests (1-64).
        #[arg(
            long,
            default_value_t = xetfs::remote_cas::DEFAULT_CAS_WORKERS,
            value_parser = parse_workers
        )]
        workers: usize,
    },
    /// Upload this local namespace as one Hub commit.
    Push {
        #[command(flatten)]
        hub: HubArgs,
        /// Commit message on the Hub.
        #[arg(long, default_value = "Sync from XetFS")]
        message: String,
        /// Maximum concurrent native Xet CAS Xorb uploads (1-64, bounded-memory batches).
        #[arg(
            long,
            default_value_t = xetfs::remote::DEFAULT_CAS_UPLOAD_WORKERS,
            value_parser = parse_workers
        )]
        workers: usize,
    },
    /// Mount a Hub revision as a read-only, on-demand Linux filesystem.
    Mount {
        #[command(flatten)]
        hub: HubArgs,
        /// Empty directory used as the mountpoint.
        mountpoint: PathBuf,
        /// Maximum number of concurrent signed Xorb range requests (1-64).
        #[arg(
            long,
            default_value_t = xetfs::remote_cas::DEFAULT_CAS_WORKERS,
            value_parser = parse_workers
        )]
        workers: usize,
        /// Memory allowed for fetched 4 MiB windows (4-16384 MiB).
        #[arg(long, default_value_t = 256, value_parser = parse_cache_mib)]
        cache_mib: usize,
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
    /// Optional repository directory; pull retains it, push selects it, and remote mount uses it as root.
    #[arg(long)]
    prefix: Option<String>,
}
#[derive(Args)]
struct HubFileArgs {
    /// Hugging Face repository id, such as owner/name.
    repo_id: String,
    /// Repository-relative file path.
    file_path: String,
    /// Hugging Face repository kind.
    #[arg(long, value_enum, default_value = "model")]
    repo_type: HubRepoType,
    /// Hub branch, tag, or commit SHA.
    #[arg(long, default_value = "main")]
    revision: String,
}
#[derive(Clone, Copy, ValueEnum)]
enum HubRepoType {
    Model,
    Dataset,
    Space,
    Kernel,
}
fn parse_workers(value: &str) -> std::result::Result<usize, String> {
    let workers = value
        .parse::<usize>()
        .map_err(|_| "workers must be an integer between 1 and 64".to_owned())?;
    if (1..=64).contains(&workers) {
        Ok(workers)
    } else {
        Err("workers must be between 1 and 64".to_owned())
    }
}
fn parse_cache_mib(value: &str) -> std::result::Result<usize, String> {
    let cache_mib = value
        .parse::<usize>()
        .map_err(|_| "cache-mib must be an integer between 4 and 16384".to_owned())?;
    if (4..=16384).contains(&cache_mib) {
        Ok(cache_mib)
    } else {
        Err("cache-mib must be between 4 and 16384".to_owned())
    }
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
        Command::S3 {
            command:
                S3Command::ConfigureBackend {
                    bucket,
                    region,
                    prefix,
                    endpoint_url,
                    path_style,
                },
        } => {
            let mut repository = Repository::open(&cli.repo)
                .with_context(|| format!("open repository {}", cli.repo.display()))?;
            let config = S3BackendConfig {
                bucket: bucket.clone(),
                region: region.clone(),
                prefix: prefix.clone(),
                endpoint_url: endpoint_url.clone(),
                path_style: *path_style,
            };
            repository.configure_s3_backend(&config)?;
            return json(&serde_json::json!({
                "repository": repository.path(),
                "bucket": config.bucket,
                "region": config.region,
                "prefix": config.prefix,
                "endpoint_url": config.endpoint_url,
                "path_style": config.path_style,
                "credentials": "AWS SDK credential chain"
            }));
        }
        Command::S3 {
            command: S3Command::Serve { listen },
        } => {
            let access_key = std::env::var("XETFS_S3_ACCESS_KEY_ID")
                .context("set XETFS_S3_ACCESS_KEY_ID for the S3 gateway")?;
            let secret_key = std::env::var("XETFS_S3_SECRET_ACCESS_KEY")
                .context("set XETFS_S3_SECRET_ACCESS_KEY for the S3 gateway")?;
            let fs = FileSystem::new(
                Repository::open(&cli.repo)
                    .with_context(|| format!("open repository {}", cli.repo.display()))?,
            );
            return xetfs::s3_server::serve(fs, *listen, access_key, secret_key)
                .context("run S3 gateway");
        }
        Command::Remote {
            command:
                RemoteCommand::Cat {
                    hub,
                    offset,
                    length,
                    workers,
                },
        } => {
            let bytes = xetfs::remote_cas::read_range_with_workers(
                &hub.repo_id,
                hub.repo_type.as_str(),
                &hub.revision,
                &hub.file_path,
                *offset,
                *length,
                *workers,
            )?;
            io::stdout().lock().write_all(&bytes)?;
            return Ok(());
        }
        Command::Remote {
            command:
                RemoteCommand::Mount {
                    hub,
                    mountpoint,
                    workers,
                    cache_mib,
                },
        } => {
            #[cfg(all(target_os = "linux", feature = "fuse"))]
            {
                xetfs::remote_fuse::mount(
                    &hub.repo_id,
                    hub.repo_type.as_str(),
                    &hub.revision,
                    hub.prefix.as_deref(),
                    mountpoint,
                    *workers,
                    cache_mib.saturating_mul(1024 * 1024),
                )?;
                return Ok(());
            }
            #[cfg(not(all(target_os = "linux", feature = "fuse")))]
            {
                let _ = (hub, mountpoint, workers, cache_mib);
                anyhow::bail!(
                    "remote FUSE mounting requires Linux and the fuse Cargo feature; remote cat works on macOS"
                );
            }
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
            RemoteCommand::Pull {
                hub,
                force,
                workers,
            } => json(&xetfs::remote::pull(
                &mut fs,
                &hub.repo_id,
                hub.repo_type.as_str(),
                &hub.revision,
                hub.prefix.as_deref(),
                force,
                workers,
            )?)?,
            RemoteCommand::Cat { .. } => {
                unreachable!("remote cat is handled before opening a local repository")
            }
            RemoteCommand::Mount { .. } => {
                unreachable!("remote mount is handled before opening a local repository")
            }
            RemoteCommand::Push {
                hub,
                message,
                workers,
            } => json(&xetfs::remote::push(
                &fs,
                &hub.repo_id,
                hub.repo_type.as_str(),
                &hub.revision,
                hub.prefix.as_deref(),
                &message,
                workers,
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
        Command::Init | Command::Hash { .. } | Command::InspectXorb { .. } | Command::S3 { .. } => {
            unreachable!()
        }
    }
    Ok(())
}
