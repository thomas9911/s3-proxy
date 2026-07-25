use super::MetadataStore;
use async_trait::async_trait;
use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;
use std::collections::HashMap;

pub struct RedisMetadataStore {
    pool: Pool,
}

impl RedisMetadataStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MetadataStore for RedisMetadataStore {
    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .set(format!("secret_key::{access_key}"), secret_key)
            .await?;
        Ok(())
    }

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection.get(format!("secret_key::{access_key}")).await?)
    }

    async fn set_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        metadata: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        if metadata.is_empty() {
            return Ok(());
        }
        let mut connection = self.pool.get().await?;
        let items: Vec<_> = metadata.iter().collect();
        let _: () = connection
            .hset_multiple(object_metadata_key(namespace, bucket, object), &items)
            .await?;
        Ok(())
    }

    async fn object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<HashMap<String, String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection
            .hgetall(object_metadata_key(namespace, bucket, object))
            .await?)
    }

    async fn debug_keys(&self, pattern: &str) -> anyhow::Result<Vec<String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection.keys(pattern).await?)
    }
}

fn object_metadata_key(namespace: &str, bucket: &str, object: &str) -> String {
    format!("object_metadata::{namespace}/{bucket}/{object}")
}
