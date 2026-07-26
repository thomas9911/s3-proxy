use async_trait::async_trait;
use std::collections::HashMap;

mod redis;
mod sql;

pub use redis::RedisMetadataStore;
pub use sql::{PostgresMetadataStore, SqliteMetadataStore};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub user_metadata: HashMap<String, String>,
}

impl ObjectMetadata {
    pub(crate) fn into_map(self) -> HashMap<String, String> {
        let mut metadata = self.user_metadata;
        if let Some(content_type) = self.content_type {
            metadata.insert("__content_type".to_string(), content_type);
        }
        if let Some(content_length) = self.content_length {
            metadata.insert("__content_length".to_string(), content_length.to_string());
        }
        if let Some(etag) = self.etag {
            metadata.insert("__etag".to_string(), etag);
        }
        metadata
    }

    pub(crate) fn from_map(mut metadata: HashMap<String, String>) -> Self {
        Self {
            content_type: metadata.remove("__content_type"),
            content_length: metadata
                .remove("__content_length")
                .and_then(|value| value.parse().ok()),
            etag: metadata.remove("__etag"),
            user_metadata: metadata,
        }
    }
}

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()>;

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>>;

    async fn set_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        metadata: &ObjectMetadata,
    ) -> anyhow::Result<()>;

    async fn object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<ObjectMetadata>;

    async fn delete_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<()>;

    async fn delete_many_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        objects: &[&str],
    ) -> anyhow::Result<()> {
        for object in objects {
            self.delete_object_metadata(namespace, bucket, object)
                .await?;
        }
        Ok(())
    }

    async fn debug_keys(&self, pattern: &str) -> anyhow::Result<Vec<String>>;

    async fn set_bucket_public(
        &self,
        namespace: &str,
        bucket: &str,
        public: bool,
    ) -> anyhow::Result<()>;

    async fn public_bucket_namespace(&self, bucket: &str) -> anyhow::Result<Option<String>>;

    async fn set_object_public(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        public: bool,
    ) -> anyhow::Result<()>;

    async fn public_object_namespace(
        &self,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<Option<String>>;

    async fn delete_public_bucket(&self, namespace: &str, bucket: &str) -> anyhow::Result<()>;
}
