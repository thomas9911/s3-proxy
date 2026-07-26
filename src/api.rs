mod buckets;
mod list;
mod objects;
mod post;

pub use buckets::{delete_bucket_route, list_buckets, put_bucket};
pub use list::get_bucket;
pub use objects::{delete_object_route, get_object, head_object, put_object};
pub use post::{post_bucket, post_object_route};
