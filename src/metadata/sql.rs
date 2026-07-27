use super::{MetadataStore, NamespaceOwner, ObjectMetadata};
use async_trait::async_trait;
use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, AssertSqlSafe};

pub struct SqlMetadataStore {
    pool: AnyPool,
    postgres: bool,
}

impl SqlMetadataStore {
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        let postgres =
            database_url.starts_with("postgres://") || database_url.starts_with("postgresql://");
        // SQLite serializes writes; PostgreSQL benefits from parallel request-level queries.
        let max_connections = if postgres { 20 } else { 1 };
        let pool = AnyPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS access_keys (
                access_key TEXT PRIMARY KEY,
                secret_key TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS namespace_owners (
                namespace TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                owner_id TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS object_metadata (
                namespace TEXT NOT NULL,
                bucket TEXT NOT NULL,
                object TEXT NOT NULL,
                metadata_key TEXT NOT NULL,
                metadata_value TEXT NOT NULL,
                PRIMARY KEY (namespace, bucket, object, metadata_key)
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS public_resources (
                resource_type TEXT NOT NULL,
                namespace TEXT NOT NULL,
                bucket TEXT NOT NULL,
                object TEXT NOT NULL,
                PRIMARY KEY (resource_type, bucket, object)
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS bucket_policies (
                namespace TEXT NOT NULL,
                bucket TEXT NOT NULL,
                policy TEXT NOT NULL,
                PRIMARY KEY (namespace, bucket)
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS metadata_schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        for index in [
            "CREATE INDEX IF NOT EXISTS idx_object_metadata_lookup ON object_metadata (namespace, bucket, object)",
            "CREATE INDEX IF NOT EXISTS idx_public_resources_lookup ON public_resources (resource_type, bucket, object)",
            "CREATE INDEX IF NOT EXISTS idx_bucket_policies_bucket ON bucket_policies (bucket)",
        ] {
            sqlx::query(index).execute(&pool).await?;
        }
        sqlx::query(
            "INSERT INTO metadata_schema_migrations (version, applied_at)
             VALUES (1, CURRENT_TIMESTAMP)
             ON CONFLICT(version) DO NOTHING",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool, postgres })
    }

    fn placeholder(&self, index: usize) -> String {
        if self.postgres {
            format!("${index}")
        } else {
            "?".to_string()
        }
    }

    async fn set_public_resource(
        &self,
        resource_type: &str,
        namespace: &str,
        bucket: &str,
        object: &str,
        public: bool,
    ) -> anyhow::Result<()> {
        if public {
            let query = format!(
                "INSERT INTO public_resources (resource_type, namespace, bucket, object)
                 VALUES ({}, {}, {}, {})
                 ON CONFLICT(resource_type, bucket, object)
                 DO UPDATE SET namespace = excluded.namespace",
                self.placeholder(1),
                self.placeholder(2),
                self.placeholder(3),
                self.placeholder(4),
            );
            sqlx::query(AssertSqlSafe(query))
                .bind(resource_type)
                .bind(namespace)
                .bind(bucket)
                .bind(object)
                .execute(&self.pool)
                .await?;
        } else {
            let query = format!(
                "DELETE FROM public_resources
                 WHERE resource_type = {} AND namespace = {} AND bucket = {} AND object = {}",
                self.placeholder(1),
                self.placeholder(2),
                self.placeholder(3),
                self.placeholder(4),
            );
            sqlx::query(AssertSqlSafe(query))
                .bind(resource_type)
                .bind(namespace)
                .bind(bucket)
                .bind(object)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    async fn public_resource_namespace(
        &self,
        resource_type: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<Option<String>> {
        let query = format!(
            "SELECT namespace FROM public_resources
             WHERE resource_type = {} AND bucket = {} AND object = {}
             LIMIT 1",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
        );
        Ok(sqlx::query_scalar(AssertSqlSafe(query))
            .bind(resource_type)
            .bind(bucket)
            .bind(object)
            .fetch_optional(&self.pool)
            .await?)
    }
}

#[async_trait]
impl MetadataStore for SqliteMetadataStore {
    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()> {
        let first = self.placeholder(1);
        let second = self.placeholder(2);
        let query = format!(
            "INSERT INTO access_keys (access_key, secret_key) VALUES ({first}, {second})
             ON CONFLICT(access_key) DO UPDATE SET secret_key = excluded.secret_key"
        );
        sqlx::query(AssertSqlSafe(query))
            .bind(access_key)
            .bind(secret_key)
            .execute(&self.pool)
            .await?;
        let owner_query = format!(
            "INSERT INTO namespace_owners (namespace, display_name, owner_id)
             VALUES ({}, {}, {}) ON CONFLICT(namespace) DO NOTHING",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
        );
        sqlx::query(AssertSqlSafe(owner_query))
            .bind(access_key)
            .bind(access_key)
            .bind(access_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT secret_key FROM access_keys WHERE access_key = {}",
            self.placeholder(1)
        )))
        .bind(access_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn set_namespace_owner(
        &self,
        namespace: &str,
        display_name: &str,
        id: &str,
    ) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO namespace_owners (namespace, display_name, owner_id)
             VALUES ({}, {}, {})
             ON CONFLICT(namespace) DO UPDATE SET
                 display_name = excluded.display_name,
                 owner_id = excluded.owner_id",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
        );
        sqlx::query(AssertSqlSafe(query))
            .bind(namespace)
            .bind(display_name)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn namespace_owner(&self, namespace: &str) -> anyhow::Result<NamespaceOwner> {
        let query = format!(
            "SELECT display_name, owner_id FROM namespace_owners WHERE namespace = {}",
            self.placeholder(1)
        );
        let owner = sqlx::query_as::<_, (String, String)>(AssertSqlSafe(query))
            .bind(namespace)
            .fetch_optional(&self.pool)
            .await?;
        Ok(owner.map_or_else(
            || NamespaceOwner {
                display_name: namespace.to_string(),
                id: namespace.to_string(),
            },
            |(display_name, id)| NamespaceOwner { display_name, id },
        ))
    }

    async fn set_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        metadata: &ObjectMetadata,
    ) -> anyhow::Result<()> {
        let _timer = super::operation_timer("sql.set_object_metadata");
        let metadata = metadata.clone().into_map();
        if metadata.is_empty() {
            return Ok(());
        }

        let mut transaction = self.pool.begin().await?;
        let mut values = String::new();
        for index in 0..metadata.len() {
            if index > 0 {
                values.push_str(", ");
            }
            let first = index * 5 + 1;
            values.push_str(&format!(
                "({}, {}, {}, {}, {})",
                self.placeholder(first),
                self.placeholder(first + 1),
                self.placeholder(first + 2),
                self.placeholder(first + 3),
                self.placeholder(first + 4),
            ));
        }
        let query_string = format!(
            "INSERT INTO object_metadata
                (namespace, bucket, object, metadata_key, metadata_value)
             VALUES {values}
             ON CONFLICT(namespace, bucket, object, metadata_key)
             DO UPDATE SET metadata_value = excluded.metadata_value"
        );
        let mut query = sqlx::query(AssertSqlSafe(query_string));
        for (key, value) in metadata {
            query = query
                .bind(namespace)
                .bind(bucket)
                .bind(object)
                .bind(key)
                .bind(value);
        }
        query.execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<ObjectMetadata> {
        let _timer = super::operation_timer("sql.object_metadata");
        let query = format!(
            "SELECT metadata_key, metadata_value FROM object_metadata
             WHERE namespace = {} AND bucket = {} AND object = {}",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
        );
        let rows = sqlx::query_as::<_, (String, String)>(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket)
            .bind(object)
            .fetch_all(&self.pool)
            .await?;
        Ok(ObjectMetadata::from_map(rows.into_iter().collect()))
    }

    async fn delete_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<()> {
        let _timer = super::operation_timer("sql.delete_object_metadata");
        let query = format!(
            "DELETE FROM object_metadata
             WHERE namespace = {} AND bucket = {} AND object = {}",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
        );
        sqlx::query(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket)
            .bind(object)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_many_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        objects: &[&str],
    ) -> anyhow::Result<()> {
        let _timer = super::operation_timer("sql.delete_many_object_metadata");
        if objects.is_empty() {
            return Ok(());
        }
        let placeholders = (0..objects.len())
            .map(|index| self.placeholder(index + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "DELETE FROM object_metadata
             WHERE namespace = {} AND bucket = {} AND object IN ({placeholders})",
            self.placeholder(1),
            self.placeholder(2),
        );
        let mut query = sqlx::query(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket);
        for object in objects {
            query = query.bind(object);
        }
        query.execute(&self.pool).await?;
        Ok(())
    }

    async fn debug_keys(&self, pattern: &str) -> anyhow::Result<Vec<String>> {
        let like_pattern = pattern.replace('*', "%");
        Ok(sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT 'secret_key::' || access_key FROM access_keys WHERE access_key LIKE {}",
            self.placeholder(1)
        )))
        .bind(like_pattern)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn set_bucket_public(
        &self,
        namespace: &str,
        bucket: &str,
        public: bool,
    ) -> anyhow::Result<()> {
        self.set_public_resource("bucket", namespace, bucket, "", public)
            .await
    }

    async fn public_bucket_namespace(&self, bucket: &str) -> anyhow::Result<Option<String>> {
        self.public_resource_namespace("bucket", bucket, "").await
    }

    async fn set_object_public(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        public: bool,
    ) -> anyhow::Result<()> {
        self.set_public_resource("object", namespace, bucket, object, public)
            .await
    }

    async fn public_object_namespace(
        &self,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<Option<String>> {
        self.public_resource_namespace("object", bucket, object)
            .await
    }

    async fn delete_public_bucket(&self, namespace: &str, bucket: &str) -> anyhow::Result<()> {
        let query = format!(
            "DELETE FROM public_resources WHERE namespace = {} AND bucket = {}",
            self.placeholder(1),
            self.placeholder(2),
        );
        sqlx::query(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_bucket_policy(
        &self,
        namespace: &str,
        bucket: &str,
        policy: &str,
    ) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO bucket_policies (namespace, bucket, policy)
             VALUES ({}, {}, {})
             ON CONFLICT(namespace, bucket) DO UPDATE SET policy = excluded.policy",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
        );
        sqlx::query(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket)
            .bind(policy)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn bucket_policy(&self, namespace: &str, bucket: &str) -> anyhow::Result<Option<String>> {
        let _timer = super::operation_timer("sql.bucket_policy");
        let query = format!(
            "SELECT policy FROM bucket_policies WHERE namespace = {} AND bucket = {}",
            self.placeholder(1),
            self.placeholder(2),
        );
        Ok(sqlx::query_scalar(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn bucket_policies(&self, bucket: &str) -> anyhow::Result<Vec<(String, String)>> {
        let _timer = super::operation_timer("sql.bucket_policies");
        let query = format!(
            "SELECT namespace, policy FROM bucket_policies WHERE bucket = {}",
            self.placeholder(1),
        );
        Ok(sqlx::query_as::<_, (String, String)>(AssertSqlSafe(query))
            .bind(bucket)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn delete_bucket_policy(&self, namespace: &str, bucket: &str) -> anyhow::Result<()> {
        let query = format!(
            "DELETE FROM bucket_policies WHERE namespace = {} AND bucket = {}",
            self.placeholder(1),
            self.placeholder(2),
        );
        sqlx::query(AssertSqlSafe(query))
            .bind(namespace)
            .bind(bucket)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

pub type SqliteMetadataStore = SqlMetadataStore;
pub type PostgresMetadataStore = SqlMetadataStore;

#[cfg(test)]
mod tests {
    use super::{MetadataStore, ObjectMetadata, SqliteMetadataStore};
    use std::collections::HashMap;

    #[tokio::test]
    async fn sql_metadata_store_round_trips_secrets_and_object_metadata() {
        let store = SqliteMetadataStore::connect("sqlite::memory:")
            .await
            .unwrap();
        store.set_secret_key("access", "secret").await.unwrap();
        assert_eq!(
            store.secret_key("access").await.unwrap().as_deref(),
            Some("secret")
        );
        assert_eq!(
            store.namespace_owner("access").await.unwrap(),
            super::NamespaceOwner {
                display_name: "access".to_string(),
                id: "access".to_string(),
            }
        );
        store
            .set_namespace_owner("access", "Testing", "owner-1")
            .await
            .unwrap();
        assert_eq!(
            store.namespace_owner("access").await.unwrap(),
            super::NamespaceOwner {
                display_name: "Testing".to_string(),
                id: "owner-1".to_string(),
            }
        );

        let metadata = ObjectMetadata {
            content_type: Some("text/plain".to_string()),
            content_length: Some(7),
            user_metadata: HashMap::from([(String::from("suite"), String::from("sqlite"))]),
            ..Default::default()
        };
        store
            .set_object_metadata("access", "bucket", "object", &metadata)
            .await
            .unwrap();
        assert_eq!(
            store
                .object_metadata("access", "bucket", "object")
                .await
                .unwrap(),
            metadata
        );
        store
            .delete_many_object_metadata("access", "bucket", &["object", "missing"])
            .await
            .unwrap();
        assert_eq!(
            store
                .object_metadata("access", "bucket", "object")
                .await
                .unwrap(),
            ObjectMetadata::default()
        );
    }
}
