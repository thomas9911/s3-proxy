use crate::axum_ext::RouterExt;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use axum_route_error::RouteError;
use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;
use opendal::{Operator, Scheme};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::Level;

mod api;
mod axum_ext;
mod signature;
mod templates;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub server_host: String,
    #[serde(default = "default_external_host")]
    pub external_server_host: String,
    pub redis: Option<deadpool_redis::Config>,
    #[serde(deserialize_with = "scheme_opendal")]
    pub opendal_provider: opendal::Scheme,
    pub opendal: HashMap<String, String>,
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
    /// metadata_pool is already an Arc
    pub metadata_pool: Pool,
    pub config: Arc<Config>,
    /// opendal_operator is already an Arc
    pub opendal_operator: Operator,
}

impl AppState {
    pub fn from_config(config: Config) -> anyhow::Result<AppState> {
        let mut maybe_pool = None;

        if let Some(redis_config) = &config.redis {
            maybe_pool = Some(redis_config.create_pool(Some(deadpool_redis::Runtime::Tokio1))?);
        }

        anyhow::ensure!(maybe_pool.is_some(), "Unable to create metadata pool");

        let operator = Operator::via_map(config.opendal_provider.clone(), config.opendal.clone())?;

        Ok(AppState {
            metadata_pool: maybe_pool.expect("pool checked is not none earlier"),
            config: Arc::new(config),
            opendal_operator: operator,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args();

    if args.find(|x| x == "--backends").is_some() {
        let mut schemes: Vec<_> = opendal::Scheme::enabled().into_iter().collect();
        schemes.sort_by_key(|x| x.into_static());

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
    
            let cap = Operator::via_map(scheme, map).map(|x| x.info().full_capability())?;
            if cap.list && cap.write && cap.read && cap.create_dir {
                println!("{} => {:?}", scheme, cap)
            }
        }
        return Ok(())
    }

    let config = Config::from_env()?;
    tracing_subscriber::fmt()
        .with_max_level(Level::ERROR)
        .init();

    let server_host = config.server_host.clone();
    let app_state = AppState::from_config(config)?;

    // build our application with a single route
    let app = Router::new()
        .route("/_metadata", get(asdfg))
        .route("/", get(api::list_buckets))
        .directory_route(
            "/:bucket_name",
            get(api::list_objects).put(api::create_bucket),
        )
        .route(
            "/:bucket_name/:object_name",
            get(api::get_object).put(api::create_object),
        )
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(server_host).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn asdfg(
    State(AppState { metadata_pool, .. }): State<AppState>,
) -> Result<impl IntoResponse, RouteError> {
    let mut conn = metadata_pool.get().await?;
    let _: () = conn
        .set(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs(),
            1,
        )
        .await?;

    let res: Vec<String> = conn.keys("17068*").await?;

    Ok(Json(res))
}
