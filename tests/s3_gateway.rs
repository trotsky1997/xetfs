use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ACCESS_KEY: &str = "xetfs-integration-access";
const SECRET_KEY: &str = "xetfs-integration-secret-key";

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_xetfs")
}

fn run_cli(repo: &Path, args: &[&str]) {
    let output = Command::new(binary())
        .arg("--repo")
        .arg(repo)
        .args(args)
        .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .output()
        .expect("run xetfs CLI");
    assert!(
        output.status.success(),
        "xetfs {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn start_server(repo: &Path) -> (Server, SocketAddr) {
    run_cli(repo, &["init"]);
    let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve local port");
    let address = reservation.local_addr().expect("read reserved address");
    drop(reservation);
    let child = Command::new(binary())
        .arg("--repo")
        .arg(repo)
        .args(["s3", "serve", "--listen", &address.to_string()])
        .env("XETFS_S3_ACCESS_KEY_ID", ACCESS_KEY)
        .env("XETFS_S3_SECRET_ACCESS_KEY", SECRET_KEY)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start local S3 gateway");
    let server = Server(child);
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(address).is_ok() {
            break;
        }
        assert!(Instant::now() < until, "S3 gateway did not start");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    (server, address)
}

fn client(address: SocketAddr) -> Client {
    let credentials = Credentials::new(ACCESS_KEY, SECRET_KEY, None, None, "xetfs-integration");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(credentials)
        .endpoint_url(format!("http://{address}"))
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

#[tokio::test]
async fn signed_s3_gateway_and_remote_xorb_backend_round_trip() {
    let scratch = tempfile::tempdir().expect("create temporary test root");
    let gateway_repo = scratch.path().join("gateway.repo");
    let (server, address) = start_server(&gateway_repo).await;
    let client = client(address);

    client
        .create_bucket()
        .bucket("s3-gateway-test")
        .send()
        .await
        .expect("create S3 bucket");
    client
        .put_object()
        .bucket("s3-gateway-test")
        .key("folder/object.bin")
        .body(ByteStream::from(b"0123456789".to_vec()))
        .send()
        .await
        .expect("put S3 object");
    client
        .put_object()
        .bucket("s3-gateway-test")
        .key("../outside//safe.bin")
        .body(ByteStream::from(b"still-in-bucket".to_vec()))
        .send()
        .await
        .expect("put path-like S3 key");
    let head = client
        .head_object()
        .bucket("s3-gateway-test")
        .key("folder/object.bin")
        .send()
        .await
        .expect("head S3 object");
    assert_eq!(head.content_length(), Some(10));
    let ranged = client
        .get_object()
        .bucket("s3-gateway-test")
        .key("folder/object.bin")
        .range("bytes=2-5")
        .send()
        .await
        .expect("range get S3 object");
    assert_eq!(
        ranged
            .body
            .collect()
            .await
            .expect("collect body")
            .into_bytes()
            .as_ref(),
        b"2345"
    );
    let listed = client
        .list_objects_v2()
        .bucket("s3-gateway-test")
        .prefix("folder/")
        .send()
        .await
        .expect("list S3 objects");
    assert_eq!(listed.contents().len(), 1);
    let traversal_key = client
        .get_object()
        .bucket("s3-gateway-test")
        .key("../outside//safe.bin")
        .send()
        .await
        .expect("get path-like S3 key");
    assert_eq!(
        traversal_key
            .body
            .collect()
            .await
            .expect("collect path-like key")
            .into_bytes()
            .as_ref(),
        b"still-in-bucket"
    );

    let initiated = client
        .create_multipart_upload()
        .bucket("s3-gateway-test")
        .key("multipart.bin")
        .send()
        .await
        .expect("start multipart upload");
    let upload_id = initiated.upload_id().expect("upload id");
    let part_one_bytes = vec![b'p'; 5 * 1024 * 1024];
    let part_two_bytes = b"part-two".to_vec();
    let part_one = client
        .upload_part()
        .bucket("s3-gateway-test")
        .key("multipart.bin")
        .upload_id(upload_id)
        .part_number(1)
        .body(ByteStream::from(part_one_bytes.clone()))
        .send()
        .await
        .expect("upload first part");
    let part_two = client
        .upload_part()
        .bucket("s3-gateway-test")
        .key("multipart.bin")
        .upload_id(upload_id)
        .part_number(2)
        .body(ByteStream::from(part_two_bytes.clone()))
        .send()
        .await
        .expect("upload second part");
    let parts = vec![
        CompletedPart::builder()
            .part_number(1)
            .e_tag(part_one.e_tag().expect("part one ETag"))
            .build(),
        CompletedPart::builder()
            .part_number(2)
            .e_tag(part_two.e_tag().expect("part two ETag"))
            .build(),
    ];
    let completed = CompletedMultipartUpload::builder()
        .set_parts(Some(parts))
        .build();
    client
        .complete_multipart_upload()
        .bucket("s3-gateway-test")
        .key("multipart.bin")
        .upload_id(upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("complete multipart upload");
    let multipart = client
        .get_object()
        .bucket("s3-gateway-test")
        .key("multipart.bin")
        .send()
        .await
        .expect("read multipart object");
    let mut expected_multipart = part_one_bytes;
    expected_multipart.extend_from_slice(&part_two_bytes);
    assert_eq!(
        multipart
            .body
            .collect()
            .await
            .expect("collect multipart")
            .into_bytes()
            .as_ref(),
        expected_multipart.as_slice()
    );
    client
        .delete_object()
        .bucket("s3-gateway-test")
        .key("folder/object.bin")
        .send()
        .await
        .expect("delete S3 object");
    client
        .delete_object()
        .bucket("s3-gateway-test")
        .key("../outside//safe.bin")
        .send()
        .await
        .expect("delete path-like S3 key");
    client
        .delete_object()
        .bucket("s3-gateway-test")
        .key("multipart.bin")
        .send()
        .await
        .expect("delete multipart object");
    client
        .delete_bucket()
        .bucket("s3-gateway-test")
        .send()
        .await
        .expect("delete empty S3 bucket");

    client
        .create_bucket()
        .bucket("xetfs-cas")
        .send()
        .await
        .expect("create remote CAS bucket");
    let repository = scratch.path().join("backend.repo");
    run_cli(&repository, &["init"]);
    let source = scratch.path().join("payload.bin");
    let destination = scratch.path().join("recovered.bin");
    let payload = b"XetFS remote S3 CAS round-trip".repeat(4096);
    std::fs::write(&source, &payload).expect("write test payload");
    let source_string = source.to_string_lossy().to_string();
    run_cli(&repository, &["put", &source_string, "/payload.bin"]);
    let endpoint = format!("http://{address}");
    let output = Command::new(binary())
        .arg("--repo")
        .arg(&repository)
        .args([
            "s3",
            "configure-backend",
            "xetfs-cas",
            "--region",
            "us-east-1",
            "--prefix",
            "team-a/xetfs",
            "--endpoint-url",
            &endpoint,
            "--path-style",
        ])
        .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .output()
        .expect("configure S3 object backend");
    assert!(
        output.status.success(),
        "configure backend failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for entry in std::fs::read_dir(repository.join("objects")).expect("list local Xorbs") {
        let entry = entry.expect("read Xorb entry");
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "xorb")
        {
            std::fs::remove_file(entry.path()).expect("remove local Xorb cache");
        }
    }
    let destination_string = destination.to_string_lossy().to_string();
    run_cli(&repository, &["get", "/payload.bin", &destination_string]);
    assert_eq!(
        std::fs::read(&destination).expect("read recovered payload"),
        payload
    );
    run_cli(&repository, &["fsck"]);
    run_cli(&repository, &["rm", "/payload.bin"]);
    run_cli(&repository, &["gc"]);
    let leftovers = client
        .list_objects_v2()
        .bucket("xetfs-cas")
        .prefix("team-a/xetfs/objects/")
        .send()
        .await
        .expect("list backend after gc");
    assert!(leftovers.contents().is_empty());
    drop(server);
}
