use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, PercentEncodingMode, SessionTokenMode, SignableBody, SignableRequest,
    SignatureLocation, SigningSettings, UriPathNormalizationMode,
};
use aws_sigv4::sign::v4::SigningParams;
use aws_smithy_runtime_api::client::identity::Identity;
use axum::{
    extract::{Host, OriginalUri, Request, State},
    http::request,
    response::IntoResponse,
    routing::get,
    Router,
};
use axum::{http::HeaderMap, response::Json};
use axum_extra::TypedHeader;
use axum_route_error::RouteError;
use headers::{authorization, Authorization};

use deadpool_redis::redis::AsyncCommands;
use deadpool_redis::Pool;

use askama::Template;
use std::{convert::Infallible, time::SystemTime};

use time::OffsetDateTime;
use time::{error::Parse, format_description::well_known::Rfc3339};

use tracing::Level;

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
    tracing_subscriber::fmt()
        .with_max_level(Level::ERROR)
        .init();

    // dbg!(&config);
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
    header_map: HeaderMap,
    Host(host): Host,
    OriginalUri(original_uri): OriginalUri,
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

    verify_headers(
        &header_map,
        &format!("http://{host}{original_uri}"),
        "notrealrnrELgWzOk3IfjzDKtFBhDby",
    );

    Ok(askama_axum::into_response(&template))
}

#[derive(Debug, Default)]
struct S4Params<'a> {
    access_key: &'a str,
    date: &'a str,
    region: &'a str,
    service: &'a str,
    postfix: &'a str,
    signed_headers: Vec<&'a str>,
    signature: &'a str,
}

use time::format_description;
use time::{Date, PrimitiveDateTime, Time};

const DATE_TIME_FORMAT: &str = "[year][month][day]T[hour][minute][second]Z";
const DATE_FORMAT: &str = "[year][month][day]";

/// Parses `YYYYMMDD'T'HHMMSS'Z'` formatted dates into a `SystemTime`.
pub(crate) fn parse_date_time(date_time_str: &str) -> Result<SystemTime, Parse> {
    let date_time = PrimitiveDateTime::parse(
        date_time_str,
        &format_description::parse(DATE_TIME_FORMAT).unwrap(),
    )?
    .assume_utc();
    Ok(date_time.into())
}

/// Parses `YYYYMMDD` formatted dates into a `SystemTime`.
pub(crate) fn parse_date(date_str: &str) -> Result<SystemTime, Parse> {
    let date_time = PrimitiveDateTime::new(
        Date::parse(date_str, &format_description::parse(DATE_FORMAT).unwrap())?,
        Time::from_hms(0, 0, 0).unwrap(),
    )
    .assume_utc();
    Ok(date_time.into())
}

fn verify_headers(header_map: &HeaderMap, full_host: &str, secret_key: &str) -> bool {
    let params = parse_authorization_header(&header_map);

    if params.is_none() {
        return false;
    }

    let params = params.unwrap();

    // the same as aws list bucket request via tracing
    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    settings.signature_location = SignatureLocation::Headers;
    settings.expires_in = None;
    settings.excluded_headers = Some(vec![
        "authorization".into(),
        "user-agent".into(),
        "x-amzn-trace-id".into(),
    ]);
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    settings.session_token_mode = SessionTokenMode::Include;

    let identity = Credentials::new(params.access_key, secret_key, None, None, "test").into();

    let datetime = header_map
        .get("x-amz-date")
        .and_then(|x| x.to_str().ok())
        .and_then(|x| parse_date_time(x).ok());
    if datetime.is_none() {
        return false;
    }
    let datetime = datetime.unwrap();

    let builder = SigningParams::builder()
        .identity(&identity)
        .region(params.region)
        .name(params.service)
        .time(datetime)
        .settings(settings);

    let signer = builder.build().unwrap();

    let request = SignableRequest::new(
        "GET",
        full_host,
        header_map
            .iter()
            .filter(|(key, _)| params.signed_headers.contains(&key.as_str()))
            // .filter(|(key, _)| !["x-amz-content-sha256", "x-amz-date"].contains(&key.as_str()))
            .map(|(key, value)| (key.as_str(), value.to_str().unwrap()))
            .inspect(|x| println!("{:?}", x)),
        SignableBody::Bytes(&[]),
    )
    .expect("host is not valid");

    // dbg!(&signer);
    // dbg!(&params.signature);

    if let Ok(output) = aws_sigv4::http_request::sign(request, &signer.into()) {
        dbg!(&params.signature);
        dbg!(output.signature());

        return dbg!(output.signature() == params.signature);
    }

    false
}

fn parse_authorization_header(header_map: &HeaderMap) -> Option<S4Params> {
    let mut params = S4Params::default();
    let authorization = header_map
        .get("authorization")
        .and_then(|x| x.to_str().ok())?;
    let (_, rest) = authorization.split_once(" ")?;

    for item in rest.split(",") {
        let item = item.trim();

        match item.split_once("=") {
            Some(("Credential", credential_string)) => {
                let mut asdf = credential_string.split('/');
                params.access_key = asdf.next()?;
                params.date = asdf.next()?;
                params.region = asdf.next()?;
                params.service = asdf.next()?;
                params.postfix = asdf.next()?;
            }
            Some(("SignedHeaders", headers)) => {
                params.signed_headers = headers.split(';').collect();
            }
            Some(("Signature", signature)) => {
                params.signature = signature;
            }
            _ => {}
        }
    }

    Some(params)
}

#[test]
fn verify_headers_test() {
    let secret_key = "notrealrnrELgWzOk3IfjzDKtFBhDby";
    let mut header_map = HeaderMap::new();
    header_map.insert(
        "user-agent",
        "aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"
            .parse()
            .unwrap(),
    );
    header_map.insert(
        "x-amz-user-agent",
        "aws-sdk-rust/1.1.4 api/s3/1.14.0 os/windows lang/rust/1.71.1"
            .parse()
            .unwrap(),
    );
    header_map.insert("x-amz-date", "20240203T125727Z".parse().unwrap());
    header_map.insert("authorization", "AWS4-HMAC-SHA256 Credential=ANOTREAL/20240203/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1".parse().unwrap());
    header_map.insert(
        "x-amz-content-sha256",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            .parse()
            .unwrap(),
    );
    header_map.insert("amz-sdk-request", "attempt=1; max=3".parse().unwrap());
    header_map.insert(
        "amz-sdk-invocation-id",
        "45ae4a4d-e614-4cfa-854e-108d396d444b".parse().unwrap(),
    );
    header_map.insert("host", "127.0.0.1:3000".parse().unwrap());

    assert!(verify_headers(
        &header_map,
        "http://127.0.0.1:3000/?x-id=ListBuckets",
        secret_key
    ))
}
