use std::process::{Child, Command};
use std::str::FromStr;

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Bucket, Owner};
use aws_sdk_s3::Client;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// `setup()` is used to prepare the environment and spawn the child process for the test cases.
async fn setup() -> anyhow::Result<Child> {
    let access_key = "ANOTREAL";
    let secret_key = "notrealrnrELgWzOk3IfjzDKtFBhDby";
    let database_name = format!("s3-proxy-test-{}.db", std::process::id());
    let database_url = format!("sqlite://target/{database_name}");
    let options = SqliteConnectOptions::from_str(&database_url)?.create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS access_keys (
            access_key TEXT PRIMARY KEY,
            secret_key TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO access_keys (access_key, secret_key) VALUES (?, ?)
         ON CONFLICT(access_key) DO UPDATE SET secret_key = excluded.secret_key",
    )
    .bind(access_key)
    .bind(secret_key)
    .execute(&pool)
    .await?;
    pool.close().await;

    let path = assert_cmd::cargo::cargo_bin(env!("CARGO_PKG_NAME"));

    let process = Command::new(path)
        .env("S3_PROXY__METADATA_BACKEND", "sqlite")
        .env("S3_PROXY__SQLITE__URL", &database_url)
        .env("S3_PROXY__EXTERNAL_SERVER_HOST", "http://127.0.0.1:3000")
        .env("S3_PROXY__OPENDAL_PROVIDER", "memory")
        .env("S3_PROXY__OPENDAL__ROOT", "/tmp")
        .spawn();

    Ok(process?)
}

#[tokio::test]
async fn test_it_runs() {
    let mut process = setup().await.unwrap();

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

    let body = ByteStream::from_static(b"s3-proxy integration test");
    let put_object_res = client
        .put_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .content_type("application/toml")
        .body(body)
        .send()
        .await;

    let list_object_res = client.list_objects().bucket("testing2").send().await;

    let get_object_res = client
        .get_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .send()
        .await;

    process.kill().expect("command couldn't be killed");
    process.wait().expect("command couldn't be waited on");
    let _ = std::fs::remove_file(format!("target/s3-proxy-test-{}.db", std::process::id()));

    let out = list_bucket_res.unwrap();

    let buckets = out.buckets();
    let expected_buckets = vec![
        Bucket::builder()
            .set_name(Some("testing".to_string()))
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

    let _response = list_object_res.unwrap();
    let response = get_object_res.unwrap();
    let content_type = response.content_type();
    let content_length = response.content_length();
    assert_eq!(Some("application/toml"), content_type);
    assert!(content_length.is_some());
    let body = String::from_utf8(response.body.collect().await.unwrap().to_vec()).unwrap();
    assert!(body.contains("s3-proxy"));
}
