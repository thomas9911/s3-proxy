use askama::Template;
use askama_web::WebTemplate;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use serde::Deserialize;
use std::borrow::Cow;

pub(crate) fn xml_response<T: Template>(status: StatusCode, template: T) -> Response<Body> {
    let body = template.render().expect("XML template rendering failed");
    Response::builder()
        .status(status)
        .header("content-type", "application/xml")
        .body(Body::from(body))
        .expect("static XML response headers are valid")
}

#[derive(Debug, Template)]
#[template(path = "error.xml")]
pub struct ErrorTemplate<'a> {
    pub code: &'a str,
    pub message: &'a str,
}

#[derive(Debug, Template)]
#[template(path = "initiate_multipart.xml")]
pub struct InitiateMultipartTemplate<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub upload_id: &'a str,
}

#[derive(Debug, Template)]
#[template(path = "complete_multipart.xml")]
pub struct CompleteMultipartTemplate<'a> {
    pub location: &'a str,
    pub bucket: &'a str,
    pub key: &'a str,
}

#[derive(Debug)]
pub struct ListPartItem {
    pub part_number: u32,
    pub size: u64,
}

#[derive(Debug, Template)]
#[template(path = "list_parts.xml")]
pub struct ListPartsTemplate<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub upload_id: &'a str,
    pub parts: &'a [ListPartItem],
}

#[derive(Debug, Template)]
#[template(path = "copy_object.xml")]
pub struct CopyObjectTemplate;

#[derive(Debug, Template)]
#[template(path = "invalid_range.xml")]
pub struct InvalidRangeTemplate;

#[derive(Debug)]
pub struct ListBucketItem<'a> {
    pub name: Cow<'a, str>,
    pub timestamp: Option<Cow<'a, str>>,
}

#[derive(Debug, Template, WebTemplate)]
#[template(path = "list_buckets.xml")]
pub struct ListBucketsTemplate<'a> {
    pub owner_name: &'a str,
    pub owner_id: &'a str,
    pub buckets: Vec<ListBucketItem<'a>>,
}

#[derive(Debug)]
pub struct ListObjectItem<'a> {
    pub etag: Option<Cow<'a, str>>,
    pub key: Cow<'a, str>,
    pub last_modified: Option<Cow<'a, str>>,
    pub size: u64,
}

#[derive(Debug)]
pub struct ListCommonPrefix<'a> {
    pub prefix: Cow<'a, str>,
}

#[derive(Debug, Template, WebTemplate)]
#[template(path = "list_objects.xml")]
pub struct ListObjectsTemplate<'a> {
    pub is_truncated: bool,
    pub continuation_token: Cow<'a, str>,
    pub next_continuation_token: Cow<'a, str>,
    pub key_count: u64,
    pub bucket_name: Cow<'a, str>,
    pub prefix: Cow<'a, str>,
    pub max_keys: u64,
    pub objects: &'a [ListObjectItem<'a>],
    pub common_prefixes: &'a [ListCommonPrefix<'a>],
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Delete")]
pub struct DeleteObjectsRequest {
    #[serde(rename = "Object", default)]
    pub objects: Vec<DeleteObjectIdentifier>,
    #[serde(rename = "Quiet", default)]
    pub quiet: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeleteObjectIdentifier {
    pub key: String,
}

#[derive(Debug, Template, WebTemplate)]
#[template(path = "delete_objects.xml")]
pub struct DeleteObjectsTemplate<'a> {
    pub deleted: &'a [String],
    pub errors: &'a [DeleteObjectError],
}

#[derive(Debug)]
pub struct DeleteObjectError {
    pub key: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct CreateBucket {
    location_constraint: Option<String>,
    location: Option<CreateBucketLocation>,
    bucket: Option<CreateBucketBucket>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct CreateBucketLocation {
    name: Option<String>,
    r#type: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct CreateBucketBucket {
    data_redundancy: Option<String>,
    r#type: Option<String>,
}

#[test]
fn renders_list_buckets_xml() {
    let owner_name = "example";
    let owner_id = "1234567890";
    let buckets: Vec<ListBucketItem<'static>> = vec![ListBucketItem {
        name: "bucket1".into(),
        timestamp: None,
    }];
    let template = ListBucketsTemplate {
        owner_name,
        owner_id,
        buckets,
    };
    let template_str = template.render().expect("Unable to render template");
    assert!(template_str.contains("1234567890"));
    assert!(template_str.contains("example"));
    assert!(template_str.contains("bucket1"));
}

#[test]
fn renders_list_objects_xml() {
    let objects: Vec<ListObjectItem<'static>> = vec![
        ListObjectItem {
            etag: Some("fba9dede5f27731c9771645a39863328".into()),
            key: "example1.jpg".into(),
            last_modified: Some("2019-10-12T17:50:30.000Z".into()),
            size: 1234,
        },
        ListObjectItem {
            etag: None,
            key: "example2.jpg".into(),
            last_modified: None,
            size: 1234,
        },
    ];
    let template = ListObjectsTemplate {
        is_truncated: false,
        continuation_token: "".into(),
        next_continuation_token: "".into(),
        key_count: 2,
        bucket_name: "bucket1".into(),
        prefix: "".into(),
        max_keys: 1000,
        objects: &objects,
        common_prefixes: &[],
    };
    let template_str = template.render().expect("Unable to render template");
    assert!(template_str.contains("fba9dede5f27731c9771645a39863328"));
    assert!(template_str.contains("2019-10-12T17:50:30.000Z"));
    assert!(template_str.contains("1234"));
    assert!(template_str.contains("example1.jpg"));
    assert!(template_str.contains("example2.jpg"));
    assert!(template_str.contains("bucket1"));
}

#[test]
fn loads_create_bucket_xml() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
    <CreateBucketConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
       <LocationConstraint>string</LocationConstraint>
       <Location>
          <Name>string</Name>
          <Type>string</Type>
       </Location>
       <Bucket>
          <DataRedundancy>string</DataRedundancy>
          <Type>string</Type>
       </Bucket>
    </CreateBucketConfiguration>"#;

    let body: CreateBucket = quick_xml::de::from_str(xml).unwrap();

    let expected = CreateBucket {
        location_constraint: Some("string".to_string()),
        location: Some(CreateBucketLocation {
            name: Some("string".to_string()),
            r#type: Some("string".to_string()),
        }),
        bucket: Some(CreateBucketBucket {
            data_redundancy: Some("string".to_string()),
            r#type: Some("string".to_string()),
        }),
    };

    assert_eq!(body, expected);
}
