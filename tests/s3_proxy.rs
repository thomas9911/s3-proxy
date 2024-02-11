use std::path::Path;
use std::process::{Child, Command};

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Bucket, Owner};
use aws_sdk_s3::Client;

fn setup() -> std::io::Result<Child> {
    // let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
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

    let body = ByteStream::from_path(Path::new("run.sh")).await;
    let put_object_res = client
        .put_object()
        .bucket("testing2")
        .key("run.sh")
        .body(body.unwrap())
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
    assert!(put_object_res.is_ok());
}
