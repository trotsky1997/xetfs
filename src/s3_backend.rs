use crate::{Error, Result, hash, xorb};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{Client, primitives::ByteStream};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    sync::mpsc::{self, Sender, SyncSender},
    thread::{self, JoinHandle},
};
use tokio::runtime::{Builder, Runtime};

const BACKEND_CONFIG: &str = "s3-backend.json";

/// Repository-local S3 configuration. Credentials always come from the AWS SDK credential chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct S3BackendConfig {
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub path_style: bool,
}

fn default_region() -> String {
    "us-east-1".to_owned()
}

impl S3BackendConfig {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: default_region(),
            prefix: String::new(),
            endpoint_url: None,
            path_style: false,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.bucket.is_empty()
            || self.bucket.len() > 63
            || !self
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return Err(Error::Invalid("invalid S3 bucket name".into()));
        }
        if self.region.is_empty() {
            return Err(Error::Invalid("S3 region must not be empty".into()));
        }
        if let Some(endpoint) = &self.endpoint_url
            && !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        {
            return Err(Error::Invalid(
                "S3 endpoint URL must use http:// or https://".into(),
            ));
        }
        Ok(())
    }
}

type S3Task = Box<dyn FnOnce(&Runtime, &Client) + Send + 'static>;

pub(crate) struct S3ObjectStore {
    tasks: Sender<S3Task>,
    _worker: JoinHandle<()>,
    bucket: String,
    prefix: String,
}

impl S3ObjectStore {
    pub(crate) fn from_config(config: &S3BackendConfig) -> Result<Self> {
        config.validate()?;
        let region = config.region.clone();
        let endpoint = config.endpoint_url.clone();
        let path_style = config.path_style || endpoint.is_some();
        let (tasks, task_rx) = mpsc::channel::<S3Task>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<std::result::Result<(), String>>(1);
        let worker = thread::Builder::new()
            .name("xetfs-s3-object-store".to_owned())
            .spawn(move || {
                let runtime = match Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("xetfs-s3-runtime")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                let shared = runtime.block_on(
                    aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region))
                        .load(),
                );
                let mut builder =
                    aws_sdk_s3::config::Builder::from(&shared).force_path_style(path_style);
                if let Some(endpoint) = endpoint {
                    builder = builder.endpoint_url(endpoint);
                }
                let client = Client::from_conf(builder.build());
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(task) = task_rx.recv() {
                    task(&runtime, &client);
                }
            })
            .map_err(|err| Error::Backend(format!("start S3 object-store worker: {err}")))?;
        ready_rx
            .recv()
            .map_err(|_| Error::Backend("S3 object-store worker stopped during startup".into()))?
            .map_err(|err| Error::Backend(format!("initialize AWS SDK: {err}")))?;
        Ok(Self {
            tasks,
            _worker: worker,
            bucket: config.bucket.clone(),
            prefix: config.prefix.trim_matches('/').to_owned(),
        })
    }

    fn call<T: Send + 'static>(
        &self,
        task: impl FnOnce(&Runtime, &Client) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (result_tx, result_rx): (SyncSender<Result<T>>, _) = mpsc::sync_channel(1);
        self.tasks
            .send(Box::new(move |runtime, client| {
                let _ = result_tx.send(task(runtime, client));
            }))
            .map_err(|_| Error::Backend("S3 object-store worker stopped".into()))?;
        result_rx
            .recv()
            .map_err(|_| Error::Backend("S3 object-store worker stopped during request".into()))?
    }

    fn key(&self, id: &str) -> String {
        if self.prefix.is_empty() {
            format!("objects/{id}.xorb")
        } else {
            format!("{}/objects/{id}.xorb", self.prefix)
        }
    }

    pub(crate) fn check_bucket(&self) -> Result<()> {
        let bucket = self.bucket.clone();
        self.call(move |runtime, client| {
            runtime
                .block_on(client.head_bucket().bucket(bucket).send())
                .map(|_| ())
                .map_err(|err| Error::Backend(format!("check S3 bucket: {err}")))
        })
    }

    pub(crate) fn put_xorb(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let bucket = self.bucket.clone();
        let key = self.key(id);
        let id = id.to_owned();
        let bytes = bytes.to_vec();
        self.call(move |runtime, client| {
            runtime
                .block_on(
                    client
                        .put_object()
                        .bucket(bucket)
                        .key(key)
                        .body(ByteStream::from(bytes))
                        .send(),
                )
                .map(|_| ())
                .map_err(|err| Error::Backend(format!("upload Xorb {id} to S3: {err}")))
        })
    }

    pub(crate) fn get_xorb(&self, id: &str) -> Result<Vec<u8>> {
        let bucket = self.bucket.clone();
        let key = self.key(id);
        let id = id.to_owned();
        self.call(move |runtime, client| {
            runtime.block_on(async {
                let output = client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|err| Error::Backend(format!("download Xorb {id} from S3: {err}")))?;
                if output
                    .content_length()
                    .is_some_and(|len| len < 0 || len as usize > xorb::MAX_XORB)
                {
                    return Err(Error::Corrupt(format!("invalid S3 Xorb length for {id}")));
                }
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|err| Error::Backend(format!("read S3 Xorb {id}: {err}")))?;
                let bytes = body.into_bytes().to_vec();
                if bytes.len() > xorb::MAX_XORB {
                    return Err(Error::Corrupt(format!("oversized S3 Xorb for {id}")));
                }
                Ok(bytes)
            })
        })
    }

    pub(crate) fn delete_xorb(&self, id: &str) -> Result<()> {
        let bucket = self.bucket.clone();
        let key = self.key(id);
        let id = id.to_owned();
        self.call(move |runtime, client| {
            runtime
                .block_on(client.delete_object().bucket(bucket).key(key).send())
                .map(|_| ())
                .map_err(|err| Error::Backend(format!("delete Xorb {id} from S3: {err}")))
        })
    }

    pub(crate) fn list_xorbs(&self) -> Result<Vec<String>> {
        let bucket = self.bucket.clone();
        let prefix = if self.prefix.is_empty() {
            "objects/".to_owned()
        } else {
            format!("{}/objects/", self.prefix)
        };
        self.call(move |runtime, client| {
            runtime.block_on(async move {
                let mut token = None;
                let mut ids = Vec::new();
                loop {
                    let mut request = client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .prefix(&prefix)
                        .max_keys(1000);
                    if let Some(value) = token.as_deref() {
                        request = request.continuation_token(value);
                    }
                    let page = request
                        .send()
                        .await
                        .map_err(|err| Error::Backend(format!("list S3 Xorbs: {err}")))?;
                    for object in page.contents() {
                        if let Some(key) = object.key()
                            && let Some(id) = key
                                .strip_prefix(&prefix)
                                .and_then(|s| s.strip_suffix(".xorb"))
                            && hash::from_hex(id).is_ok()
                        {
                            ids.push(id.to_owned());
                        }
                    }
                    token = page.next_continuation_token().map(str::to_owned);
                    if token.is_none() {
                        break;
                    }
                }
                Ok(ids)
            })
        })
    }
}

pub(crate) fn read_config(root: &Path) -> Result<Option<S3BackendConfig>> {
    let path = root.join(BACKEND_CONFIG);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let config: S3BackendConfig = serde_json::from_slice(&bytes)
        .map_err(|err| Error::Backend(format!("invalid S3 backend config: {err}")))?;
    config.validate()?;
    Ok(Some(config))
}

pub(crate) fn write_config(root: &Path, config: &S3BackendConfig) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(config)
        .map_err(|err| Error::Backend(format!("serialize S3 backend config: {err}")))?;
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    use std::io::Write;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(root.join(BACKEND_CONFIG))
        .map_err(|err| err.error)?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

pub(crate) fn verify_and_cache_xorb(
    root: &Path,
    id: &str,
    expected_size: Option<u64>,
    object_store: Option<&S3ObjectStore>,
) -> Result<std::path::PathBuf> {
    let path = root.join("objects").join(format!("{id}.xorb"));
    if path.is_file() {
        return Ok(path);
    }
    let store = object_store
        .ok_or_else(|| Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
    let bytes = store.get_xorb(id)?;
    if expected_size.is_some_and(|size| size != bytes.len() as u64) {
        return Err(Error::Corrupt(format!("S3 Xorb length mismatch for {id}")));
    }
    verify_xorb_bytes(id, &bytes)?;
    let mut temporary = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
    use std::io::Write;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|err| Error::Io(err.error))?;
    fs::File::open(root.join("objects"))?.sync_all()?;
    Ok(path)
}

pub(crate) fn verify_xorb_bytes(id: &str, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > xorb::MAX_XORB {
        return Err(Error::Corrupt(format!("invalid Xorb length for {id}")));
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let mut chunks = Vec::new();
    while let Some((data, _)) = xorb::decode(&mut cursor)? {
        chunks.push((hash::chunk_hash(&data), data.len() as u64));
    }
    if chunks.is_empty()
        || cursor.position() != bytes.len() as u64
        || hash::to_hex(&hash::xorb_hash(&chunks)) != id
    {
        return Err(Error::Corrupt(format!("Xorb hash mismatch for {id}")));
    }
    Ok(())
}
