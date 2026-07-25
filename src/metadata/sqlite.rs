use super::{MetadataStore, ObjectMetadata};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

pub struct SqliteMetadataStore {
    pool: SqlitePool,
}

impl SqliteMetadataStore {
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        let max_connections = if database_url.starts_with("sqlite::memory:") {
            1
        } else {
            5
        };
        let options = database_url
            .parse::<SqliteConnectOptions>()?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
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
        Ok(Self { pool })
    }
}

#[async_trait]
impl MetadataStore for SqliteMetadataStore {
    async fn set_secret_key(&self, access_key: &str, secret_key: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO access_keys (access_key, secret_key) VALUES (?, ?)
             ON CONFLICT(access_key) DO UPDATE SET secret_key = excluded.secret_key",
        )
        .bind(access_key)
        .bind(secret_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn secret_key(&self, access_key: &str) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT secret_key FROM access_keys WHERE access_key = ?")
                .bind(access_key)
                .fetch_optional(&self.pool)
                .await?,
        )
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
        let mut query_builder = QueryBuilder::<Sqlite>::new(
            "INSERT INTO object_metadata
                (namespace, bucket, object, metadata_key, metadata_value) ",
        );
        query_builder.push_values(metadata, |mut row, (key, value)| {
            row.push_bind(namespace)
                .push_bind(bucket)
                .push_bind(object)
                .push_bind(key)
                .push_bind(value);
        });
        query_builder.push(
            " ON CONFLICT(namespace, bucket, object, metadata_key)
              DO UPDATE SET metadata_value = excluded.metadata_value",
        );
        query_builder.build().execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn object_metadata(
        &self,
        namespace: &str,
        bucket: &str,
        object: &str,
    ) -> anyhow::Result<ObjectMetadata> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT metadata_key, metadata_value FROM object_metadata
             WHERE namespace = ? AND bucket = ? AND object = ?",
        )
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
        sqlx::query(
            "DELETE FROM object_metadata
             WHERE namespace = ? AND bucket = ? AND object = ?",
        )
        .bind(namespace)
        .bind(bucket)
        .bind(object)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn debug_keys(&self, pattern: &str) -> anyhow::Result<Vec<String>> {
        let like_pattern = pattern.replace('*', "%");
        Ok(sqlx::query_scalar(
            "SELECT 'secret_key::' || access_key FROM access_keys WHERE access_key LIKE ?",
        )
        .bind(like_pattern)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::{MetadataStore, ObjectMetadata, SqliteMetadataStore};
    use std::collections::HashMap;

    #[tokio::test]
    async fn sqlite_metadata_store_round_trips_secrets_and_object_metadata() {
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
    }
}
