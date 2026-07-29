use super::{MetadataStore, NamespaceOwner, ObjectMetadata};
use async_trait::async_trait;
use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;

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
    async fn create_access_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<bool> {
        let mut connection = self.pool.get().await?;
        let created: bool = connection
            .set_nx(format!("secret_key::{access_key}"), secret_key)
            .await?;
        if created {
            let _: () = connection
                .hset_multiple(
                    namespace_owner_key(access_key),
                    &[("display_name", access_key), ("owner_id", access_key)],
                )
                .await?;
        }
        Ok(created)
    }

    async fn delete_access_key(&self, access_key: &str) -> anyhow::Result<bool> {
        let mut connection = self.pool.get().await?;
        let deleted: i32 = connection.del(format!("secret_key::{access_key}")).await?;
        if deleted == 1 {
            let _: () = connection.del(namespace_owner_key(access_key)).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .set(format!("secret_key::{access_key}"), secret_key)
            .await?;
        let owner_key = namespace_owner_key(access_key);
        if !connection.exists(&owner_key).await? {
            let _: () = connection
                .hset_multiple(
                    owner_key,
                    &[("display_name", access_key), ("owner_id", access_key)],
                )
                .await?;
        }
        Ok(())
    }

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection.get(format!("secret_key::{access_key}")).await?)
    }

    async fn set_namespace_owner(
        &self,
        namespace: &str,
        display_name: &str,
        id: &str,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .hset_multiple(
                namespace_owner_key(namespace),
                &[("display_name", display_name), ("owner_id", id)],
            )
            .await?;
        Ok(())
    }

    async fn namespace_owner(&self, namespace: &str) -> anyhow::Result<NamespaceOwner> {
        let mut connection = self.pool.get().await?;
        let owner: std::collections::HashMap<String, String> =
            connection.hgetall(namespace_owner_key(namespace)).await?;
        Ok(NamespaceOwner {
            display_name: owner
                .get("display_name")
                .cloned()
                .unwrap_or_else(|| namespace.to_string()),
            id: owner
                .get("owner_id")
                .cloned()
                .unwrap_or_else(|| namespace.to_string()),
        })
    }

    async fn set_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        metadata: &ObjectMetadata,
    ) -> anyhow::Result<()> {
        let _timer = super::operation_timer("redis.set_object_metadata");
        let metadata = metadata.clone().into_map();
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
    ) -> anyhow::Result<ObjectMetadata> {
        let _timer = super::operation_timer("redis.object_metadata");
        let mut connection = self.pool.get().await?;
        Ok(ObjectMetadata::from_map(
            connection
                .hgetall(object_metadata_key(namespace, bucket, object))
                .await?,
        ))
    }

    async fn delete_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<()> {
        let _timer = super::operation_timer("redis.delete_object_metadata");
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .del(object_metadata_key(namespace, bucket, object))
            .await?;
        Ok(())
    }

    async fn delete_many_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        objects: &[&str],
    ) -> anyhow::Result<()> {
        let _timer = super::operation_timer("redis.delete_many_object_metadata");
        if objects.is_empty() {
            return Ok(());
        }
        let mut connection = self.pool.get().await?;
        let mut pipeline = deadpool_redis::redis::pipe();
        for object in objects {
            pipeline.del(object_metadata_key(namespace, bucket, object));
        }
        let _: Vec<i32> = pipeline.query_async(&mut connection).await?;
        Ok(())
    }

    async fn debug_keys(&self, pattern: &str) -> anyhow::Result<Vec<String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection.keys(pattern).await?)
    }

    async fn set_bucket_public(
        &self,
        namespace: &str,
        bucket: &str,
        public: bool,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let key = public_bucket_key(bucket);
        if public {
            let _: () = connection.set(key, namespace).await?;
        } else {
            let _: () = connection.del(key).await?;
        }
        Ok(())
    }

    async fn public_bucket_namespace(&self, bucket: &str) -> anyhow::Result<Option<String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection.get(public_bucket_key(bucket)).await?)
    }

    async fn set_object_public(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        public: bool,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let key = public_object_key(bucket, object);
        if public {
            let _: () = connection.set(key, namespace).await?;
        } else {
            let _: () = connection.del(key).await?;
        }
        Ok(())
    }

    async fn public_object_namespace(
        &self,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<Option<String>> {
        let mut connection = self.pool.get().await?;
        Ok(connection.get(public_object_key(bucket, object)).await?)
    }

    async fn delete_public_bucket(&self, _namespace: &str, bucket: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let keys: Vec<String> = connection.keys(public_object_key(bucket, "*")).await?;
        if !keys.is_empty() {
            let _: Vec<i32> = deadpool_redis::redis::pipe()
                .del(keys)
                .del(public_bucket_key(bucket))
                .query_async(&mut connection)
                .await?;
        } else {
            let _: () = connection.del(public_bucket_key(bucket)).await?;
        }
        Ok(())
    }

    async fn set_bucket_policy(
        &self,
        namespace: &str,
        bucket: &str,
        policy: &str,
    ) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .set(bucket_policy_key(namespace, bucket), policy)
            .await?;
        Ok(())
    }

    async fn bucket_policy(&self, namespace: &str, bucket: &str) -> anyhow::Result<Option<String>> {
        let _timer = super::operation_timer("redis.bucket_policy");
        let mut connection = self.pool.get().await?;
        Ok(connection.get(bucket_policy_key(namespace, bucket)).await?)
    }

    async fn bucket_policies(&self, bucket: &str) -> anyhow::Result<Vec<(String, String)>> {
        let _timer = super::operation_timer("redis.bucket_policies");
        let mut connection = self.pool.get().await?;
        let keys: Vec<String> = connection
            .keys(format!("bucket_policy::*/{bucket}"))
            .await?;
        let mut policies = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(policy) = connection.get::<_, Option<String>>(&key).await? else {
                continue;
            };
            if let Some(namespace) = key
                .strip_prefix("bucket_policy::")
                .and_then(|value| value.strip_suffix(&format!("/{bucket}")))
            {
                policies.push((namespace.to_string(), policy));
            }
        }
        Ok(policies)
    }

    async fn delete_bucket_policy(&self, namespace: &str, bucket: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let _: () = connection.del(bucket_policy_key(namespace, bucket)).await?;
        Ok(())
    }
}

fn object_metadata_key(namespace: &str, bucket: &str, object: &str) -> String {
    format!("object_metadata::{namespace}/{bucket}/{object}")
}

fn namespace_owner_key(namespace: &str) -> String {
    format!("namespace_owner::{namespace}")
}

fn public_bucket_key(bucket: &str) -> String {
    format!("public_bucket::{bucket}")
}

fn public_object_key(bucket: &str, object: &str) -> String {
    format!("public_object::{bucket}/{object}")
}

fn bucket_policy_key(namespace: &str, bucket: &str) -> String {
    format!("bucket_policy::{namespace}/{bucket}")
}
