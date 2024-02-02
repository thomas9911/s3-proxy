use axum::response::Json;
use axum::{extract::State, response::IntoResponse, routing::get, Router};
use axum_route_error::RouteError;

use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;

use askama::Template;
use std::time::SystemTime;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

struct ListBucketItem<'a> {
    name: &'a str,
    timestamp: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "list_buckets.xml")]
pub struct ListBucketsTemplate<'a> {
    owner_name: &'a str,
    owner_id: &'a str,
    buckets: Vec<ListBucketItem<'a>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub server_host: String,
    pub redis: Option<deadpool_redis::Config>,
}

fn default_host() -> String {
    String::from("0.0.0.0:3000")
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
}

impl AppState {
    pub fn from_config(config: &Config) -> anyhow::Result<AppState> {
        let mut maybe_pool = None;

        if let Some(redis_config) = &config.redis {
            maybe_pool = Some(redis_config.create_pool(Some(deadpool_redis::Runtime::Tokio1))?);
        }

        anyhow::ensure!(maybe_pool.is_some(), "Unable to create metadata pool");

        Ok(AppState {
            metadata_pool: maybe_pool.expect("pool checked is not none earlier"),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    dbg!(&config);
    let app_state = AppState::from_config(&config)?;

    // build our application with a single route
    let app = Router::new()
        .route("/_metadata", get(asdfg))
        .route("/", get(list_buckets))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(config.server_host)
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}

async fn asdfg(
    State(AppState { metadata_pool }): State<AppState>,
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
    State(AppState { metadata_pool }): State<AppState>,
) -> Result<impl IntoResponse, RouteError> {
    let datetime = OffsetDateTime::from_unix_timestamp(1706911595)?;
    let tmp_timestamp = datetime.format(&Rfc3339).unwrap();

    let template = ListBucketsTemplate {
        owner_name: "Testing",
        owner_id: "1",
        buckets: vec![
            ListBucketItem {
                name: "testing1",
                timestamp: Some(&tmp_timestamp),
            },
            ListBucketItem {
                name: "testing2",
                timestamp: None,
            },
        ],
    };

    Ok(askama_axum::into_response(&template))
}
