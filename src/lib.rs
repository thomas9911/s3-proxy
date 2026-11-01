use crate::axum_ext::RouterExt;
use crate::signature::s3_error_response;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use axum_route_error::RouteError;
use opendal::Operator;
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 256 * 1024 * 1024;

mod api;
mod audit;
mod axum_ext;
pub mod backends;
pub mod metadata;
mod metrics;
mod policy;
pub mod quota;
mod retry;
mod signature;
pub mod storage;
pub mod sync;
pub mod templates;
mod versioning;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub server_host: String,
    #[serde(default = "default_external_host")]
    pub external_server_host: String,
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_metadata_backend")]
    pub metadata_backend: metadata::MetaDataBackend,
    #[cfg(feature = "redis")]
    pub redis: Option<deadpool_redis::Config>,
    #[serde(default = "default_sqlite")]
    pub sqlite: Option<SqliteConfig>,
    pub postgres: Option<PostgresConfig>,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[cfg(feature = "management")]
    #[serde(default)]
    pub management: Option<ManagementConfig>,
    #[serde(default)]
    pub quotas: quota::QuotaConfig,
    #[serde(default = "default_opendal_provider")]
    pub opendal_provider: String,
    #[serde(default)]
    pub opendal: HashMap<String, String>,
    #[serde(default)]
    pub storage_layout: StorageLayout,
    pub single_bucket: Option<SingleBucketConfig>,
}

#[derive(Debug, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageLayout {
    #[default]
    Namespaced,
    SingleBucket,
}

#[derive(Debug, serde::Deserialize)]
pub struct SingleBucketConfig {
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct SqliteConfig {
    pub url: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct PostgresConfig {
    pub url: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct AdminConfig {
    pub access_key: String,
    pub secret_key: String,
}

#[cfg(feature = "management")]
#[derive(Debug, serde::Deserialize)]
pub struct ManagementConfig {
    pub username: String,
    pub password: String,
}

fn default_host() -> String {
    String::from("0.0.0.0:3000")
}

fn default_external_host() -> String {
    String::from("http://0.0.0.0:3000")
}

fn default_max_request_body_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BODY_BYTES
}

fn default_metadata_backend() -> metadata::MetaDataBackend {
    metadata::MetaDataBackend::Sqlite
}

fn default_opendal_provider() -> String {
    String::from("memory")
}

fn default_sqlite() -> Option<SqliteConfig> {
    Some(SqliteConfig {
        url: String::from("sqlite::memory:"),
    })
}

impl Config {
    pub fn from_env() -> Result<Self, config::ConfigError> {
        let cfg = config::Config::builder()
            .add_source(config::Environment::with_prefix("S3_PROXY").separator("__"))
            .build()?;

        cfg.try_deserialize()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub metadata_store: Arc<dyn metadata::MetadataStore>,
    pub config: Arc<Config>,
    /// opendal_operator is already an Arc
    pub opendal_operator: Operator,
}

impl AppState {
    pub async fn from_config(config: Config) -> anyhow::Result<AppState> {
        if matches!(config.storage_layout, StorageLayout::SingleBucket) {
            let single = config.single_bucket.as_ref().ok_or_else(|| {
                anyhow::anyhow!("single_bucket configuration is required for single_bucket layout")
            })?;
            if single.namespace.is_empty() || single.name.is_empty() {
                anyhow::bail!("single_bucket namespace and name must not be empty")
            }
        }
        opendal::init_default_registry();
        let metadata_store: Arc<dyn metadata::MetadataStore> =
            match config.metadata_backend {
                #[cfg(feature = "redis")]
                metadata::MetaDataBackend::Redis => {
                    let redis_config = config.redis.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Redis metadata configuration is missing")
                    })?;
                    let pool = redis_config.create_pool(Some(deadpool_redis::Runtime::Tokio1))?;
                    Arc::new(metadata::RedisMetadataStore::new(pool))
                }
                metadata::MetaDataBackend::Sqlite => {
                    let sqlite_config = config.sqlite.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("SQLite metadata configuration is missing")
                    })?;
                    Arc::new(metadata::SqliteMetadataStore::connect(&sqlite_config.url).await?)
                }
                #[cfg(feature = "postgres")]
                metadata::MetaDataBackend::Postgres => {
                    let postgres_config = config.postgres.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("PostgreSQL metadata configuration is missing")
                    })?;
                    Arc::new(metadata::PostgresMetadataStore::connect(&postgres_config.url).await?)
                }
            };
        if let Some(admin) = config.admin.as_ref() {
            let access_key = metadata_store.access_key(&admin.access_key).await?;
            if access_key.is_none() {
                metadata_store
                    .set_secret_key(&admin.access_key, &admin.secret_key)
                    .await?;
            }
            let principal_id = access_key
                .map(|access_key| access_key.principal_id)
                .unwrap_or_else(|| admin.access_key.clone());
            let owners = metadata_store.list_namespace_owners().await?;
            if !owners
                .iter()
                .any(|(namespace, _)| namespace == &principal_id)
            {
                metadata_store
                    .set_namespace_owner(&principal_id, &principal_id, &principal_id)
                    .await?;
            }
        }
        let operator = Operator::via_iter(&config.opendal_provider, config.opendal.clone())?
            .layer(opendal::layers::TracingLayer::new());

        Ok(AppState {
            metadata_store,
            config: Arc::new(config),
            opendal_operator: operator,
        })
    }
}

pub fn build_app(app_state: AppState) -> Router {
    let max_request_body_bytes = app_state.config.max_request_body_bytes;
    let router = Router::new()
        .route("/_metadata", get(metadata_debug))
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/metrics", get(metrics));

    #[cfg(feature = "management")]
    let router = router.merge(management_router());

    router
        .route("/", get(api::list_buckets).post(api::create_access_key))
        .directory_route(
            "/{bucket_name}",
            get(api::get_bucket)
                .put(api::put_bucket)
                .delete(api::delete_bucket_route)
                .post(api::post_bucket),
        )
        .route(
            "/{bucket_name}/{*object_name}",
            get(api::get_object)
                .head(api::head_object)
                .put(api::put_object)
                .delete(api::delete_object_route)
                .post(api::post_object_route),
        )
        // Single-request uploads are buffered while their S3 signature is verified.
        .layer(DefaultBodyLimit::max(max_request_body_bytes))
        .layer(axum::middleware::from_fn(audit_and_metrics))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .fallback(|| async {
            s3_error_response(
                axum::http::StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The requested resource was not found.",
            )
        })
        .with_state(app_state)
}

#[cfg(feature = "management")]
fn management_router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(api::management_dashboard))
        .nest("/admin/api", management_api_router())
        .nest("/admin/fragments", management_fragments_router())
}

#[cfg(feature = "management")]
fn management_api_router() -> Router<AppState> {
    Router::new()
        .route("/status", get(api::management_status))
        .route("/audit", get(api::management_audit))
        .route("/metrics", get(api::management_metrics))
        .route("/principals", get(api::management_principals))
        .route(
            "/quotas",
            get(api::management_quota).post(api::management_update_quota),
        )
        .route(
            "/access-keys",
            axum::routing::post(api::management_create_access_key)
                .patch(api::management_update_access_key)
                .delete(api::management_delete_access_key),
        )
        .route(
            "/access-keys/rotate",
            axum::routing::post(api::management_rotate_access_key),
        )
        .route(
            "/multipart-uploads",
            get(api::management_list_multipart_uploads)
                .delete(api::management_abort_multipart_upload),
        )
        .route(
            "/buckets",
            get(api::management_buckets)
                .post(api::management_create_bucket)
                .delete(api::management_delete_bucket),
        )
        .route(
            "/bucket-configuration",
            axum::routing::post(api::management_update_bucket_configuration),
        )
        .route(
            "/objects",
            get(api::management_list_objects)
                .post(api::management_upload_object)
                .delete(api::management_delete_object),
        )
        .route(
            "/presigned-urls",
            axum::routing::post(api::management_presign_object),
        )
        .route("/download", get(api::management_download_object))
        .route(
            "/presigned-posts",
            axum::routing::post(api::management_presign_post),
        )
        .route("/inspect", get(api::management_inspect))
}

#[cfg(feature = "management")]
fn management_fragments_router() -> Router<AppState> {
    Router::new()
        .route("/objects", get(api::management_list_objects_fragment))
        .route("/inspect", get(api::management_inspect_fragment))
        .route("/status", get(api::management_status_fragment))
        .route("/principals", get(api::management_principals_fragment))
        .route(
            "/buckets",
            get(api::management_buckets_fragment).post(api::management_create_bucket_fragment),
        )
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn readiness(State(state): State<AppState>) -> Response {
    let metadata_ready = state.metadata_store.debug_keys("__readyz__").await.is_ok();
    let storage_ready = state.opendal_operator.exists(".s3-proxy/").await.is_ok();
    if metadata_ready && storage_ready {
        Json(serde_json::json!({ "status": "ready" })).into_response()
    } else {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": "not_ready" })),
        )
            .into_response()
    }
}

async fn metrics() -> Response {
    Response::builder()
        .header("content-type", "text/plain; version=0.0.4")
        .body(axum::body::Body::from(crate::metrics::render()))
        .expect("static metrics response is valid")
}

async fn audit_and_metrics(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    crate::metrics::record_http(response.status().as_u16());
    crate::audit::record(
        method.to_string(),
        path.clone(),
        response.status().as_u16(),
        started.elapsed().as_micros() as u64,
    );
    tracing::info!(
        audit = true,
        method = %method,
        path,
        status = response.status().as_u16(),
        elapsed_us = started.elapsed().as_micros() as u64,
        "request completed"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::{metadata::MetaDataBackend, Config};

    #[test]
    fn config_defaults_to_in_memory_services() {
        let config: Config = serde_json::from_str("{}").unwrap();

        assert!(matches!(config.metadata_backend, MetaDataBackend::Sqlite));
        assert_eq!(config.sqlite.unwrap().url, "sqlite::memory:");
        assert_eq!(config.max_request_body_bytes, 256 * 1024 * 1024);
        assert_eq!(config.opendal_provider, "memory");
        assert!(config.opendal.is_empty());
    }
}

#[allow(clippy::items_after_test_module)]
async fn metadata_debug(
    State(AppState { metadata_store, .. }): State<AppState>,
) -> Result<impl IntoResponse, RouteError> {
    let res = metadata_store.debug_keys("17068*").await?;

    Ok(Json(res))
}
