use async_trait::async_trait;
use std::collections::HashMap;

mod redis;
mod sqlite;

pub use redis::RedisMetadataStore;
pub use sqlite::SqliteMetadataStore;

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()>;

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>>;

    async fn set_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        metadata: &HashMap<String, String>,
    ) -> anyhow::Result<()>;

    async fn object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<HashMap<String, String>>;

    async fn debug_keys(&self, pattern: &str) -> anyhow::Result<Vec<String>>;
}
