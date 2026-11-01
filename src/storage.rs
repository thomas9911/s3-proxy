use crate::{Config, StorageLayout};

pub fn bucket_prefix(config: &Config, namespace: &str, bucket: &str) -> Option<String> {
    match config.storage_layout {
        StorageLayout::Namespaced => Some(format!("{namespace}/{bucket}/")),
        StorageLayout::SingleBucket => config
            .single_bucket
            .as_ref()
            .filter(|single| single.namespace == namespace && single.name == bucket)
            .map(|_| String::new()),
    }
}

pub fn object_path(config: &Config, namespace: &str, bucket: &str, key: &str) -> Option<String> {
    bucket_prefix(config, namespace, bucket).map(|prefix| format!("{prefix}{key}"))
}

pub fn is_single_bucket(config: &Config) -> bool {
    matches!(config.storage_layout, StorageLayout::SingleBucket)
}

pub fn internal_path(
    config: &Config,
    namespace: &str,
    bucket: &str,
    suffix: &str,
) -> Option<String> {
    match config.storage_layout {
        StorageLayout::Namespaced => Some(format!("{namespace}/.s3-proxy/{suffix}")),
        StorageLayout::SingleBucket => {
            bucket_prefix(config, namespace, bucket).map(|_| format!(".s3-proxy/{suffix}"))
        }
    }
}
