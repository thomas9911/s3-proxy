use askama::Template;
use serde::Deserialize;
use std::borrow::Cow;

#[derive(Debug)]
pub struct ListBucketItem<'a> {
    pub name: Cow<'a, str>,
    pub timestamp: Option<Cow<'a, str>>,
}

#[derive(Debug, Template)]
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

#[derive(Debug, Template)]
#[template(path = "list_objects.xml")]
pub struct ListObjectsTemplate<'a> {
    pub is_truncated: bool,
    pub marker: Cow<'a, str>,
    pub next_marker: Cow<'a, str>,
    pub bucket_name: Cow<'a, str>,
    pub prefix: Cow<'a, str>,
    pub max_keys: u64,
    pub objects: Vec<ListObjectItem<'a>>,
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
        marker: "".into(),
        next_marker: "".into(),
        bucket_name: "bucket1".into(),
        prefix: "".into(),
        max_keys: 1000,
        objects,
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
