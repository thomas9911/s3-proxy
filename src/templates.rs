use std::borrow::Cow;

use askama::Template;

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
