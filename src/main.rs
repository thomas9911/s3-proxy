use axum::extract::{Path, State};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, put};
use axum::Router;
use axum_route_error::RouteError;
use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;
use opendal::Operator;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio_stream::StreamExt;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::Level;

use crate::signature::VerifiedRequest;

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
    let config = Config::from_env()?;
    tracing_subscriber::fmt()
        .with_max_level(Level::ERROR)
        .init();

    let server_host = config.server_host.clone();
    let app_state = AppState::from_config(config)?;

    // build our application with a single route
    let app = Router::new()
        .route("/_metadata", get(asdfg))
        .route("/", get(list_buckets))
        .route("/:bucket_name/", put(create_bucket))
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

async fn list_buckets(
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = &signature.namespace;

    // let bucket = "testing";

    // opendal_operator
    //     .write(
    //         &format!("{}/{}/testing.bin", namespace, bucket),
    //         vec![0; 4096],
    //     )
    //     .await?;

    let mut lister = opendal_operator
        .lister_with(&format!("{}/", namespace))
        .await?;

    let mut buckets = Vec::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(x) => {
                if x.metadata().is_dir() {
                    buckets.push(templates::ListBucketItem {
                        name: x.name().trim_end_matches('/').to_string().into(),
                        timestamp: None,
                    })
                }
            }
            Err(e) => {
                tracing::error!("{}", e.to_string());
                return Err(RouteError::new_internal_server());
            }
        }
    }

    // let datetime = OffsetDateTime::from_unix_timestamp(1706911595)?;
    // let tmp_timestamp = datetime.format(&Rfc3339).unwrap();

    let template = templates::ListBucketsTemplate {
        owner_name: "Testing",
        owner_id: "1",
        // buckets: vec![
        //     templates::ListBucketItem {
        //         name: "testing1".into(),
        //         timestamp: Some(tmp_timestamp.into()),
        //     },
        //     templates::ListBucketItem {
        //         name: "testing2".into(),
        //         timestamp: None,
        //     },
        // ],
        buckets,
    };

    Ok(askama_axum::into_response(&template))
}

async fn create_bucket(
    Path(bucket_name): Path<String>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = &signature.namespace;

    let utf8_slice = std::str::from_utf8(&signature.bytes)?;

    let _body: Option<templates::CreateBucket> = quick_xml::de::from_str(utf8_slice)?;

    opendal_operator
        .create_dir(&format!("{}/", namespace))
        .await?;
    opendal_operator
        .create_dir(&format!("{}/{}/", namespace, bucket_name))
        .await?;

    Ok("OK".into_response())
}
