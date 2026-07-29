use std::collections::HashMap;

use aws_config::meta::region::RegionProviderChain;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Bucket, CompletedPart, Delete, ObjectIdentifier, Owner};
use aws_sdk_s3::Client;
use s3_proxy::metadata::MetaDataBackend;
use s3_proxy::{build_app, AdminConfig, AppState, Config, ManagementConfig, SqliteConfig};

#[tokio::test]
async fn test_it_runs_in_process() {
    let config = Config {
        server_host: "127.0.0.1:0".to_string(),
        external_server_host: "http://127.0.0.1:0".to_string(),
        max_request_body_bytes: 256 * 1024 * 1024,
        metadata_backend: MetaDataBackend::Sqlite,
        redis: None,
        sqlite: Some(SqliteConfig {
            url: "sqlite::memory:".to_string(),
        }),
        postgres: None,
        admin: Some(AdminConfig {
            access_key: "ANOTREAL".to_string(),
            secret_key: "notrealrnrELgWzOk3IfjzDKtFBhDby".to_string(),
        }),
        management: Some(ManagementConfig {
            username: "dashboard".to_string(),
            password: "dashboard-secret".to_string(),
        }),
        quotas: s3_proxy::quota::QuotaConfig::default(),
        opendal_provider: "memory".to_string(),
        opendal: HashMap::new(),
    };
    let state = AppState::from_config(config).await.unwrap();
    state
        .metadata_store
        .set_namespace_owner("ANOTREAL", "Testing", "1")
        .await
        .unwrap();

    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let region_provider = RegionProviderChain::first_try(Region::new("us-west-2"));
    let shared_config = aws_config::from_env()
        .region(region_provider)
        .credentials_provider(Credentials::new(
            "ANOTREAL",
            "notrealrnrELgWzOk3IfjzDKtFBhDby",
            None,
            None,
            "test",
        ))
        .endpoint_url(format!("http://{address}"))
        .load()
        .await;
    let client = Client::new(&shared_config);

    client
        .create_bucket()
        .bucket("testing")
        .send()
        .await
        .unwrap();
    client
        .create_bucket()
        .bucket("testing2")
        .send()
        .await
        .unwrap();
    let bucket_policy = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:ListBucket","Resource":"arn:aws:s3:::testing2"}]}"#;
    client
        .put_bucket_policy()
        .bucket("testing2")
        .policy(bucket_policy)
        .send()
        .await
        .unwrap();
    let stored_policy = client
        .get_bucket_policy()
        .bucket("testing2")
        .send()
        .await
        .unwrap();
    assert_eq!(stored_policy.policy(), Some(bucket_policy));
    let list_bucket_res = client.list_buckets().send().await.unwrap();

    let put_object_res = client
        .put_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .content_type("application/toml")
        .body(ByteStream::from_static(b"s3-proxy integration test"))
        .send()
        .await;
    let list_object_res = client
        .list_objects()
        .bucket("testing2")
        .send()
        .await
        .unwrap();
    let delimited_list = client
        .list_objects_v2()
        .bucket("testing2")
        .prefix("nested/")
        .delimiter("/")
        .send()
        .await
        .unwrap();
    let get_object_res = client
        .get_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .send()
        .await
        .unwrap();
    let head_object_res = client
        .head_object()
        .bucket("testing2")
        .key("Cargo.toml")
        .send()
        .await
        .unwrap();

    let out = list_bucket_res;
    let buckets = out.buckets();
    let expected_buckets = vec![
        Bucket::builder()
            .set_name(Some("testing".to_string()))
            .build(),
        Bucket::builder()
            .set_name(Some("testing2".to_string()))
            .build(),
    ];
    assert_eq!(buckets, expected_buckets);
    assert_eq!(
        out.owner(),
        Some(
            &Owner::builder()
                .set_display_name(Some("Testing".to_string()))
                .set_id(Some("1".to_string()))
                .build()
        )
    );
    put_object_res.unwrap();
    assert_eq!(list_object_res.contents().len(), 1);
    assert!(delimited_list.contents().is_empty());
    assert_eq!(head_object_res.content_type(), Some("application/toml"));
    assert_eq!(get_object_res.content_type(), Some("application/toml"));
    assert!(get_object_res.content_length().is_some());
    let body = String::from_utf8(get_object_res.body.collect().await.unwrap().to_vec()).unwrap();
    assert!(body.contains("s3-proxy"));

    client
        .copy_object()
        .bucket("testing2")
        .key("copied.toml")
        .copy_source("testing2/Cargo.toml")
        .send()
        .await
        .unwrap();
    client
        .delete_object()
        .bucket("testing2")
        .key("copied.toml")
        .send()
        .await
        .unwrap();

    let multipart = client
        .create_multipart_upload()
        .bucket("testing2")
        .key("multipart.bin")
        .send()
        .await
        .unwrap();
    let upload_id = multipart.upload_id().unwrap();
    let part = client
        .upload_part()
        .bucket("testing2")
        .key("multipart.bin")
        .upload_id(upload_id)
        .part_number(1)
        .body(ByteStream::from_static(b"multipart"))
        .send()
        .await
        .unwrap();
    client
        .complete_multipart_upload()
        .bucket("testing2")
        .key("multipart.bin")
        .upload_id(upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .parts(
                    CompletedPart::builder()
                        .part_number(1)
                        .e_tag(part.e_tag().unwrap())
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    client
        .delete_objects()
        .bucket("testing2")
        .delete(
            Delete::builder()
                .objects(
                    ObjectIdentifier::builder()
                        .key("Cargo.toml")
                        .build()
                        .unwrap(),
                )
                .objects(
                    ObjectIdentifier::builder()
                        .key("multipart.bin")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    client
        .delete_bucket_policy()
        .bucket("testing2")
        .send()
        .await
        .unwrap();
    client
        .delete_bucket()
        .bucket("testing2")
        .send()
        .await
        .unwrap();
    client
        .delete_bucket()
        .bucket("testing")
        .send()
        .await
        .unwrap();

    server.abort();
    let _ = server.await;
}
