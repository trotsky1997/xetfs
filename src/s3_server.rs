//! S3-compatible HTTP gateway over the XetFS namespace.
//!
//! Object keys are encoded below a reserved per-bucket directory, so S3 keys such as `../x`
//! cannot escape their bucket and remain distinct from XetFS path components.

use crate::{Error, FileSystem, Kind, Node, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ConnBuilder,
};
use s3s::{
    S3, S3Request, S3Response, S3Result, auth::SimpleAuth, dto::*, s3_error,
    service::S3ServiceBuilder,
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    io::Write,
    net::SocketAddr,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, runtime::Builder, task};

const INTERNAL_OBJECT_DIR: &str = ".xetfs-s3-objects";
const KEY_COMPONENT_BYTES: usize = 200;
const MAX_S3_KEY_BYTES: usize = 1024;
const MIN_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 1024 * 1024;

type SharedFs = Arc<Mutex<FileSystem>>;
static NEXT_UPLOAD_ID: AtomicU64 = AtomicU64::new(1);

struct MultipartPart {
    file: tempfile::NamedTempFile,
    etag: String,
    size: u64,
}

struct MultipartUpload {
    bucket: String,
    key: String,
    parts: BTreeMap<i32, MultipartPart>,
}

#[derive(Clone)]
struct Gateway {
    fs: SharedFs,
    uploads: Arc<tokio::sync::Mutex<HashMap<String, MultipartUpload>>>,
}

/// Start the authenticated S3-compatible gateway and serve until interrupted.
pub fn serve(
    fs: FileSystem,
    address: SocketAddr,
    access_key: String,
    secret_key: String,
) -> Result<()> {
    if access_key.is_empty() || secret_key.is_empty() {
        return Err(Error::Invalid(
            "S3 access and secret keys must both be non-empty".into(),
        ));
    }
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("xetfs-s3-gateway")
        .build()
        .map_err(|err| Error::Backend(format!("create S3 gateway runtime: {err}")))?;
    runtime.block_on(run(fs, address, access_key, secret_key))
}

async fn run(
    fs: FileSystem,
    address: SocketAddr,
    access_key: String,
    secret_key: String,
) -> Result<()> {
    let mut builder = S3ServiceBuilder::new(Gateway {
        fs: Arc::new(Mutex::new(fs)),
        uploads: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    });
    builder.set_auth(SimpleAuth::from_single(access_key, secret_key));
    let service = builder.build();
    let listener = TcpListener::bind(address).await?;
    let http_server = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    eprintln!(
        "XetFS S3 gateway listening on http://{}",
        listener.local_addr()?
    );
    loop {
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = ctrl_c.as_mut() => break,
        };
        let connection = http_server.serve_connection(TokioIo::new(socket), service.clone());
        let connection = graceful.watch(connection.into_owned());
        tokio::spawn(async move {
            let _ = connection.await;
        });
    }
    tokio::select! {
        () = graceful.shutdown() => {},
        () = tokio::time::sleep(Duration::from_secs(10)) => {},
    }
    Ok(())
}

async fn with_fs<T, F>(shared: SharedFs, f: F) -> S3Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut FileSystem) -> S3Result<T> + Send + 'static,
{
    task::spawn_blocking(move || {
        let mut fs = shared
            .lock()
            .map_err(|_| s3_error!(InternalError, "filesystem lock poisoned"))?;
        f(&mut fs)
    })
    .await
    .map_err(|err| s3_error!(InternalError, "filesystem worker failed: {err}"))?
}

fn timestamp(ns: i64) -> Timestamp {
    let duration = Duration::from_nanos(ns.max(0) as u64);
    Timestamp::from(
        UNIX_EPOCH
            .checked_add(duration)
            .unwrap_or(SystemTime::UNIX_EPOCH),
    )
}

fn validate_bucket_name(bucket: &str) -> S3Result<()> {
    if bucket.is_empty() || bucket.len() > 255 || bucket.contains('/') || bucket.contains('\0') {
        return Err(s3_error!(InvalidBucketName));
    }
    Ok(())
}

fn validate_key(key: &str) -> S3Result<()> {
    if key.is_empty() || key.len() > MAX_S3_KEY_BYTES || key.contains('\0') {
        return Err(s3_error!(InvalidArgument, "invalid S3 key"));
    }
    Ok(())
}

fn bucket_node(fs: &FileSystem, bucket: &str) -> S3Result<Node> {
    validate_bucket_name(bucket)?;
    let node = fs
        .lookup(1, OsStr::new(bucket))
        .map_err(|_| s3_error!(NoSuchBucket))?;
    if node.kind != Kind::Directory {
        return Err(s3_error!(NoSuchBucket));
    }
    Ok(node)
}

fn lookup_path(fs: &FileSystem, path: &Path) -> S3Result<Node> {
    let mut current = fs
        .node(1)
        .map_err(|err| s3_error!(InternalError, "read filesystem root: {err}"))?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                if current.kind != Kind::Directory {
                    return Err(s3_error!(NoSuchKey));
                }
                current = fs
                    .lookup(current.ino, name)
                    .map_err(|_| s3_error!(NoSuchKey))?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(s3_error!(InvalidArgument, "invalid internal object path"));
            }
        }
    }
    Ok(current)
}

fn object_path(bucket: &str, key: &str) -> S3Result<PathBuf> {
    validate_bucket_name(bucket)?;
    validate_key(key)?;
    let encoded = URL_SAFE_NO_PAD.encode(key.as_bytes());
    let mut path = PathBuf::from(format!("/{bucket}/{INTERNAL_OBJECT_DIR}"));
    for start in (0..encoded.len()).step_by(KEY_COMPONENT_BYTES) {
        let end = (start + KEY_COMPONENT_BYTES).min(encoded.len());
        let component = encoded
            .get(start..end)
            .ok_or_else(|| s3_error!(InternalError, "invalid internal key encoding"))?;
        path.push(component);
    }
    Ok(path)
}

fn remove_path(fs: &mut FileSystem, path: &std::path::Path) -> Result<bool> {
    let Some(name) = path.file_name() else {
        return Ok(false);
    };
    let Some(parent_path) = path.parent() else {
        return Ok(false);
    };
    let parent = match lookup_path(fs, parent_path) {
        Ok(node) if node.kind == Kind::Directory => node.ino,
        _ => return Ok(false),
    };
    let node = match fs.lookup(parent, name) {
        Ok(node) => node,
        Err(Error::Errno(libc::ENOENT)) => return Ok(false),
        Err(err) => return Err(err),
    };
    fs.unlink(parent, name, node.kind == Kind::Directory)?;
    Ok(true)
}

fn list_bucket_objects(fs: &FileSystem, bucket: &str) -> S3Result<Vec<(String, Node)>> {
    let bucket_node = bucket_node(fs, bucket)?;
    let internal = fs.lookup(bucket_node.ino, OsStr::new(INTERNAL_OBJECT_DIR));
    let Ok(internal) = internal else {
        return Ok(Vec::new());
    };
    if internal.kind != Kind::Directory {
        return Err(s3_error!(
            InternalError,
            "S3 object namespace is not a directory"
        ));
    }
    let mut stack = vec![(internal.ino, String::new())];
    let mut objects = Vec::new();
    while let Some((ino, encoded_prefix)) = stack.pop() {
        for entry in fs
            .list(ino)
            .map_err(|err| s3_error!(InternalError, "list S3 namespace: {err}"))?
        {
            let component = std::str::from_utf8(&entry.name)
                .map_err(|_| s3_error!(InternalError, "invalid S3 namespace entry"))?;
            if !component
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(s3_error!(InternalError, "invalid S3 namespace entry"));
            }
            let encoded = format!("{encoded_prefix}{component}");
            match entry.node.kind {
                Kind::Directory => stack.push((entry.node.ino, encoded)),
                Kind::File => {
                    let bytes = URL_SAFE_NO_PAD
                        .decode(encoded.as_bytes())
                        .map_err(|_| s3_error!(InternalError, "corrupt S3 key mapping"))?;
                    let key = String::from_utf8(bytes)
                        .map_err(|_| s3_error!(InternalError, "corrupt S3 key mapping"))?;
                    objects.push((key, entry.node));
                }
                Kind::Symlink => {
                    return Err(s3_error!(
                        InternalError,
                        "unexpected symlink in S3 namespace"
                    ));
                }
            }
        }
    }
    objects.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(objects)
}

fn object_etag(node: &Node) -> S3Result<ETag> {
    node.content
        .clone()
        .map(ETag::Strong)
        .ok_or_else(|| s3_error!(InternalError, "S3 object has no content hash"))
}

fn remove_directory_tree(fs: &mut FileSystem, directory: u64) -> S3Result<()> {
    for entry in fs
        .list(directory)
        .map_err(|err| s3_error!(InternalError, "list directory: {err}"))?
    {
        let name = OsStr::from_bytes(&entry.name);
        if entry.node.kind == Kind::Directory {
            remove_directory_tree(fs, entry.node.ino)?;
            fs.unlink(directory, name, true)
                .map_err(|err| s3_error!(InternalError, "remove directory: {err}"))?;
        } else {
            fs.unlink(directory, name, false)
                .map_err(|err| s3_error!(InternalError, "remove file: {err}"))?;
        }
    }
    Ok(())
}

fn common_prefix(key: &str, prefix: &str, delimiter: &str) -> Option<String> {
    if delimiter.is_empty() {
        return None;
    }
    let rest = key.strip_prefix(prefix)?;
    let index = rest.find(delimiter)?;
    Some(format!(
        "{}{end}",
        prefix,
        end = &rest[..index + delimiter.len()]
    ))
}

fn token_key(token: &str) -> S3Result<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|_| s3_error!(InvalidToken))?;
    String::from_utf8(bytes).map_err(|_| s3_error!(InvalidToken))
}

fn continuation_token(key: &str) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

fn new_upload_id() -> String {
    let counter = NEXT_UPLOAD_ID.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = format!("{}:{now}:{counter}", std::process::id());
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}

async fn stage_body(
    body: &mut StreamingBlob,
    directory: &std::path::Path,
    expected_size: Option<i64>,
) -> S3Result<MultipartPart> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .map_err(|err| s3_error!(InternalError, "create upload staging file: {err}"))?;
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| s3_error!(InternalError, "read request body: {err}"))?;
        total = total
            .checked_add(chunk.len() as u64)
            .filter(|size| *size <= i64::MAX as u64)
            .ok_or_else(|| s3_error!(EntityTooLarge))?;
        hasher.update(&chunk);
        temporary
            .write_all(&chunk)
            .map_err(|err| s3_error!(InternalError, "stage request body: {err}"))?;
    }
    if expected_size.is_some_and(|expected| expected < 0 || expected as u64 != total) {
        return Err(s3_error!(IncompleteBody));
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|err| s3_error!(InternalError, "sync request body: {err}"))?;
    Ok(MultipartPart {
        file: temporary,
        etag: hasher.finalize().to_hex().to_string(),
        size: total,
    })
}

struct ReadLease {
    fs: SharedFs,
    handle: u64,
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        if let Ok(mut fs) = self.fs.lock() {
            let _ = fs.close_handle(self.handle);
        }
    }
}

#[async_trait::async_trait]
impl S3 for Gateway {
    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        let bucket = req.input.bucket;
        validate_bucket_name(&bucket)?;
        with_fs(self.fs.clone(), move |fs| {
            match fs.lookup(1, OsStr::new(&bucket)) {
                Ok(_) => return Err(s3_error!(BucketAlreadyExists)),
                Err(Error::Errno(libc::ENOENT)) => (),
                Err(err) => return Err(s3_error!(InternalError, "lookup bucket: {err}")),
            }
            fs.mkdir(
                1,
                OsStr::new(&bucket),
                0o755,
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
            )
            .map_err(|err| s3_error!(InternalError, "create bucket: {err}"))?;
            Ok(S3Response::new(CreateBucketOutput::default()))
        })
        .await
    }

    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        let bucket = req.input.bucket;
        with_fs(self.fs.clone(), move |fs| {
            let node = bucket_node(fs, &bucket)?;
            if !list_bucket_objects(fs, &bucket)?.is_empty() {
                return Err(s3_error!(BucketNotEmpty));
            }
            let entries = fs
                .list(node.ino)
                .map_err(|err| s3_error!(InternalError, "list bucket: {err}"))?;
            if entries
                .iter()
                .any(|entry| entry.name.as_slice() != INTERNAL_OBJECT_DIR.as_bytes())
            {
                return Err(s3_error!(BucketNotEmpty));
            }
            if let Ok(internal) = fs.lookup(node.ino, OsStr::new(INTERNAL_OBJECT_DIR)) {
                remove_directory_tree(fs, internal.ino)?;
                fs.unlink(node.ino, OsStr::new(INTERNAL_OBJECT_DIR), true)
                    .map_err(|err| s3_error!(InternalError, "remove S3 namespace: {err}"))?;
            }
            fs.unlink(1, OsStr::new(&bucket), true)
                .map_err(|err| s3_error!(InternalError, "delete bucket: {err}"))?;
            Ok(S3Response::new(DeleteBucketOutput::default()))
        })
        .await
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let input = req.input;
        let path = object_path(&input.bucket, &input.key)?;
        with_fs(self.fs.clone(), move |fs| {
            bucket_node(fs, &input.bucket)?;
            remove_path(fs, &path)
                .map_err(|err| s3_error!(InternalError, "delete object: {err}"))?;
            Ok(S3Response::new(DeleteObjectOutput::default()))
        })
        .await
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let input = req.input;
        let bucket = input.bucket;
        let keys: Vec<String> = input
            .delete
            .objects
            .into_iter()
            .map(|object| object.key)
            .collect();
        with_fs(self.fs.clone(), move |fs| {
            bucket_node(fs, &bucket)?;
            let mut deleted = Vec::new();
            for key in keys {
                let path = object_path(&bucket, &key)?;
                remove_path(fs, &path)
                    .map_err(|err| s3_error!(InternalError, "delete object: {err}"))?;
                if input.delete.quiet != Some(true) {
                    deleted.push(DeletedObject {
                        key: Some(key),
                        ..Default::default()
                    });
                }
            }
            Ok(S3Response::new(DeleteObjectsOutput {
                deleted: Some(deleted),
                ..Default::default()
            }))
        })
        .await
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let input = req.input;
        validate_key(&input.key)?;
        with_fs(self.fs.clone(), {
            let bucket = input.bucket.clone();
            move |fs| {
                bucket_node(fs, &bucket)?;
                Ok(())
            }
        })
        .await?;
        let upload_id = new_upload_id();
        self.uploads.lock().await.insert(
            upload_id.clone(),
            MultipartUpload {
                bucket: input.bucket.clone(),
                key: input.key.clone(),
                parts: BTreeMap::new(),
            },
        );
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(upload_id),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let input = req.input;
        if !(1..=10_000).contains(&input.part_number) {
            return Err(s3_error!(
                InvalidArgument,
                "part number must be between 1 and 10000"
            ));
        }
        let Some(mut body) = input.body else {
            return Err(s3_error!(IncompleteBody));
        };
        let tmp_dir = with_fs(self.fs.clone(), |fs| Ok(fs.repository().path().join("tmp"))).await?;
        let part = stage_body(&mut body, &tmp_dir, input.content_length).await?;
        let etag = part.etag.clone();
        let mut uploads = self.uploads.lock().await;
        let Some(upload) = uploads.get_mut(&input.upload_id) else {
            return Err(s3_error!(NoSuchUpload));
        };
        if upload.bucket != input.bucket || upload.key != input.key {
            return Err(s3_error!(NoSuchUpload));
        }
        upload.parts.insert(input.part_number, part);
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(etag)),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let input = req.input;
        let completed = input
            .multipart_upload
            .and_then(|upload| upload.parts)
            .ok_or_else(|| s3_error!(InvalidPart))?;
        if completed.is_empty() {
            return Err(s3_error!(InvalidPart, "at least one part is required"));
        }
        let mut uploads = self.uploads.lock().await;
        let Some(session) = uploads.get(&input.upload_id) else {
            return Err(s3_error!(NoSuchUpload));
        };
        if session.bucket != input.bucket || session.key != input.key {
            return Err(s3_error!(NoSuchUpload));
        }
        let mut requested_parts = Vec::with_capacity(completed.len());
        let mut previous = 0;
        let last_part = completed.last().and_then(|part| part.part_number);
        for item in completed {
            let number = item.part_number.ok_or_else(|| s3_error!(InvalidPart))?;
            if number <= previous {
                return Err(s3_error!(InvalidPartOrder));
            }
            previous = number;
            let part = session
                .parts
                .get(&number)
                .ok_or_else(|| s3_error!(InvalidPart))?;
            if Some(number) != last_part && part.size < MIN_MULTIPART_PART_BYTES {
                return Err(s3_error!(EntityTooSmall));
            }
            let requested_etag = item.e_tag.ok_or_else(|| s3_error!(InvalidPart))?;
            if requested_etag.value() != part.etag {
                return Err(s3_error!(InvalidPart));
            }
            requested_parts.push(number);
        }
        let total_size = requested_parts.iter().try_fold(0u64, |sum, number| {
            session
                .parts
                .get(number)
                .and_then(|part| sum.checked_add(part.size))
                .filter(|size| *size <= i64::MAX as u64)
                .ok_or_else(|| s3_error!(EntityTooLarge))
        })?;
        if input
            .mpu_object_size
            .is_some_and(|expected| expected < 0 || expected as u64 != total_size)
        {
            return Err(s3_error!(InvalidRequest, "completed object size mismatch"));
        }
        let session = uploads
            .remove(&input.upload_id)
            .ok_or_else(|| s3_error!(NoSuchUpload))?;
        drop(uploads);

        let result = async {
            let tmp_dir =
                with_fs(self.fs.clone(), |fs| Ok(fs.repository().path().join("tmp"))).await?;
            let mut combined = tempfile::NamedTempFile::new_in(&tmp_dir)
                .map_err(|err| s3_error!(InternalError, "create completed upload file: {err}"))?;
            let mut copied_size = 0u64;
            for number in &requested_parts {
                let part = session
                    .parts
                    .get(number)
                    .ok_or_else(|| s3_error!(InvalidPart))?;
                let mut reader = part
                    .file
                    .reopen()
                    .map_err(|err| s3_error!(InternalError, "open uploaded part: {err}"))?;
                let copied = std::io::copy(&mut reader, &mut combined)
                    .map_err(|err| s3_error!(InternalError, "assemble uploaded parts: {err}"))?;
                if copied != part.size {
                    return Err(s3_error!(InvalidPart, "uploaded part size changed"));
                }
                copied_size = copied_size.saturating_add(copied);
            }
            if copied_size != total_size {
                return Err(s3_error!(InvalidPart, "assembled object size mismatch"));
            }
            combined
                .as_file()
                .sync_all()
                .map_err(|err| s3_error!(InternalError, "sync completed upload: {err}"))?;
            let path = object_path(&input.bucket, &input.key)?;
            let bucket = input.bucket.clone();
            let file = combined
                .reopen()
                .map_err(|err| s3_error!(InternalError, "open completed upload: {err}"))?;
            with_fs(self.fs.clone(), move |fs| {
                bucket_node(fs, &bucket)?;
                fs.mkdir_all(path.parent().unwrap_or_else(|| std::path::Path::new("/")))
                    .map_err(|err| s3_error!(InternalError, "create S3 namespace: {err}"))?;
                fs.put_file(path, file)
                    .map_err(|err| s3_error!(InternalError, "store completed upload: {err}"))
            })
            .await
        }
        .await;
        let node = match result {
            Ok(node) => node,
            Err(err) => {
                self.uploads
                    .lock()
                    .await
                    .insert(input.upload_id.clone(), session);
                return Err(err);
            }
        };
        let location = format!("/{}/{}", input.bucket, input.key);
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            location: Some(location),
            e_tag: Some(object_etag(&node)?),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let input = req.input;
        let mut uploads = self.uploads.lock().await;
        let Some(upload) = uploads.get(&input.upload_id) else {
            return Err(s3_error!(NoSuchUpload));
        };
        if upload.bucket != input.bucket || upload.key != input.key {
            return Err(s3_error!(NoSuchUpload));
        }
        uploads.remove(&input.upload_id);
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let input = req.input;
        let path = object_path(&input.bucket, &input.key)?;
        let range_request = input.range;
        let bucket = input.bucket;
        let (node, handle, start, length, content_range) = with_fs(self.fs.clone(), {
            move |fs| {
                bucket_node(fs, &bucket)?;
                let node = lookup_path(fs, &path)?;
                if node.kind != Kind::File {
                    return Err(s3_error!(NoSuchKey));
                }
                let (start, length, content_range) = match range_request {
                    Some(range) => {
                        let range = range
                            .check(node.size)
                            .map_err(|_| s3_error!(InvalidRange))?;
                        let content_range =
                            format!("bytes {}-{}/{}", range.start, range.end - 1, node.size);
                        (range.start, range.end - range.start, Some(content_range))
                    }
                    None => (0, node.size, None),
                };
                let handle = fs
                    .open_handle(node.ino, libc::O_RDONLY)
                    .map_err(|err| s3_error!(InternalError, "open object: {err}"))?;
                Ok((node, handle, start, length, content_range))
            }
        })
        .await?;
        let lease = ReadLease {
            fs: self.fs.clone(),
            handle,
        };
        let body_stream = stream::try_unfold(
            (lease, start, length),
            |(lease, offset, remaining)| async move {
                if remaining == 0 {
                    drop(lease);
                    return Ok::<_, std::io::Error>(None);
                }
                let count = remaining.min(READ_CHUNK_BYTES as u64) as usize;
                let fs = lease.fs.clone();
                let handle = lease.handle;
                let bytes = task::spawn_blocking(move || {
                    let fs = fs
                        .lock()
                        .map_err(|_| std::io::Error::other("filesystem lock poisoned"))?;
                    fs.read_handle(handle, offset, count)
                        .map_err(|err| std::io::Error::other(err.to_string()))
                })
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))??;
                if bytes.is_empty() {
                    drop(lease);
                    return Ok(None);
                }
                let next = bytes.len() as u64;
                Ok(Some((
                    Bytes::from(bytes),
                    (lease, offset + next, remaining - next),
                )))
            },
        );
        let etag = object_etag(&node)?;
        let output = GetObjectOutput {
            body: Some(StreamingBlob::wrap(body_stream)),
            content_length: Some(i64::try_from(length).map_err(|_| s3_error!(InternalError))?),
            content_range,
            last_modified: Some(timestamp(node.mtime_ns)),
            e_tag: Some(etag),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        let bucket = req.input.bucket;
        with_fs(self.fs.clone(), move |fs| {
            bucket_node(fs, &bucket)?;
            Ok(S3Response::new(HeadBucketOutput::default()))
        })
        .await
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let input = req.input;
        let path = object_path(&input.bucket, &input.key)?;
        with_fs(self.fs.clone(), move |fs| {
            bucket_node(fs, &input.bucket)?;
            let node = lookup_path(fs, &path)?;
            if node.kind != Kind::File {
                return Err(s3_error!(NoSuchKey));
            }
            Ok(S3Response::new(HeadObjectOutput {
                content_length: Some(
                    i64::try_from(node.size).map_err(|_| s3_error!(InternalError))?,
                ),
                last_modified: Some(timestamp(node.mtime_ns)),
                e_tag: Some(object_etag(&node)?),
                ..Default::default()
            }))
        })
        .await
    }

    async fn list_buckets(
        &self,
        _req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        with_fs(self.fs.clone(), |fs| {
            let buckets = fs
                .list(1)
                .map_err(|err| s3_error!(InternalError, "list buckets: {err}"))?
                .into_iter()
                .filter(|entry| entry.node.kind == Kind::Directory)
                .map(|entry| {
                    let name = String::from_utf8(entry.name)
                        .map_err(|_| s3_error!(InternalError, "non-UTF8 bucket name"))?;
                    Ok(Bucket {
                        name: Some(name),
                        creation_date: Some(timestamp(entry.node.ctime_ns)),
                        ..Default::default()
                    })
                })
                .collect::<S3Result<Vec<_>>>()?;
            Ok(S3Response::new(ListBucketsOutput {
                buckets: Some(buckets),
                ..Default::default()
            }))
        })
        .await
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let input = req.input;
        let objects = with_fs(self.fs.clone(), {
            let bucket = input.bucket.clone();
            move |fs| list_bucket_objects(fs, &bucket)
        })
        .await?;
        let prefix = input.prefix.as_deref().unwrap_or("");
        let delimiter = input.delimiter.as_deref();
        let marker = match input.continuation_token.as_deref() {
            Some(token) => Some(token_key(token)?),
            None => None,
        }
        .into_iter()
        .chain(input.start_after.clone())
        .max();
        let mut entries = BTreeMap::<String, Option<Node>>::new();
        for (key, node) in objects {
            if !key.starts_with(prefix) {
                continue;
            }
            if let Some(common) = delimiter.and_then(|d| common_prefix(&key, prefix, d)) {
                entries.entry(common).or_insert(None);
            } else {
                entries.insert(key, Some(node));
            }
        }
        if let Some(marker) = marker {
            entries.retain(|key, _| key > &marker);
        }
        let max_keys = input.max_keys.unwrap_or(1000).clamp(0, 1000) as usize;
        let truncated = max_keys > 0 && entries.len() > max_keys;
        let page: Vec<_> = entries.into_iter().take(max_keys).collect();
        let next_token = if truncated {
            page.last().map(|(key, _)| continuation_token(key))
        } else {
            None
        };
        let mut content = Vec::new();
        let mut common = Vec::new();
        for (key, node) in &page {
            match node {
                Some(node) => content.push(Object {
                    key: Some(key.clone()),
                    size: Some(i64::try_from(node.size).map_err(|_| s3_error!(InternalError))?),
                    last_modified: Some(timestamp(node.mtime_ns)),
                    e_tag: Some(object_etag(node)?),
                    ..Default::default()
                }),
                None => common.push(CommonPrefix {
                    prefix: Some(key.clone()),
                }),
            }
        }
        let bucket = input.bucket;
        let output = ListObjectsV2Output {
            name: Some(bucket),
            prefix: input.prefix,
            delimiter: input.delimiter,
            max_keys: Some(max_keys as i32),
            key_count: Some(page.len() as i32),
            is_truncated: Some(truncated),
            continuation_token: input.continuation_token,
            next_continuation_token: next_token,
            contents: Some(content),
            common_prefixes: Some(common),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let input = req.input;
        let path = object_path(&input.bucket, &input.key)?;
        let content_length = input.content_length;
        with_fs(self.fs.clone(), {
            let bucket = input.bucket.clone();
            move |fs| {
                bucket_node(fs, &bucket)?;
                Ok(())
            }
        })
        .await?;
        let Some(mut body) = input.body else {
            return Err(s3_error!(IncompleteBody));
        };
        let tmp_dir = with_fs(self.fs.clone(), |fs| Ok(fs.repository().path().join("tmp"))).await?;
        let temporary = stage_body(&mut body, &tmp_dir, content_length).await?;
        let file = temporary
            .file
            .reopen()
            .map_err(|err| s3_error!(InternalError, "open staged request body: {err}"))?;
        let bucket = input.bucket.clone();
        let node = with_fs(self.fs.clone(), move |fs| {
            bucket_node(fs, &bucket)?;
            fs.mkdir_all(path.parent().unwrap_or_else(|| std::path::Path::new("/")))
                .map_err(|err| s3_error!(InternalError, "create S3 namespace: {err}"))?;
            fs.put_file(path, file)
                .map_err(|err| s3_error!(InternalError, "store S3 object: {err}"))
        })
        .await?;
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(object_etag(&node)?),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_keys_round_trip_without_path_traversal() {
        let long = format!("long/{}", "x".repeat(800));
        let keys = vec!["folder/a.bin", "../escape", "a//b", ".", long.as_str()];
        for key in keys {
            let path = object_path("bucket", key).unwrap();
            assert!(path.starts_with(format!("/bucket/{INTERNAL_OBJECT_DIR}")));
            assert!(
                path.components()
                    .all(|component| { component.as_os_str() != OsStr::new("..") })
            );
        }
    }

    #[test]
    fn delimiter_listing_rolls_up_nested_keys() {
        assert_eq!(
            common_prefix("logs/2026/a", "logs/", "/"),
            Some("logs/2026/".into())
        );
        assert_eq!(common_prefix("logs/a", "logs/", "/"), None);
    }
}
