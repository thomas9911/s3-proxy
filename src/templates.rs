use askama::Template;
use serde::Deserialize;
use std::borrow::Cow;

pub struct ListBucketItem<'a> {
    pub name: Cow<'a, str>,
    pub timestamp: Option<Cow<'a, str>>,
}

#[derive(Template)]
#[template(path = "list_buckets.xml")]
pub struct ListBucketsTemplate<'a> {
    pub owner_name: &'a str,
    pub owner_id: &'a str,
    pub buckets: Vec<ListBucketItem<'a>>,
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
fn test_template_parsing() {
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
