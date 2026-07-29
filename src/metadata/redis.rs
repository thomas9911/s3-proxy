use super::{AccessKey, AccessKeyStatus, MetadataStore, NamespaceOwner, ObjectMetadata};
use async_trait::async_trait;
use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;
use std::collections::HashSet;

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
    async fn create_access_key(
        &self,
        access_key: &str,
        secret_key: &str,
        principal_id: &str,
    ) -> anyhow::Result<bool> {
        let mut connection = self.pool.get().await?;
        let created: bool = connection
            .set_nx(format!("secret_key::{access_key}"), secret_key)
            .await?;
        if created {
            let _: () = connection
                .hset_multiple(
                    access_key_record_key(access_key),
                    &[
                        ("secret_key", secret_key),
                        ("principal_id", principal_id),
                        ("status", "Active"),
                        ("created_at", &now()),
                    ],
                )
                .await?;
            let _: () = connection
                .hset_multiple(
                    namespace_owner_key(principal_id),
                    &[("display_name", principal_id), ("owner_id", principal_id)],
                )
                .await?;
        }
        Ok(created)
    }

    async fn delete_access_key(&self, access_key: &str) -> anyhow::Result<bool> {
        let mut connection = self.pool.get().await?;
        let deleted: i32 = connection.del(format!("secret_key::{access_key}")).await?;
        if deleted == 1 {
            let _: () = connection.del(access_key_record_key(access_key)).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn access_key(&self, access_key: &str) -> anyhow::Result<Option<AccessKey>> {
        let mut connection = self.pool.get().await?;
        let record: std::collections::HashMap<String, String> = connection
            .hgetall(access_key_record_key(access_key))
            .await?;
        if !record.is_empty() {
            return Ok(Some(access_key_from_hash(access_key, record)));
        }
        Ok(connection
            .get::<_, Option<String>>(format!("secret_key::{access_key}"))
            .await?
            .map(|secret_key| AccessKey {
                id: access_key.to_string(),
                principal_id: access_key.to_string(),
                status: AccessKeyStatus::Active,
                secret_key,
                created_at: None,
                last_used_at: None,
            }))
    }

    async fn list_access_keys(&self, principal_id: &str) -> anyhow::Result<Vec<AccessKey>> {
        let mut connection = self.pool.get().await?;
        let keys: Vec<String> = connection.keys("access_key::*").await?;
        let mut access_keys = Vec::new();
        let mut known_access_keys = HashSet::new();
        for key in keys {
            let record: std::collections::HashMap<String, String> =
                connection.hgetall(&key).await?;
            let Some(access_key) = key.strip_prefix("access_key::") else {
                continue;
            };
            let record = access_key_from_hash(access_key, record);
            if record.principal_id == principal_id {
                known_access_keys.insert(record.id.clone());
                access_keys.push(record);
            }
        }
        let legacy_keys: Vec<String> = connection.keys("secret_key::*").await?;
        for key in legacy_keys {
            let Some(access_key) = key.strip_prefix("secret_key::") else {
                continue;
            };
            if known_access_keys.contains(access_key) || access_key != principal_id {
                continue;
            }
            let Some(secret_key) = connection.get::<_, Option<String>>(&key).await? else {
                continue;
            };
            access_keys.push(AccessKey {
                id: access_key.to_string(),
                principal_id: principal_id.to_string(),
                status: AccessKeyStatus::Active,
                secret_key,
                created_at: None,
                last_used_at: None,
            });
        }
        access_keys.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then(left.id.cmp(&right.id))
        });
        Ok(access_keys)
    }

    async fn set_access_key_status(
        &self,
        access_key: &str,
        status: AccessKeyStatus,
    ) -> anyhow::Result<bool> {
        let Some(record) = self.access_key(access_key).await? else {
            return Ok(false);
        };
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .hset(access_key_record_key(access_key), "status", status.as_str())
            .await?;
        if record.created_at.is_none() {
            let _: () = connection
                .hset_multiple(
                    access_key_record_key(access_key),
                    &[
                        ("secret_key", record.secret_key.as_str()),
                        ("principal_id", record.principal_id.as_str()),
                        ("created_at", now().as_str()),
                    ],
                )
                .await?;
        }
        Ok(true)
    }

    async fn record_access_key_use(&self, access_key: &str) -> anyhow::Result<()> {
        let Some(record) = self.access_key(access_key).await? else {
            return Ok(());
        };
        let mut connection = self.pool.get().await?;
        let now = now();
        let _: () = connection
            .hset_multiple(
                access_key_record_key(access_key),
                &[
                    ("secret_key", record.secret_key.as_str()),
                    ("principal_id", record.principal_id.as_str()),
                    ("status", record.status.as_str()),
                    ("created_at", record.created_at.as_deref().unwrap_or(&now)),
                    ("last_used_at", now.as_str()),
                ],
            )
            .await?;
        Ok(())
    }

    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()> {
        let mut connection = self.pool.get().await?;
        let _: () = connection
            .set(format!("secret_key::{access_key}"), secret_key)
            .await?;
        let _: () = connection
            .hset_multiple(
                access_key_record_key(access_key),
                &[
                    ("secret_key", secret_key),
                    ("principal_id", access_key),
                    ("status", "Active"),
                    ("created_at", now().as_str()),
                ],
            )
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
        Ok(self
            .access_key(access_key)
            .await?
            .filter(|record| record.status == AccessKeyStatus::Active)
            .map(|record| record.secret_key))
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

    async fn list_namespace_owners(&self) -> anyhow::Result<Vec<(String, NamespaceOwner)>> {
        let mut connection = self.pool.get().await?;
        let keys: Vec<String> = connection.keys("namespace_owner::*").await?;
        let mut owners = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(namespace) = key.strip_prefix("namespace_owner::") else {
                continue;
            };
            let fields: std::collections::HashMap<String, String> =
                connection.hgetall(&key).await?;
            owners.push((
                namespace.to_string(),
                NamespaceOwner {
                    display_name: fields
                        .get("display_name")
                        .cloned()
                        .unwrap_or_else(|| namespace.to_string()),
                    id: fields
                        .get("owner_id")
                        .cloned()
                        .unwrap_or_else(|| namespace.to_string()),
                },
            ));
        }
        owners.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(owners)
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

fn access_key_record_key(access_key: &str) -> String {
    format!("access_key::{access_key}")
}

fn access_key_from_hash(
    access_key: &str,
    record: std::collections::HashMap<String, String>,
) -> AccessKey {
    AccessKey {
        id: access_key.to_string(),
        principal_id: record
            .get("principal_id")
            .cloned()
            .unwrap_or_else(|| access_key.to_string()),
        status: record
            .get("status")
            .map(String::as_str)
            .map(AccessKeyStatus::parse)
            .unwrap_or(AccessKeyStatus::Active),
        secret_key: record.get("secret_key").cloned().unwrap_or_default(),
        created_at: record.get("created_at").cloned(),
        last_used_at: record.get("last_used_at").cloned(),
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible")
}
