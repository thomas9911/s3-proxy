use crate::axum_ext::RouterExt;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use axum_route_error::RouteError;
use opendal::Operator;
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::Level;

mod api;
mod axum_ext;
mod backends;
mod metadata;
mod signature;
mod templates;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub server_host: String,
    #[serde(default = "default_external_host")]
    pub external_server_host: String,
    #[serde(default = "default_metadata_backend")]
    pub metadata_backend: String,
    pub redis: Option<deadpool_redis::Config>,
    pub sqlite: Option<SqliteConfig>,
    pub opendal_provider: String,
    pub opendal: HashMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct SqliteConfig {
    pub url: String,
}

fn default_host() -> String {
    String::from("0.0.0.0:3000")
}

fn default_external_host() -> String {
    String::from("http://0.0.0.0:3000")
}

fn default_metadata_backend() -> String {
    String::from("redis")
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
        let metadata_store: Arc<dyn metadata::MetadataStore> =
            match config.metadata_backend.as_str() {
                "redis" => {
                    let redis_config = config.redis.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Redis metadata configuration is missing")
                    })?;
                    let pool = redis_config.create_pool(Some(deadpool_redis::Runtime::Tokio1))?;
                    Arc::new(metadata::RedisMetadataStore::new(pool))
                }
                "sqlite" => {
                    let sqlite_config = config.sqlite.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("SQLite metadata configuration is missing")
                    })?;
                    Arc::new(metadata::SqliteMetadataStore::connect(&sqlite_config.url).await?)
                }
                backend => anyhow::bail!("Unsupported metadata backend: {backend}"),
            };
        let operator = Operator::via_iter(&config.opendal_provider, config.opendal.clone())?;

        Ok(AppState {
            metadata_store,
            config: Arc::new(config),
            opendal_operator: operator,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Ensure all services enabled through Cargo features are available in the
    // global registry, including in binaries where constructor registration is
    // not run by the linker.
    opendal::init_default_registry();

    if std::env::args().any(|arg| arg == "--backends") {
        backends::probe();
        return Ok(());
    }

    let config = Config::from_env()?;
    tracing_subscriber::fmt()
        .with_max_level(Level::ERROR)
        .init();

    let server_host = config.server_host.clone();
    let app_state = AppState::from_config(config).await?;

    let app = Router::new()
        .route("/_metadata", get(metadata_debug))
        .route("/", get(api::list_buckets))
        .directory_route(
            "/{bucket_name}",
            get(api::list_objects)
                .put(api::create_bucket)
                .delete(api::delete_bucket)
                .post(api::post_object),
        )
        .route(
            "/{bucket_name}/{*object_name}",
            get(api::get_object)
                .head(api::head_object)
                .put(api::create_object)
                .delete(api::delete_object),
        )
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(server_host).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn metadata_debug(
    State(AppState { metadata_store, .. }): State<AppState>,
) -> Result<impl IntoResponse, RouteError> {
    let res = metadata_store.debug_keys("17068*").await?;

    Ok(Json(res))
}
