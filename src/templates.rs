use askama::Template;

pub struct ListBucketItem<'a> {
    pub name: &'a str,
    pub timestamp: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "list_buckets.xml")]
pub struct ListBucketsTemplate<'a> {
    pub owner_name: &'a str,
    pub owner_id: &'a str,
    pub buckets: Vec<ListBucketItem<'a>>,
}
