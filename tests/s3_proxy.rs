use std::process::{Child, Command};

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::{
    config::Region,
    primitives::DateTime,
    types::{Bucket, Owner},
    Client,
};

fn setup() -> std::io::Result<Child> {
    // let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
    let path = assert_cmd::cargo::cargo_bin(env!("CARGO_PKG_NAME"));

    let process = Command::new(path)
        .env("S3_PROXY__REDIS__URL", "redis://127.0.0.1:6379")
        .spawn();

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

    let out = client.list_buckets().send().await.unwrap();

    let buckets = out.buckets();
    let expected_buckets = vec![
        Bucket::builder()
            .set_name(Some("testing1".to_string()))
            .set_creation_date(Some(DateTime::from_secs(1706911595)))
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

    process.kill().expect("command couldn't be killed");

    assert_eq!(buckets, expected_buckets);
    assert_eq!(owner, Some(&expected_owner));
}
