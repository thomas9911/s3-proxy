mod buckets;
mod list;
#[cfg(feature = "management")]
mod management;
mod objects;
mod post;

pub use access_keys::create_access_key;
pub use buckets::{delete_bucket_route, list_buckets, put_bucket};
pub use list::get_bucket;
#[cfg(feature = "management")]
pub use management::{
    management_abort_multipart_upload, management_audit, management_buckets,
    management_buckets_fragment, management_create_access_key, management_create_bucket,
    management_create_bucket_fragment, management_dashboard, management_delete_access_key,
    management_delete_bucket, management_delete_object, management_download_object,
    management_inspect, management_inspect_fragment, management_list_multipart_uploads,
    management_list_objects, management_list_objects_fragment, management_metrics,
    management_presign_object, management_presign_post, management_principals,
    management_principals_fragment, management_quota, management_rotate_access_key,
    management_status, management_status_fragment, management_update_access_key,
    management_update_bucket_configuration, management_update_quota, management_upload_object,
};
pub use objects::{delete_object_route, get_object, head_object, put_object};
pub use post::{post_bucket, post_object_route};

mod access_keys;
