use crate::axum_ext::RouterExt;
use crate::signature::s3_error_response;
use axum::extract::{DefaultBodyLimit, State};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use axum_route_error::RouteError;
use opendal::Operator;
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

mod api;
mod axum_ext;
pub mod backends;
pub mod metadata;
mod policy;
mod retry;
mod signature;
pub mod templates;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub server_host: String,
    #[serde(default = "default_external_host")]
    pub external_server_host: String,
    #[serde(default = "default_metadata_backend")]
    pub metadata_backend: metadata::MetaDataBackend,
    pub redis: Option<deadpool_redis::Config>,
    #[serde(default = "default_sqlite")]
    pub sqlite: Option<SqliteConfig>,
    pub postgres: Option<PostgresConfig>,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[serde(default = "default_opendal_provider")]
    pub opendal_provider: String,
    #[serde(default)]
    pub opendal: HashMap<String, String>,
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

fn default_host() -> String {
    String::from("0.0.0.0:3000")
}

fn default_external_host() -> String {
    String::from("http://0.0.0.0:3000")
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
        opendal::init_default_registry();
        let metadata_store: Arc<dyn metadata::MetadataStore> =
            match config.metadata_backend {
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
                metadata::MetaDataBackend::Postgres => {
                    let postgres_config = config.postgres.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("PostgreSQL metadata configuration is missing")
                    })?;
                    Arc::new(metadata::PostgresMetadataStore::connect(&postgres_config.url).await?)
                }
            };
        if let Some(admin) = config.admin.as_ref() {
            if metadata_store
                .secret_key(&admin.access_key)
                .await?
                .is_none()
            {
                metadata_store
                    .set_secret_key(&admin.access_key, &admin.secret_key)
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
    Router::new()
        .route("/_metadata", get(metadata_debug))
        .route("/", get(api::list_buckets))
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
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
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

#[cfg(test)]
mod tests {
    use super::{metadata::MetaDataBackend, Config};

    #[test]
    fn config_defaults_to_in_memory_services() {
        let config: Config = serde_json::from_str("{}").unwrap();

        assert!(matches!(config.metadata_backend, MetaDataBackend::Sqlite));
        assert_eq!(config.sqlite.unwrap().url, "sqlite::memory:");
        assert_eq!(config.opendal_provider, "memory");
        assert!(config.opendal.is_empty());
    }
}

async fn metadata_debug(
    State(AppState { metadata_store, .. }): State<AppState>,
) -> Result<impl IntoResponse, RouteError> {
    let res = metadata_store.debug_keys("17068*").await?;

    Ok(Json(res))
}
