use std::path::Path;
use std::process::{Child, Command};

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Bucket, Owner};
use aws_sdk_s3::Client;

/// `setup()` is used to prepare the environment and spawn the child process for the test cases.
fn setup() -> std::io::Result<Child> {
    let path = assert_cmd::cargo::cargo_bin(env!("CARGO_PKG_NAME"));

    let process = Command::new(path)
        .env("S3_PROXY__REDIS__URL", "redis://127.0.0.1:6379")
        .env("S3_PROXY__OPENDAL_PROVIDER", "memory")
        .env("S3_PROXY__OPENDAL__ROOT", "/tmp")
        .spawn();

    // tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).init();

    process
}

#[tokio::test]
async fn test_it_runs() {
    let mut process = setup().unwrap();

    let region_provider = RegionProviderChain::first_try(Region::new("us-west-2"));

    let shared_config = aws_config::from_env()
        .region(region_provider)
        .test_credentials()
        .endpoint_url("http://127.0.0.1:3000")
        .load()
        .await;
    let client = Client::new(&shared_config);

    let create_bucket_req1 = client.create_bucket().bucket("testing");
    let create_bucket_req2 = client.create_bucket().bucket("testing2");
    let list_bucket_req = client.list_buckets();

    let _ = create_bucket_req1.send().await;
    let _ = create_bucket_req2.send().await;
    let list_bucket_res = list_bucket_req.send().await;

    let body = ByteStream::from_path(Path::new("Cargo.toml")).await;
    let put_object_res = client
        .put_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .content_type("application/toml")
        .body(body.unwrap())
        .send()
        .await;

    let get_object_res = client
        .get_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .send()
        .await;

    process.kill().expect("command couldn't be killed");

    let out = list_bucket_res.unwrap();

    let buckets = out.buckets();
    let expected_buckets = vec![
        Bucket::builder()
            .set_name(Some("testing".to_string()))
            // .set_creation_date(Some(DateTime::from_secs(1706911595)))
            .build(),
        Bucket::builder()
            .set_name(Some("testing2".to_string()))
            .build(),
    ];

    let owner = out.owner();
    let expected_owner = Owner::builder()
        .set_display_name(Some("Testing".to_string()))
        .set_id(Some("1".to_string()))
        .build();

    assert_eq!(buckets, expected_buckets);
    assert_eq!(owner, Some(&expected_owner));
    put_object_res.unwrap();

    let response = get_object_res.unwrap();
    let content_type = response.content_type();
    let content_length = response.content_length();
    assert_eq!(Some("application/toml"), content_type);
    assert!(content_length.is_some());
    let body = String::from_utf8(response.body.collect().await.unwrap().to_vec()).unwrap();
    assert!(body.contains("s3-proxy"));
}
