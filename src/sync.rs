use crate::metadata::ObjectMetadata;
use crate::storage;
use crate::AppState;
use tokio_stream::StreamExt;

#[derive(Debug, Default)]
pub struct SyncOptions {
    pub namespace: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub namespaces: usize,
    pub buckets: usize,
    pub objects: usize,
}

/// Import storage-derived metadata from the proxy's `namespace/bucket/key` layout.
pub async fn sync_metadata(state: &AppState, options: &SyncOptions) -> anyhow::Result<SyncReport> {
    if storage::is_single_bucket(&state.config) {
        return sync_single_bucket(state, options).await;
    }
    let namespaces = match options.namespace.as_deref() {
        Some(namespace) => vec![namespace.to_string()],
        None => directories(&state.opendal_operator, "").await?,
    };
    let known_namespaces = state
        .metadata_store
        .list_namespace_owners()
        .await?
        .into_iter()
        .map(|(namespace, _)| namespace)
        .collect::<std::collections::HashSet<_>>();
    let mut report = SyncReport::default();

    for namespace in namespaces {
        if !valid_component(&namespace) || namespace == ".s3-proxy" {
            continue;
        }
        if !state
            .opendal_operator
            .exists(&format!("{namespace}/"))
            .await?
        {
            anyhow::bail!("namespace `{namespace}` does not exist in the storage backend")
        }
        report.namespaces += 1;
        if !known_namespaces.contains(&namespace) && !options.dry_run {
            state
                .metadata_store
                .set_namespace_owner(&namespace, &namespace, &namespace)
                .await?;
        }

        for bucket in directories(&state.opendal_operator, &format!("{namespace}/")).await? {
            if !valid_bucket_name(&bucket) {
                tracing::warn!(%namespace, %bucket, "skipping invalid bucket name during metadata sync");
                continue;
            }
            report.buckets += 1;
            let bucket_prefix = format!("{namespace}/{bucket}/");
            let mut lister = state
                .opendal_operator
                .lister_with(&bucket_prefix)
                .recursive(true)
                .await?;
            while let Some(entry) = lister.next().await {
                let entry = entry?;
                if !entry.metadata().is_file() {
                    continue;
                }
                let object = entry
                    .path()
                    .strip_prefix(&bucket_prefix)
                    .unwrap_or(entry.path());
                if object.is_empty() || object.starts_with(".s3-proxy/") {
                    continue;
                }
                report.objects += 1;
                if options.dry_run {
                    continue;
                }
                let storage_metadata = entry.metadata();
                let metadata = ObjectMetadata {
                    content_type: storage_metadata.content_type().map(ToOwned::to_owned),
                    content_length: Some(storage_metadata.content_length()),
                    etag: storage_metadata.etag().map(ToOwned::to_owned),
                    last_modified: storage_metadata.last_modified().map(Into::into),
                    ..Default::default()
                };
                // OpenDAL is authoritative for imported objects; remove stale values first.
                state
                    .metadata_store
                    .delete_object_metadata(&namespace, &bucket, object)
                    .await?;
                state
                    .metadata_store
                    .set_object_metadata(&namespace, &bucket, object, &metadata)
                    .await?;
            }
        }
    }
    Ok(report)
}

async fn sync_single_bucket(state: &AppState, options: &SyncOptions) -> anyhow::Result<SyncReport> {
    let single = state
        .config
        .single_bucket
        .as_ref()
        .expect("validated at startup");
    if let Some(namespace) = options
        .namespace
        .as_deref()
        .filter(|namespace| *namespace != single.namespace)
    {
        anyhow::bail!("namespace `{namespace}` is not configured for the single-bucket layout")
    }
    let mut report = SyncReport {
        namespaces: 1,
        buckets: 1,
        objects: 0,
    };
    if !options.dry_run {
        state
            .metadata_store
            .set_namespace_owner(&single.namespace, &single.namespace, &single.namespace)
            .await?;
    }
    let mut lister = state
        .opendal_operator
        .lister_with("")
        .recursive(true)
        .await?;
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if !entry.metadata().is_file() || entry.path().starts_with(".s3-proxy/") {
            continue;
        }
        report.objects += 1;
        if options.dry_run {
            continue;
        }
        let source = entry.metadata();
        let metadata = ObjectMetadata {
            content_type: source.content_type().map(ToOwned::to_owned),
            content_length: Some(source.content_length()),
            etag: source.etag().map(ToOwned::to_owned),
            last_modified: source.last_modified().map(Into::into),
            ..Default::default()
        };
        state
            .metadata_store
            .delete_object_metadata(&single.namespace, &single.name, entry.path())
            .await?;
        state
            .metadata_store
            .set_object_metadata(&single.namespace, &single.name, entry.path(), &metadata)
            .await?;
    }
    Ok(report)
}

async fn directories(operator: &opendal::Operator, prefix: &str) -> anyhow::Result<Vec<String>> {
    let mut lister = operator.lister_with(prefix).await?;
    let mut directories = Vec::new();
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if !entry.metadata().is_dir() {
            continue;
        }
        let name = entry
            .path()
            .strip_prefix(prefix)
            .unwrap_or(entry.path())
            .trim_end_matches('/');
        if valid_component(name) {
            directories.push(name.to_string());
        }
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value != "."
        && value != ".."
}

fn valid_bucket_name(name: &str) -> bool {
    (3..=63).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && name
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
}

#[cfg(test)]
mod tests {
    use super::{sync_metadata, SyncOptions};
    use crate::metadata::{MetaDataBackend, MetadataStore, SqliteMetadataStore};
    use crate::{AppState, Config, SqliteConfig};
    use opendal::services::Memory;
    use opendal::Operator;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn imports_objects_from_the_namespaced_storage_layout() {
        let metadata_store = Arc::new(
            SqliteMetadataStore::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let operator = Operator::new(Memory::default()).unwrap();
        operator.create_dir("admin/photos/").await.unwrap();
        operator
            .write("admin/photos/cat.jpg", b"cat".to_vec())
            .await
            .unwrap();
        let state = AppState {
            metadata_store: metadata_store.clone(),
            config: Arc::new(Config {
                server_host: "127.0.0.1:0".to_string(),
                external_server_host: "http://127.0.0.1:0".to_string(),
                max_request_body_bytes: 256 * 1024 * 1024,
                metadata_backend: MetaDataBackend::Sqlite,
                redis: None,
                sqlite: Some(SqliteConfig {
                    url: "sqlite::memory:".to_string(),
                }),
                postgres: None,
                admin: None,
                #[cfg(feature = "management")]
                management: None,
                quotas: crate::quota::QuotaConfig::default(),
                opendal_provider: "memory".to_string(),
                opendal: HashMap::new(),
                storage_layout: crate::StorageLayout::Namespaced,
                single_bucket: None,
            }),
            opendal_operator: operator,
        };

        let report = sync_metadata(&state, &SyncOptions::default())
            .await
            .unwrap();

        assert_eq!(report.namespaces, 1);
        assert_eq!(report.buckets, 1);
        assert_eq!(report.objects, 1);
        assert_eq!(
            metadata_store
                .object_metadata("admin", "photos", "cat.jpg")
                .await
                .unwrap()
                .content_length,
            Some(3)
        );
        assert_eq!(
            metadata_store.list_namespace_owners().await.unwrap().len(),
            1
        );
    }
}
