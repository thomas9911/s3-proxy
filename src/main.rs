use crate::axum_ext::RouterExt;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use axum_route_error::RouteError;
use opendal::{Operator, Scheme};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::Level;

mod api;
mod axum_ext;
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
    #[serde(deserialize_with = "scheme_opendal")]
    pub opendal_provider: opendal::Scheme,
    pub opendal: HashMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct SqliteConfig {
    pub url: String,
}

fn scheme_opendal<'de, D>(deserializer: D) -> Result<opendal::Scheme, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    String::deserialize(deserializer).and_then(|string| {
        let scheme =
            opendal::Scheme::from_str(&string).map_err(|err| Error::custom(err.to_string()))?;

        if !opendal::Scheme::enabled().contains(&scheme) {
            return Err(Error::custom(format!("{} support is not enabled", scheme)));
        }

        Ok(scheme)
    })
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
        let operator = Operator::via_map(config.opendal_provider, config.opendal.clone())?;

        Ok(AppState {
            metadata_store,
            config: Arc::new(config),
            opendal_operator: operator,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args();

    if args.find(|arg| arg == "--backends").is_some() {
        let mut schemes: Vec<_> = opendal::Scheme::enabled().into_iter().collect();
        schemes.sort_by_key(|scheme| scheme.into_static());

        for scheme in schemes {
            if scheme == Scheme::Ghac {
                continue;
            }
            let map = HashMap::from([
                ("root".to_string(), "/tmp".to_string()),
                ("container".to_string(), "tmp".to_string()),
                ("filesystem".to_string(), "tmp".to_string()),
                ("bucket".to_string(), "tmp".to_string()),
                ("region".to_string(), "eu-west1".to_string()),
                ("endpoint".to_string(), "127.0.0.1".to_string()),
                ("account_name".to_string(), "abc".to_string()),
                ("access_key_id".to_string(), "abc".to_string()),
                ("secret_access_key".to_string(), "abc".to_string()),
            ]);

            let cap =
                Operator::via_map(scheme, map).map(|operator| operator.info().full_capability())?;
            if cap.list && cap.write && cap.read && cap.create_dir {
                println!("{} => {:?}", scheme, cap)
            }
        }
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
            "/:bucket_name",
            get(api::list_objects)
                .put(api::create_bucket)
                .delete(api::delete_bucket)
                .post(api::post_object),
        )
        .route(
            "/:bucket_name/*object_name",
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
