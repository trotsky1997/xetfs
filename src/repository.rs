use crate::{
    Error, Result,
    chunker::Chunker,
    hash::{self, Hash},
    s3_backend::{
        S3BackendConfig, S3ObjectStore, read_config, verify_and_cache_xorb, verify_xorb_bytes,
        write_config,
    },
    xorb,
};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt as UnixFileExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(i64)]
pub enum Kind {
    File = 1,
    Directory = 2,
    Symlink = 3,
}
impl Kind {
    pub(crate) fn from_i64(n: i64) -> rusqlite::Result<Self> {
        match n {
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub ino: u64,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub content: Option<String>,
    pub target: Option<Vec<u8>>,
    pub nlink: u32,
}
impl Node {
    pub(crate) fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            ino: r.get(0)?,
            kind: Kind::from_i64(r.get(1)?)?,
            mode: r.get(2)?,
            uid: r.get(3)?,
            gid: r.get(4)?,
            size: r.get(5)?,
            atime_ns: r.get(6)?,
            mtime_ns: r.get(7)?,
            ctime_ns: r.get(8)?,
            content: r.get(9)?,
            target: r.get(10)?,
            nlink: r.get(11)?,
        })
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub node: Node,
}
#[derive(Debug, Serialize)]
pub struct Stats {
    pub files: u64,
    pub directories: u64,
    pub logical_bytes: u64,
    pub recipes: u64,
    pub recipe_bytes: u64,
    pub unique_chunks: u64,
    pub unique_chunk_bytes: u64,
    pub xorbs: u64,
    pub xorb_bytes: u64,
    pub database_bytes: u64,
    pub snapshots: u64,
}
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub name: String,
    pub created_ns: i64,
}
#[derive(Debug, Serialize)]
pub struct GcReport {
    pub removed_objects: u64,
    pub reclaimed_bytes: u64,
    pub before_object_bytes: u64,
    pub after_object_bytes: u64,
    pub chunks_remaining: u64,
}
#[derive(Debug, Serialize)]
pub struct Reconstruction {
    pub file_hash: String,
    pub size: u64,
    pub terms: Vec<Term>,
}
#[derive(Debug, Serialize)]
pub struct Term {
    pub xorb: String,
    pub chunk_start: u64,
    pub chunk_end: u64,
    pub offset: u64,
    pub size: u64,
    pub verification_hash: String,
}
#[derive(Debug, Serialize)]
pub struct FsckReport {
    pub xorbs_verified: u64,
    pub chunks_verified: u64,
    pub recipes_verified: u64,
}

pub(crate) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// A repository has one process owner. The lock covers metadata, staging and GC.
pub struct Repository {
    pub(crate) conn: Connection,
    pub(crate) root: PathBuf,
    object_store: Option<S3ObjectStore>,
    _lock: File,
}

/// Stable, re-readable view of one inode's bytes for remote upload.
///
/// Committed files read immutable chunk records directly from the local CAS;
/// open write handles read from a cloned descriptor for their staging file.
#[derive(Clone)]
pub(crate) struct UploadSnapshot {
    size: u64,
    source: UploadSnapshotSource,
}

/// A validated local Xorb ready for direct transfer to the Hugging Face CAS.
pub(crate) struct UploadXorb {
    pub(crate) hash: Hash,
    pub(crate) bytes: Vec<u8>,
    pub(crate) raw_bytes: u64,
    pub(crate) chunks: Vec<UploadXorbChunk>,
}

pub(crate) struct UploadXorbChunk {
    pub(crate) hash: Hash,
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

#[derive(Clone)]
enum UploadSnapshotSource {
    Staged(Arc<File>),
    Cas { chunks: Arc<[UploadChunk]> },
}

#[derive(Clone)]
struct UploadChunk {
    file_offset: u64,
    len: u64,
    hash: String,
    object: PathBuf,
    object_offset: u64,
    stored: u64,
}

impl UploadSnapshot {
    pub(crate) fn staged(file: File, size: u64) -> Self {
        Self {
            size,
            source: UploadSnapshotSource::Staged(Arc::new(file)),
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn read_at(&self, offset: u64, size: usize) -> Result<Vec<u8>> {
        if offset >= self.size || size == 0 {
            return Ok(Vec::new());
        }
        let length = (self.size - offset).min(size as u64) as usize;
        if let UploadSnapshotSource::Staged(file) = &self.source {
            let mut bytes = vec![0; length];
            file.read_exact_at(&mut bytes, offset)?;
            return Ok(bytes);
        }

        let UploadSnapshotSource::Cas { chunks } = &self.source else {
            unreachable!();
        };
        let end = offset + length as u64;
        let mut output = Vec::with_capacity(length);
        let mut index = chunks.partition_point(|chunk| chunk.file_offset + chunk.len <= offset);
        while let Some(chunk) = chunks.get(index) {
            if chunk.file_offset >= end {
                break;
            }
            let chunk_end = chunk
                .file_offset
                .checked_add(chunk.len)
                .ok_or_else(|| Error::Corrupt("upload snapshot chunk range overflow".into()))?;
            if chunk_end <= offset || chunk.file_offset >= end {
                continue;
            }
            let data = read_chunk_record(
                &chunk.object,
                chunk.object_offset,
                chunk.len,
                chunk.stored,
                &chunk.hash,
            )?;
            let from = offset.saturating_sub(chunk.file_offset) as usize;
            let to = (end - chunk.file_offset).min(chunk.len) as usize;
            output.extend_from_slice(&data[from..to]);
            index += 1;
        }
        if output.len() != length {
            return Err(Error::Corrupt("incomplete upload snapshot read".into()));
        }
        Ok(output)
    }
}

fn read_chunk_record(
    path: &Path,
    offset: u64,
    len: u64,
    stored: u64,
    expected_hash: &str,
) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let (data, consumed) =
        xorb::decode(&mut file)?.ok_or_else(|| Error::Corrupt("missing chunk".into()))?;
    if data.len() as u64 != len
        || consumed != stored
        || hash::to_hex(&hash::chunk_hash(&data)) != expected_hash
    {
        return Err(Error::Corrupt(format!(
            "chunk integrity failed: {expected_hash}"
        )));
    }
    Ok(data)
}

#[derive(Default)]
struct Pack {
    bytes: Vec<u8>,
    chunks: Vec<(Hash, u64, u64, u64)>,
}
impl Pack {
    fn add(&mut self, h: Hash, len: u64, record: Vec<u8>) {
        self.chunks
            .push((h, len, self.bytes.len() as u64, record.len() as u64));
        self.bytes.extend(record);
    }
}
fn save_pack(
    tx: &Transaction<'_>,
    root: &Path,
    object_store: Option<&S3ObjectStore>,
    pack: &mut Pack,
) -> Result<()> {
    if pack.chunks.is_empty() {
        return Ok(());
    }
    let id = hash::to_hex(&hash::xorb_hash(
        &pack
            .chunks
            .iter()
            .map(|&(h, n, _, _)| (h, n))
            .collect::<Vec<_>>(),
    ));
    let path = root.join("objects").join(format!("{id}.xorb"));
    let mut temp = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
    temp.write_all(&pack.bytes)?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(&path) {
        Ok(_) => (),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::read(&path)? != pack.bytes {
                return Err(Error::Corrupt(format!("existing object differs: {id}")));
            }
        }
        Err(e) => return Err(e.error.into()),
    }
    sync_dir(&root.join("objects"))?;
    if let Some(store) = object_store {
        store.put_xorb(&id, &pack.bytes)?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO xorbs VALUES (?1,?2)",
        params![id, pack.bytes.len() as u64],
    )?;
    for (i, &(h, len, offset, stored)) in pack.chunks.iter().enumerate() {
        let hex = hash::to_hex(&h);
        tx.execute(
            "INSERT OR IGNORE INTO xorb_chunks VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, i as u64, hex, len, offset, stored],
        )?;
        tx.execute("INSERT INTO chunks VALUES (?1,?2,?3) ON CONFLICT(hash) DO UPDATE SET xorb=excluded.xorb,idx=excluded.idx", params![hex,id,i as u64])?;
    }
    *pack = Pack::default();
    Ok(())
}
fn read_chunk(
    conn: &Connection,
    root: &Path,
    object_store: Option<&S3ObjectStore>,
    hex: &str,
) -> Result<Vec<u8>> {
    let (object,offset,len,stored,object_size): (String,u64,u64,u64,u64) = conn.query_row(
        "SELECT p.xorb,p.offset,p.len,p.stored,x.size FROM chunks c JOIN xorb_chunks p ON p.xorb=c.xorb AND p.idx=c.idx JOIN xorbs x ON x.id=p.xorb WHERE c.hash=?1",
        [hex], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    hash::from_hex(&object)?;
    let path = verify_and_cache_xorb(root, &object, Some(object_size), object_store)?;
    read_chunk_record(&path, offset, len, stored, hex)
}
impl Repository {
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() && fs::read_dir(path)?.next().is_some() {
            return Err(Error::Invalid("repository directory must be empty".into()));
        }
        fs::create_dir_all(path)?;
        let root = fs::canonicalize(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("LOCK"))?;
        lock.try_lock_exclusive().map_err(|_| Error::Busy)?;
        fs::create_dir(root.join("objects"))?;
        fs::create_dir(root.join("tmp"))?;
        let mut conn = Connection::open(root.join("metadata.sqlite"))?;
        Self::configure(&conn)?;
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema.sql"))?;
        let now = now_ns();
        tx.execute("INSERT INTO inodes(ino,kind,mode,uid,gid,size,atime,mtime,ctime) VALUES(1,2,?1,?2,?3,0,?4,?4,?4)", params![0o755,unsafe {libc::getuid()},unsafe {libc::getgid()},now])?;
        tx.commit()?;
        sync_dir(&root)?;
        Ok(Self {
            conn,
            root,
            object_store: None,
            _lock: lock,
        })
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(path)?;
        if !root.join("metadata.sqlite").is_file() {
            return Err(Error::Invalid("not a XetFS repository".into()));
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("LOCK"))?;
        lock.try_lock_exclusive().map_err(|_| Error::Busy)?;
        let conn = Connection::open(root.join("metadata.sqlite"))?;
        Self::configure(&conn)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 1 {
            return Err(Error::Invalid(format!(
                "unsupported repository version: {version}"
            )));
        }
        conn.execute("DELETE FROM inodes WHERE ino<>1 AND NOT EXISTS(SELECT 1 FROM entries WHERE entries.ino=inodes.ino)",[])?;
        for entry in fs::read_dir(root.join("tmp"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        let object_store = read_config(&root)?
            .map(|config| S3ObjectStore::from_config(&config))
            .transpose()?;
        Ok(Self {
            conn,
            root,
            object_store,
            _lock: lock,
        })
    }
    fn configure(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.root
    }
    /// Configure S3 as the durable Xorb object store and upload all existing objects first.
    pub fn configure_s3_backend(&mut self, config: &S3BackendConfig) -> Result<()> {
        config.validate()?;
        let store = S3ObjectStore::from_config(config)?;
        store.check_bucket()?;
        let objects: Vec<(String, u64)> = self
            .conn
            .prepare("SELECT id,size FROM xorbs ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, expected_size) in objects {
            let bytes = fs::read(self.root.join("objects").join(format!("{id}.xorb")))?;
            if bytes.len() as u64 != expected_size {
                return Err(Error::Corrupt(format!(
                    "Xorb size mismatch while configuring S3: {id}"
                )));
            }
            verify_xorb_bytes(&id, &bytes)?;
            store.put_xorb(&id, &bytes)?;
        }
        write_config(&self.root, config)?;
        self.object_store = Some(store);
        Ok(())
    }
    /// Objects become durable before the transaction publishes their index.
    pub fn ingest(&mut self, input: impl Read) -> Result<(String, u64)> {
        let tx = self.conn.transaction()?;
        let mut pack = Pack::default();
        let mut seen = HashSet::new();
        let mut recipe = Vec::new();
        let mut size = 0u64;
        for data in Chunker::new(input) {
            let data = data?;
            let h = hash::chunk_hash(&data);
            let hex = hash::to_hex(&h);
            let len = data.len() as u64;
            size = size
                .checked_add(len)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| Error::Invalid("file exceeds maximum size".into()))?;
            recipe.push((h, len));
            if !seen.insert(hex.clone()) {
                continue;
            }
            if tx
                .query_row("SELECT 1 FROM chunks WHERE hash=?1", [&hex], |_| Ok(()))
                .optional()?
                .is_some()
            {
                continue;
            }
            let record = xorb::encode(&data)?;
            if pack.bytes.len() + record.len() > xorb::MAX_XORB {
                save_pack(&tx, &self.root, self.object_store.as_ref(), &mut pack)?;
            }
            pack.add(h, len, record);
        }
        save_pack(&tx, &self.root, self.object_store.as_ref(), &mut pack)?;
        let id = hash::to_hex(&hash::file_hash(&recipe));
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO contents VALUES (?1,?2)",
            params![id, size],
        )?;
        if inserted != 0 {
            let mut offset = 0u64;
            let mut stmt = tx.prepare_cached("INSERT INTO file_chunks VALUES (?1,?2,?3,?4,?5)")?;
            for (idx, (h, len)) in recipe.iter().enumerate() {
                stmt.execute(params![id, idx as u64, offset, len, hash::to_hex(h)])?;
                offset += len;
            }
        }
        tx.commit()?;
        Ok((id, size))
    }
    pub fn read_content(&self, id: &str, offset: u64, size: usize) -> Result<Vec<u8>> {
        let len: u64 = self
            .conn
            .query_row("SELECT size FROM contents WHERE id=?1", [id], |r| r.get(0))?;
        let end = offset.saturating_add(size as u64).min(len);
        if offset >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut stmt = self.conn.prepare("SELECT hash,offset,len FROM file_chunks WHERE content=?1 AND offset>=COALESCE((SELECT offset FROM file_chunks WHERE content=?1 AND offset<=?2 ORDER BY offset DESC LIMIT 1),0) AND offset<?3 ORDER BY offset")?;
        let rows = stmt.query_map(params![id, offset, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, u64>(2)?,
            ))
        })?;
        for row in rows {
            let (hex, start, len) = row?;
            let data = read_chunk(&self.conn, &self.root, self.object_store.as_ref(), &hex)?;
            let from = offset.saturating_sub(start) as usize;
            let to = (end - start).min(len) as usize;
            out.extend_from_slice(&data[from..to]);
        }
        if out.len() as u64 != end - offset {
            return Err(Error::Corrupt("incomplete file reconstruction".into()));
        }
        Ok(out)
    }

    pub(crate) fn upload_snapshot(&self, id: &str, expected_size: u64) -> Result<UploadSnapshot> {
        let size: u64 =
            self.conn
                .query_row("SELECT size FROM contents WHERE id=?1", [id], |r| r.get(0))?;
        if size != expected_size {
            return Err(Error::Corrupt(format!(
                "upload snapshot size changed: expected {expected_size}, found {size}"
            )));
        }

        let mut statement = self.conn.prepare(
            "SELECT f.offset,f.len,f.hash,c.xorb,p.offset,p.stored \
             FROM file_chunks f \
             JOIN chunks c ON c.hash=f.hash \
             JOIN xorb_chunks p ON p.xorb=c.xorb AND p.idx=c.idx \
             WHERE f.content=?1 ORDER BY f.idx",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
            ))
        })?;

        let mut chunks = Vec::new();
        let mut next_offset = 0u64;
        for row in rows {
            let (file_offset, len, chunk_hash, object_hash, object_offset, stored) = row?;
            if file_offset != next_offset || len == 0 {
                return Err(Error::Corrupt(format!("invalid upload recipe for {id}")));
            }
            hash::from_hex(&chunk_hash)?;
            hash::from_hex(&object_hash)?;
            verify_and_cache_xorb(&self.root, &object_hash, None, self.object_store.as_ref())?;
            next_offset = next_offset
                .checked_add(len)
                .ok_or_else(|| Error::Corrupt("upload recipe size overflow".into()))?;
            chunks.push(UploadChunk {
                file_offset,
                len,
                hash: chunk_hash,
                object: self
                    .root
                    .join("objects")
                    .join(format!("{object_hash}.xorb")),
                object_offset,
                stored,
            });
        }
        if next_offset != size {
            return Err(Error::Corrupt(format!("incomplete upload recipe for {id}")));
        }
        Ok(UploadSnapshot {
            size,
            source: UploadSnapshotSource::Cas {
                chunks: Arc::from(chunks),
            },
        })
    }

    /// Read and validate an immutable local Xorb for direct remote upload.
    pub(crate) fn read_upload_xorb(&self, id: &str) -> Result<UploadXorb> {
        let expected_hash = hash::from_hex(id)?;
        let expected_size: u64 =
            self.conn
                .query_row("SELECT size FROM xorbs WHERE id=?1", [id], |row| row.get(0))?;
        if expected_size == 0 || expected_size > xorb::MAX_XORB as u64 {
            return Err(Error::Corrupt(format!("invalid Xorb size for {id}")));
        }
        let path = verify_and_cache_xorb(
            &self.root,
            id,
            Some(expected_size),
            self.object_store.as_ref(),
        )?;
        let bytes = fs::read(&path)?;
        if bytes.len() as u64 != expected_size {
            return Err(Error::Corrupt(format!("Xorb size changed for {id}")));
        }

        let mut statement = self.conn.prepare(
            "SELECT idx,hash,len,offset,stored FROM xorb_chunks WHERE xorb=?1 ORDER BY idx",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?;

        let mut cursor = std::io::Cursor::new(bytes.as_slice());
        let mut chunks = Vec::new();
        let mut hashes_and_lengths = Vec::new();
        let mut raw_bytes = 0u64;
        for (row_index, row) in rows.enumerate() {
            let (index, expected_chunk_hash, expected_len, expected_offset, expected_stored) = row?;
            if index != row_index as u64 || cursor.position() != expected_offset {
                return Err(Error::Corrupt(format!("invalid Xorb chunk index for {id}")));
            }
            let (data, stored) = xorb::decode(&mut cursor)?
                .ok_or_else(|| Error::Corrupt(format!("Xorb {id} ended before chunk {index}")))?;
            let chunk_hash = hash::chunk_hash(&data);
            if data.len() as u64 != expected_len
                || stored != expected_stored
                || hash::to_hex(&chunk_hash) != expected_chunk_hash
            {
                return Err(Error::Corrupt(format!(
                    "Xorb chunk integrity failed for {id}"
                )));
            }
            chunks.push(UploadXorbChunk {
                hash: chunk_hash,
                // MDB shard chunk ranges are measured in uncompressed bytes;
                // expected_offset above is only the physical offset in the
                // locally encoded (possibly compressed) Xorb object.
                offset: raw_bytes,
                len: expected_len,
            });
            hashes_and_lengths.push((chunk_hash, expected_len));
            raw_bytes = raw_bytes
                .checked_add(expected_len)
                .ok_or_else(|| Error::Corrupt("Xorb raw size overflow".into()))?;
        }
        if chunks.is_empty()
            || cursor.position() != expected_size
            || hash::xorb_hash(&hashes_and_lengths) != expected_hash
        {
            return Err(Error::Corrupt(format!("Xorb integrity failed for {id}")));
        }
        Ok(UploadXorb {
            hash: expected_hash,
            bytes,
            raw_bytes,
            chunks,
        })
    }
    pub fn export_content(&self, id: &str, mut output: impl Write) -> Result<()> {
        self.conn
            .query_row("SELECT 1 FROM contents WHERE id=?1", [id], |_| Ok(()))
            .optional()?
            .ok_or(Error::Errno(libc::ENOENT))?;
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM file_chunks WHERE content=?1 ORDER BY idx")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        for row in rows {
            output.write_all(&read_chunk(
                &self.conn,
                &self.root,
                self.object_store.as_ref(),
                &row?,
            )?)?;
        }
        Ok(())
    }
    /// Adjacent chunks in the same Xorb coalesce into end-exclusive range terms.
    pub fn reconstruction(&self, id: &str) -> Result<Reconstruction> {
        let size = self
            .conn
            .query_row("SELECT size FROM contents WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or(Error::Errno(libc::ENOENT))?;
        let mut stmt=self.conn.prepare("SELECT c.xorb,c.idx,f.offset,f.len,f.hash FROM file_chunks f JOIN chunks c ON c.hash=f.hash WHERE f.content=?1 ORDER BY f.idx")?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, u64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut terms: Vec<Term> = Vec::new();
        let mut term_chunks: Vec<Vec<Hash>> = Vec::new();
        for row in rows {
            let (xorb, idx, offset, len, hex) = row?;
            let h = hash::from_hex(&hex)?;
            if let Some(last) = terms
                .last_mut()
                .filter(|last| last.xorb == xorb && last.chunk_end == idx)
            {
                last.chunk_end += 1;
                last.size += len;
                term_chunks.last_mut().unwrap().push(h);
            } else {
                terms.push(Term {
                    xorb,
                    chunk_start: idx,
                    chunk_end: idx + 1,
                    offset,
                    size: len,
                    verification_hash: String::new(),
                });
                term_chunks.push(vec![h]);
            }
        }
        for (term, chunks) in terms.iter_mut().zip(term_chunks) {
            term.verification_hash = hash::to_hex(&hash::verification_hash(&chunks));
        }
        Ok(Reconstruction {
            file_hash: id.into(),
            size,
            terms,
        })
    }
    pub fn stats(&self) -> Result<Stats> {
        let scalar = |sql: &str| -> Result<u64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let database_bytes = [
            "metadata.sqlite",
            "metadata.sqlite-wal",
            "metadata.sqlite-shm",
        ]
        .iter()
        .filter_map(|p| fs::metadata(self.root.join(p)).ok())
        .map(|m| m.len())
        .sum();
        Ok(Stats {
            files: scalar(
                "SELECT COUNT(*) FROM inodes WHERE kind=1 AND EXISTS(SELECT 1 FROM entries WHERE entries.ino=inodes.ino)",
            )?,
            directories: scalar("SELECT COUNT(*) FROM inodes WHERE kind=2")?,
            logical_bytes: scalar(
                "SELECT COALESCE(SUM(size),0) FROM inodes WHERE kind=1 AND EXISTS(SELECT 1 FROM entries WHERE entries.ino=inodes.ino)",
            )?,
            recipes: scalar("SELECT COUNT(*) FROM contents")?,
            recipe_bytes: scalar("SELECT COALESCE(SUM(size),0) FROM contents")?,
            unique_chunks: scalar("SELECT COUNT(*) FROM chunks")?,
            unique_chunk_bytes: scalar(
                "SELECT COALESCE(SUM(p.len),0) FROM chunks c JOIN xorb_chunks p ON c.xorb=p.xorb AND c.idx=p.idx",
            )?,
            xorbs: scalar("SELECT COUNT(*) FROM xorbs")?,
            xorb_bytes: scalar("SELECT COALESCE(SUM(size),0) FROM xorbs")?,
            database_bytes,
            snapshots: scalar("SELECT COUNT(*) FROM snapshots")?,
        })
    }
    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        Ok(self
            .conn
            .prepare("SELECT name,created FROM snapshots ORDER BY name")?
            .query_map([], |r| {
                Ok(Snapshot {
                    name: r.get(0)?,
                    created_ns: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub(crate) fn snapshot(&mut self, name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 255 || name.contains('\0') {
            return Err(Error::Invalid("invalid snapshot name".into()));
        }
        if self
            .conn
            .query_row("SELECT 1 FROM snapshots WHERE name=?1", [name], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Err(Error::Errno(libc::EEXIST));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO snapshots VALUES (?1,?2)",
            params![name, now_ns()],
        )?;
        tx.execute("INSERT INTO snapshot_nodes SELECT ?1,* FROM inodes WHERE ino=1 OR EXISTS(SELECT 1 FROM entries WHERE entries.ino=inodes.ino)",[name])?;
        tx.execute(
            "INSERT INTO snapshot_entries SELECT ?1,* FROM entries",
            [name],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn drop_snapshot(&mut self, name: &str) -> Result<()> {
        if self
            .conn
            .execute("DELETE FROM snapshots WHERE name=?1", [name])?
            == 0
        {
            return Err(Error::Errno(libc::ENOENT));
        }
        Ok(())
    }
    pub(crate) fn restore(&mut self, name: &str) -> Result<()> {
        if self
            .conn
            .query_row("SELECT 1 FROM snapshots WHERE name=?1", [name], |_| Ok(()))
            .optional()?
            .is_none()
        {
            return Err(Error::Errno(libc::ENOENT));
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM entries", [])?;
        tx.execute("DELETE FROM inodes", [])?;
        tx.execute("INSERT INTO inodes SELECT ino,kind,mode,uid,gid,size,atime,mtime,ctime,content,target FROM snapshot_nodes WHERE snapshot=?1",[name])?;
        tx.execute(
            "INSERT INTO entries SELECT parent,name,ino FROM snapshot_entries WHERE snapshot=?1",
            [name],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn gc(&mut self, compact: bool) -> Result<GcReport> {
        let object_bytes = || -> Result<u64> {
            let mut total = 0;
            for entry in fs::read_dir(self.root.join("objects"))? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry
                        .file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix(".xorb"))
                        .is_some_and(|s| hash::from_hex(s).is_ok())
                {
                    total += entry.metadata()?.len();
                }
            }
            Ok(total)
        };
        let before_object_bytes = object_bytes()?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM contents WHERE NOT EXISTS(SELECT 1 FROM inodes WHERE content=contents.id) AND NOT EXISTS(SELECT 1 FROM snapshot_nodes WHERE content=contents.id)",[])?;
        tx.execute("DELETE FROM chunks WHERE NOT EXISTS(SELECT 1 FROM file_chunks WHERE file_chunks.hash=chunks.hash)",[])?;
        if compact {
            let keys: Vec<String> = tx
                .prepare("SELECT hash FROM chunks ORDER BY xorb,idx")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut pack = Pack::default();
            for hex in keys {
                let data = read_chunk(&tx, &self.root, self.object_store.as_ref(), &hex)?;
                let record = xorb::encode(&data)?;
                if pack.bytes.len() + record.len() > xorb::MAX_XORB {
                    save_pack(&tx, &self.root, self.object_store.as_ref(), &mut pack)?;
                }
                pack.add(hash::from_hex(&hex)?, data.len() as u64, record);
            }
            save_pack(&tx, &self.root, self.object_store.as_ref(), &mut pack)?;
        }
        tx.execute(
            "DELETE FROM xorbs WHERE NOT EXISTS(SELECT 1 FROM chunks WHERE chunks.xorb=xorbs.id)",
            [],
        )?;
        tx.commit()?;
        let referenced: HashSet<String> = self
            .conn
            .prepare("SELECT id FROM xorbs")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut report = GcReport {
            removed_objects: 0,
            reclaimed_bytes: 0,
            before_object_bytes,
            after_object_bytes: 0,
            chunks_remaining: self.stats()?.unique_chunks,
        };
        let mut removed = HashSet::new();
        for entry in fs::read_dir(self.root.join("objects"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".xorb")) else {
                continue;
            };
            if hash::from_hex(id).is_ok()
                && !referenced.contains(id)
                && entry.file_type()?.is_file()
            {
                if let Some(store) = self.object_store.as_ref() {
                    store.delete_xorb(id)?;
                }
                fs::remove_file(entry.path())?;
                removed.insert(id.to_owned());
            }
        }
        if let Some(store) = self.object_store.as_ref() {
            for id in store.list_xorbs()? {
                if !referenced.contains(&id) && removed.insert(id.clone()) {
                    store.delete_xorb(&id)?;
                }
            }
        }
        report.removed_objects = removed.len() as u64;
        sync_dir(&self.root.join("objects"))?;
        report.after_object_bytes = object_bytes()?;
        report.reclaimed_bytes = report
            .before_object_bytes
            .saturating_sub(report.after_object_bytes);
        Ok(report)
    }
    pub fn fsck(&self) -> Result<FsckReport> {
        let check: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(Error::Corrupt(check));
        }
        if self
            .conn
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_some()
        {
            return Err(Error::Corrupt("foreign key check failed".into()));
        }
        let mut report = FsckReport {
            xorbs_verified: 0,
            chunks_verified: 0,
            recipes_verified: 0,
        };
        let objects: Vec<(String, u64)> = self
            .conn
            .prepare("SELECT id,size FROM xorbs")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, size) in objects {
            hash::from_hex(&id)?;
            let object_path =
                verify_and_cache_xorb(&self.root, &id, Some(size), self.object_store.as_ref())?;
            let mut file = File::open(object_path)?;
            if file.metadata()?.len() != size || size > xorb::MAX_XORB as u64 {
                return Err(Error::Corrupt(format!("object length: {id}")));
            }
            let records: Vec<(u64, String, u64, u64, u64)> = self
                .conn
                .prepare(
                    "SELECT idx,hash,len,offset,stored FROM xorb_chunks WHERE xorb=?1 ORDER BY idx",
                )?
                .query_map([&id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut chunks = Vec::new();
            let mut position = 0;
            for (expected_idx, (idx, hex, len, offset, stored)) in records.into_iter().enumerate() {
                let (data, n) = xorb::decode(&mut file)?
                    .ok_or_else(|| Error::Corrupt("truncated object".into()))?;
                let h = hash::chunk_hash(&data);
                if idx != expected_idx as u64
                    || position != offset
                    || n != stored
                    || data.len() as u64 != len
                    || hash::to_hex(&h) != hex
                {
                    return Err(Error::Corrupt(format!("object record integrity: {id}")));
                }
                position += n;
                chunks.push((h, len));
                report.chunks_verified += 1;
            }
            if position != size || hash::to_hex(&hash::xorb_hash(&chunks)) != id {
                return Err(Error::Corrupt(format!("Xorb hash mismatch: {id}")));
            }
            report.xorbs_verified += 1;
        }
        let recipes: Vec<(String, u64)> = self
            .conn
            .prepare("SELECT id,size FROM contents")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, size) in recipes {
            let records: Vec<(u64, u64, u64, String)> = self
                .conn
                .prepare(
                    "SELECT idx,offset,len,hash FROM file_chunks WHERE content=?1 ORDER BY idx",
                )?
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut chunks = Vec::new();
            let mut position = 0;
            for (i, (idx, offset, len, hex)) in records.into_iter().enumerate() {
                let data = read_chunk(&self.conn, &self.root, self.object_store.as_ref(), &hex)?;
                if idx != i as u64 || offset != position || data.len() as u64 != len {
                    return Err(Error::Corrupt(format!("recipe offset/length: {id}")));
                }
                chunks.push((hash::from_hex(&hex)?, len));
                position += len;
            }
            if position != size || hash::to_hex(&hash::file_hash(&chunks)) != id {
                return Err(Error::Corrupt(format!("file hash mismatch: {id}")));
            }
            report.recipes_verified += 1;
        }
        let bad_nodes: u64 = self.conn.query_row("SELECT COUNT(*) FROM inodes i LEFT JOIN contents c ON c.id=i.content WHERE (i.kind=1 AND (c.id IS NULL OR i.size<>c.size)) OR (i.kind=3 AND (i.target IS NULL OR i.size<>length(i.target)))",[],|r| r.get(0))?;
        if bad_nodes != 0 {
            return Err(Error::Corrupt("inode content or size mismatch".into()));
        }
        for sql in [
            "SELECT COUNT(*) FROM chunks c JOIN xorb_chunks p ON c.xorb=p.xorb AND c.idx=p.idx WHERE c.hash<>p.hash",
            "SELECT COUNT(*) FROM inodes WHERE ino=1 AND kind<>2",
            "SELECT COUNT(*) FROM entries e JOIN inodes p ON p.ino=e.parent WHERE p.kind<>2 OR e.ino=1 OR length(e.name)=0 OR length(e.name)>255 OR instr(e.name,x'2f')>0 OR instr(e.name,x'00')>0 OR e.name IN (x'2e',x'2e2e')",
            "SELECT COUNT(*) FROM inodes i WHERE i.kind=2 AND (SELECT COUNT(*) FROM entries WHERE ino=i.ino)>1",
            "WITH RECURSIVE reachable(ino) AS (VALUES(1) UNION SELECT e.ino FROM entries e JOIN reachable r ON e.parent=r.ino) SELECT COUNT(*) FROM entries WHERE parent NOT IN reachable",
            "SELECT COUNT(*) FROM snapshot_nodes i LEFT JOIN contents c ON c.id=i.content WHERE (i.kind=1 AND (c.id IS NULL OR i.size<>c.size)) OR (i.kind=3 AND (i.target IS NULL OR i.size<>length(i.target)))",
            "SELECT COUNT(*) FROM snapshot_entries e LEFT JOIN snapshot_nodes p ON p.snapshot=e.snapshot AND p.ino=e.parent LEFT JOIN snapshot_nodes n ON n.snapshot=e.snapshot AND n.ino=e.ino WHERE p.ino IS NULL OR n.ino IS NULL OR p.kind<>2",
        ] {
            let count: u64 = self.conn.query_row(sql, [], |r| r.get(0))?;
            if count != 0 {
                return Err(Error::Corrupt(
                    "namespace, snapshot or chunk index invariant failed".into(),
                ));
            }
        }
        let roots: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM inodes WHERE ino=1 AND kind=2",
            [],
            |r| r.get(0),
        )?;
        if roots != 1 {
            return Err(Error::Corrupt("root inode missing".into()));
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_xorb_chunk_ranges_use_uncompressed_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let mut fs = crate::FileSystem::new(Repository::init(dir.path().join("repo")).unwrap());
        let mut data = vec![0x5a; 256 * 1024];
        let midpoint = data.len() / 2;
        data[midpoint..].fill(0x5b);
        let node = fs.put_file("/compressed", data.as_slice()).unwrap();
        let content = node.content.as_deref().unwrap();
        let recipe = fs.repository().reconstruction(content).unwrap();
        let xorb = fs
            .repository()
            .read_upload_xorb(&recipe.terms[0].xorb)
            .unwrap();

        assert!(xorb.chunks.len() >= 2);
        let mut raw_offset = 0;
        for chunk in &xorb.chunks {
            assert_eq!(chunk.offset, raw_offset);
            raw_offset += chunk.len;
        }
        assert_eq!(raw_offset, xorb.raw_bytes);
        // The physical compressed offsets differ after the first chunk; this
        // guards against accidentally serializing local storage offsets into
        // the public MDB shard's raw byte ranges.
        assert!(xorb.chunks[1].offset > 0);
        assert!(xorb.chunks[1].offset > xorb.chunks[0].offset);
        assert!((xorb.bytes.len() as u64) < xorb.raw_bytes);
    }
}
