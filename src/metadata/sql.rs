use super::{MetadataStore, ObjectMetadata};
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
        Ok(Self { pool, postgres })
    }

    fn placeholder(&self, index: usize) -> String {
        if self.postgres {
            format!("${index}")
        } else {
            "?".to_string()
        }
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

    async fn set_object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
        metadata: &ObjectMetadata,
    ) -> anyhow::Result<()> {
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
