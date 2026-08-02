use crate::Config;
use base64::Engine as _;
use opendal::Operator;
use rand::random;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BucketVersioning {
    #[default]
    Off,
    Enabled,
    Suspended,
}

impl BucketVersioning {
    pub fn as_s3_status(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Enabled => Some("Enabled"),
            Self::Suspended => Some("Suspended"),
        }
    }

    pub fn parse_s3_status(value: &str) -> Option<Self> {
        match value {
            "Enabled" => Some(Self::Enabled),
            "Suspended" => Some(Self::Suspended),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectVersion {
    pub version_id: String,
    pub is_delete_marker: bool,
    pub created_at: String,
    pub data_path: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct VersionManifest {
    versions: Vec<ObjectVersion>,
}

pub async fn bucket_versioning(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
) -> anyhow::Result<BucketVersioning> {
    let path = crate::storage::internal_path(
        config,
        namespace,
        bucket,
        &format!("buckets/{}/versioning.json", hex::encode(bucket)),
    )
    .expect("validated bucket layout");
    if !operator.exists(&path).await? {
        return Ok(BucketVersioning::Off);
    }
    Ok(serde_json::from_slice(
        &operator.read(&path).await?.to_vec(),
    )?)
}

pub async fn set_bucket_versioning(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    status: BucketVersioning,
) -> anyhow::Result<()> {
    operator
        .write(
            &crate::storage::internal_path(
                config,
                namespace,
                bucket,
                &format!("buckets/{}/versioning.json", hex::encode(bucket)),
            )
            .expect("validated bucket layout"),
            serde_json::to_vec(&status)?,
        )
        .await?;
    Ok(())
}

pub async fn delete_bucket_state(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
) -> anyhow::Result<()> {
    let bucket_id = hex::encode(bucket);
    for path in [
        internal_path(config, namespace, bucket, &format!("buckets/{bucket_id}/")),
        internal_path(config, namespace, bucket, &format!("versions/{bucket_id}/")),
        internal_path(
            config,
            namespace,
            bucket,
            &format!("version-data/{bucket_id}/"),
        ),
    ] {
        if operator.exists(&path).await? {
            operator.delete_with(&path).recursive(true).await?;
        }
    }
    Ok(())
}

pub async fn record_put(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
    current_path: &str,
) -> anyhow::Result<Option<String>> {
    let status = bucket_versioning(operator, config, namespace, bucket).await?;
    if status == BucketVersioning::Off {
        return Ok(None);
    }
    let version_id = match status {
        BucketVersioning::Enabled => new_version_id(),
        BucketVersioning::Suspended => "null".to_string(),
        BucketVersioning::Off => unreachable!(),
    };
    let data_path = version_data_path(config, namespace, bucket, object, &version_id);
    copy(operator, current_path, &data_path).await?;
    let mut manifest = object_manifest(operator, config, namespace, bucket, object).await?;
    manifest
        .versions
        .retain(|version| version.version_id != version_id);
    manifest.versions.insert(
        0,
        ObjectVersion {
            version_id: version_id.clone(),
            is_delete_marker: false,
            created_at: now(),
            data_path: Some(data_path),
        },
    );
    write_manifest(operator, config, namespace, bucket, object, &manifest).await?;
    Ok(Some(version_id))
}

pub async fn prepare_overwrite(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
    current_path: &str,
) -> anyhow::Result<()> {
    if bucket_versioning(operator, config, namespace, bucket).await? == BucketVersioning::Off
        || !operator.exists(current_path).await?
    {
        return Ok(());
    }
    let mut manifest = object_manifest(operator, config, namespace, bucket, object).await?;
    if !manifest.versions.is_empty() {
        return Ok(());
    }
    let version_id = "null";
    let data_path = version_data_path(config, namespace, bucket, object, version_id);
    copy(operator, current_path, &data_path).await?;
    manifest.versions.push(ObjectVersion {
        version_id: version_id.to_string(),
        is_delete_marker: false,
        created_at: now(),
        data_path: Some(data_path),
    });
    write_manifest(operator, config, namespace, bucket, object, &manifest).await
}

pub async fn record_delete_marker(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
) -> anyhow::Result<Option<String>> {
    let status = bucket_versioning(operator, config, namespace, bucket).await?;
    if status == BucketVersioning::Off {
        return Ok(None);
    }
    let version_id = match status {
        BucketVersioning::Enabled => new_version_id(),
        BucketVersioning::Suspended => "null".to_string(),
        BucketVersioning::Off => unreachable!(),
    };
    let mut manifest = object_manifest(operator, config, namespace, bucket, object).await?;
    manifest
        .versions
        .retain(|version| version.version_id != version_id);
    manifest.versions.insert(
        0,
        ObjectVersion {
            version_id: version_id.clone(),
            is_delete_marker: true,
            created_at: now(),
            data_path: None,
        },
    );
    write_manifest(operator, config, namespace, bucket, object, &manifest).await?;
    Ok(Some(version_id))
}

pub async fn version(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
    version_id: Option<&str>,
) -> anyhow::Result<Option<ObjectVersion>> {
    let manifest = object_manifest(operator, config, namespace, bucket, object).await?;
    Ok(match version_id {
        Some(version_id) => manifest
            .versions
            .into_iter()
            .find(|version| version.version_id == version_id),
        None => manifest.versions.into_iter().next(),
    })
}

pub async fn list_versions(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
) -> anyhow::Result<Vec<(String, ObjectVersion)>> {
    let prefix = version_manifest_prefix(config, namespace, bucket);
    if !operator.exists(&prefix).await? {
        return Ok(Vec::new());
    }
    let mut lister = operator.lister_with(&prefix).recursive(true).await?;
    let mut versions = Vec::new();
    use tokio_stream::StreamExt;
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if !entry.metadata().is_file() || !entry.path().ends_with(".json") {
            continue;
        }
        let manifest: VersionManifest =
            serde_json::from_slice(&operator.read(entry.path()).await?.to_vec())?;
        let object = decode_object_name(entry.name().trim_end_matches(".json"))?;
        versions.extend(
            manifest
                .versions
                .into_iter()
                .map(|version| (object.clone(), version)),
        );
    }
    versions.sort_by(|left, right| right.1.created_at.cmp(&left.1.created_at));
    Ok(versions)
}

fn internal_path(config: &Config, namespace: &str, bucket: &str, suffix: &str) -> String {
    crate::storage::internal_path(config, namespace, bucket, suffix)
        .expect("validated bucket layout")
}

fn version_manifest_prefix(config: &Config, namespace: &str, bucket: &str) -> String {
    internal_path(
        config,
        namespace,
        bucket,
        &format!("versions/{}/", hex::encode(bucket)),
    )
}

fn object_manifest_path(config: &Config, namespace: &str, bucket: &str, object: &str) -> String {
    format!(
        "{}{}.json",
        version_manifest_prefix(config, namespace, bucket),
        hex::encode(object)
    )
}

fn version_data_path(
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
    version_id: &str,
) -> String {
    internal_path(
        config,
        namespace,
        bucket,
        &format!(
            "version-data/{}/{}/{version_id}",
            hex::encode(bucket),
            hex::encode(object),
        ),
    )
}

async fn object_manifest(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
) -> anyhow::Result<VersionManifest> {
    let path = object_manifest_path(config, namespace, bucket, object);
    if !operator.exists(&path).await? {
        return Ok(VersionManifest::default());
    }
    Ok(serde_json::from_slice(
        &operator.read(&path).await?.to_vec(),
    )?)
}

async fn write_manifest(
    operator: &Operator,
    config: &Config,
    namespace: &str,
    bucket: &str,
    object: &str,
    manifest: &VersionManifest,
) -> anyhow::Result<()> {
    operator
        .write(
            &object_manifest_path(config, namespace, bucket, object),
            serde_json::to_vec(manifest)?,
        )
        .await?;
    Ok(())
}

async fn copy(operator: &Operator, from: &str, to: &str) -> anyhow::Result<()> {
    match operator.copy(from, to).await {
        Ok(_) => Ok(()),
        Err(_) => {
            let bytes = operator.read(from).await?;
            operator.write(to, bytes).await?;
            Ok(())
        }
    }
}

fn decode_object_name(value: &str) -> anyhow::Result<String> {
    Ok(String::from_utf8(hex::decode(value)?)?)
}

fn new_version_id() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<[u8; 24]>())
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible")
}

#[cfg(test)]
mod tests {
    use super::{
        bucket_versioning, prepare_overwrite, record_delete_marker, record_put,
        set_bucket_versioning, version, BucketVersioning,
    };
    use opendal::services::Memory;
    use opendal::Operator;

    fn config() -> crate::Config {
        crate::Config {
            server_host: String::new(),
            external_server_host: String::new(),
            max_request_body_bytes: 0,
            metadata_backend: crate::metadata::MetaDataBackend::Sqlite,
            redis: None,
            sqlite: None,
            postgres: None,
            admin: None,
            #[cfg(feature = "management")]
            management: None,
            quotas: Default::default(),
            opendal_provider: String::new(),
            opendal: Default::default(),
            storage_layout: crate::StorageLayout::Namespaced,
            single_bucket: None,
        }
    }

    #[tokio::test]
    async fn persists_object_versions_and_delete_markers() {
        let operator = Operator::new(Memory::default()).unwrap();
        let config = config();
        operator.create_dir("principal/bucket/").await.unwrap();
        set_bucket_versioning(
            &operator,
            &config,
            "principal",
            "bucket",
            BucketVersioning::Enabled,
        )
        .await
        .unwrap();
        assert_eq!(
            bucket_versioning(&operator, &config, "principal", "bucket")
                .await
                .unwrap(),
            BucketVersioning::Enabled
        );
        operator
            .write("principal/bucket/key", b"first".to_vec())
            .await
            .unwrap();
        let first = record_put(
            &operator,
            &config,
            "principal",
            "bucket",
            "key",
            "principal/bucket/key",
        )
        .await
        .unwrap()
        .unwrap();
        operator
            .write("principal/bucket/key", b"second".to_vec())
            .await
            .unwrap();
        let second = record_put(
            &operator,
            &config,
            "principal",
            "bucket",
            "key",
            "principal/bucket/key",
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(first, second);
        let first_version = version(
            &operator,
            &config,
            "principal",
            "bucket",
            "key",
            Some(&first),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            operator
                .read(&first_version.data_path.unwrap())
                .await
                .unwrap()
                .to_vec(),
            b"first"
        );
        let marker = record_delete_marker(&operator, &config, "principal", "bucket", "key")
            .await
            .unwrap()
            .unwrap();
        let marker = version(
            &operator,
            &config,
            "principal",
            "bucket",
            "key",
            Some(&marker),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(marker.is_delete_marker);
    }

    #[tokio::test]
    async fn preserves_pre_versioning_object_as_null_version() {
        let operator = Operator::new(Memory::default()).unwrap();
        let config = config();
        operator.create_dir("principal/bucket/").await.unwrap();
        let current = "principal/bucket/key";
        operator.write(current, b"before".to_vec()).await.unwrap();
        set_bucket_versioning(
            &operator,
            &config,
            "principal",
            "bucket",
            BucketVersioning::Enabled,
        )
        .await
        .unwrap();
        prepare_overwrite(&operator, &config, "principal", "bucket", "key", current)
            .await
            .unwrap();
        operator.write(current, b"after".to_vec()).await.unwrap();
        record_put(&operator, &config, "principal", "bucket", "key", current)
            .await
            .unwrap();
        let null_version = version(
            &operator,
            &config,
            "principal",
            "bucket",
            "key",
            Some("null"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            operator
                .read(&null_version.data_path.unwrap())
                .await
                .unwrap()
                .to_vec(),
            b"before"
        );
    }
}
