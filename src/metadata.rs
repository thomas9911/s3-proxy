use async_trait::async_trait;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "redis")]
mod redis;
mod sql;

#[cfg(feature = "redis")]
pub use redis::RedisMetadataStore;
#[cfg(feature = "postgres")]
pub use sql::PostgresMetadataStore;
pub use sql::SqliteMetadataStore;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaDataBackend {
    Sqlite,
    #[cfg(feature = "redis")]
    Redis,
    #[cfg(feature = "postgres")]
    Postgres,
}

impl MetaDataBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            #[cfg(feature = "redis")]
            Self::Redis => "redis",
            #[cfg(feature = "postgres")]
            Self::Postgres => "postgres",
        }
    }
}

pub(crate) struct OperationTimer {
    operation: &'static str,
    started: Instant,
}

impl Drop for OperationTimer {
    fn drop(&mut self) {
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        crate::metrics::record_metadata(elapsed_us);
        tracing::debug!(
            operation = self.operation,
            elapsed_us,
            "metadata operation completed"
        );
    }
}

pub(crate) fn operation_timer(operation: &'static str) -> OperationTimer {
    OperationTimer {
        operation,
        started: Instant::now(),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<SystemTime>,
    pub user_metadata: HashMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespaceOwner {
    pub display_name: String,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessKey {
    pub id: String,
    pub principal_id: String,
    pub status: AccessKeyStatus,
    pub secret_key: String,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKeyStatus {
    Active,
    Inactive,
}

impl AccessKeyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Inactive => "Inactive",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "Inactive" => Self::Inactive,
            _ => Self::Active,
        }
    }
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
        if let Some(last_modified) = self.last_modified {
            if let Ok(seconds) = last_modified.duration_since(UNIX_EPOCH) {
                metadata.insert("__last_modified".to_string(), seconds.as_secs().to_string());
            }
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
            last_modified: metadata
                .remove("__last_modified")
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds)),
            user_metadata: metadata,
        }
    }
}

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_access_key(
        &self,
        access_key: &str,
        secret_key: &str,
        principal_id: &str,
    ) -> anyhow::Result<bool>;

    async fn delete_access_key(&self, access_key: &str) -> anyhow::Result<bool>;

    async fn access_key(&self, access_key: &str) -> anyhow::Result<Option<AccessKey>>;

    async fn list_access_keys(&self, principal_id: &str) -> anyhow::Result<Vec<AccessKey>>;

    async fn set_access_key_status(
        &self,
        access_key: &str,
        status: AccessKeyStatus,
    ) -> anyhow::Result<bool>;

    async fn record_access_key_use(&self, access_key: &str) -> anyhow::Result<()>;

    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()>;

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>>;

    async fn set_namespace_owner(
        &self,
        namespace: &str,
        display_name: &str,
        id: &str,
    ) -> anyhow::Result<()>;

    async fn namespace_owner(&self, namespace: &str) -> anyhow::Result<NamespaceOwner>;

    async fn list_namespace_owners(&self) -> anyhow::Result<Vec<(String, NamespaceOwner)>>;

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

    async fn set_bucket_policy(
        &self,
        namespace: &str,
        bucket: &str,
        policy: &str,
    ) -> anyhow::Result<()>;

    async fn bucket_policy(&self, namespace: &str, bucket: &str) -> anyhow::Result<Option<String>>;

    async fn bucket_policies(&self, bucket: &str) -> anyhow::Result<Vec<(String, String)>>;

    async fn delete_bucket_policy(&self, namespace: &str, bucket: &str) -> anyhow::Result<()>;
}
