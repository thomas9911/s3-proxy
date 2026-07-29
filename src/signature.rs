use crate::templates;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, PercentEncodingMode, SessionTokenMode, SignableBody, SignableRequest,
    SignatureLocation, SigningSettings, UriPathNormalizationMode,
};
use aws_sigv4::sign::v4::SigningParams;
use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, FromRequestParts, OriginalUri, Request};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use std::convert::Infallible;
use std::time::{Duration, SystemTime};
use time::error::Parse;
use tracing::error;

#[derive(Debug, Default, PartialEq)]
pub struct S3V4Params<'a> {
    pub access_key: &'a str,
    pub date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub postfix: &'a str,
    pub signed_headers: Vec<&'a str>,
    pub signature: &'a str,
}

use time::{format_description, macros, PrimitiveDateTime};

use crate::AppState;

const DATE_TIME_FORMAT: format_description::StaticFormatDescription =
    macros::format_description!("[year][month][day]T[hour][minute][second]Z");

#[derive(Debug, Default, PartialEq)]
pub struct VerifiedRequest {
    pub access_key: String,
    pub namespace: String,
    pub bytes: Bytes,
}

#[derive(Debug, Default)]
struct PresignedV4Params {
    access_key: String,
    date: String,
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
    expires: u64,
}

pub(crate) fn s3_error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    templates::xml_response(status, templates::ErrorTemplate { code, message })
}

pub enum VerifiedRequestError {
    FormattedResponse(Response<Body>),
    Metadata(anyhow::Error),
}

impl IntoResponse for VerifiedRequestError {
    fn into_response(self) -> Response<Body> {
        match self {
            VerifiedRequestError::FormattedResponse(response) => response,
            VerifiedRequestError::Metadata(error) => {
                error!("{}", error.to_string());

                let mut response = Response::default();
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                response
            }
        }
    }
}

impl From<Response<Body>> for VerifiedRequestError {
    fn from(value: Response<Body>) -> Self {
        VerifiedRequestError::FormattedResponse(value)
    }
}

impl From<Infallible> for VerifiedRequestError {
    fn from(_: Infallible) -> Self {
        unreachable!()
    }
}

impl From<anyhow::Error> for VerifiedRequestError {
    fn from(value: anyhow::Error) -> Self {
        VerifiedRequestError::Metadata(value)
    }
}

impl FromRequest<AppState> for VerifiedRequest {
    type Rejection = VerifiedRequestError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let metadata_store = &state.metadata_store;
        let config = &state.config;
        let (mut parts, body) = req.into_parts();
        let header_map = HeaderMap::from_request_parts(&mut parts, state).await?;
        let OriginalUri(original_uri) = OriginalUri::from_request_parts(&mut parts, state).await?;
        let http_method = &parts.method;

        let cloned_parts = parts.clone();

        let extra_requests = Request::from_parts(cloned_parts, body);
        let bytes = Bytes::from_request(extra_requests, &state)
            .await
            .map_err(|e| e.into_response())?;
        let external_host = &config.external_server_host;

        if let Some(params) = parse_presigned_query(&original_uri) {
            let access_key = match metadata_store.access_key(&params.access_key).await? {
                Some(access_key) if access_key.status.as_str() == "Active" => access_key,
                None => {
                    return Err(s3_error_response(
                        StatusCode::FORBIDDEN,
                        "AccessDenied",
                        "Access Denied",
                    )
                    .into())
                }
                Some(_) => {
                    return Err(s3_error_response(
                        StatusCode::FORBIDDEN,
                        "AccessDenied",
                        "Access Denied",
                    )
                    .into())
                }
            };
            if !verify_presigned_query(
                &header_map,
                &params,
                http_method,
                &format!(
                    "{external_host}{presigned_uri}",
                    presigned_uri = strip_presign_query(&original_uri)
                ),
                &access_key.secret_key,
            ) {
                return Err(s3_error_response(
                    StatusCode::FORBIDDEN,
                    "SignatureDoesNotMatch",
                    "The request signature we calculated does not match the signature you provided.",
                )
                .into());
            }
            if !crate::quota::allow_request(&config.quotas, &access_key.principal_id) {
                return Err(s3_error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "SlowDown",
                    "The request quota for this principal has been exceeded.",
                )
                .into());
            }
            tracing::info!(
                audit = true,
                event = "authenticated_request",
                access_key = %params.access_key,
                principal = %access_key.principal_id,
            );
            metadata_store
                .record_access_key_use(&params.access_key)
                .await?;
            return Ok(VerifiedRequest {
                access_key: params.access_key.clone(),
                namespace: access_key.principal_id,
                bytes,
            });
        }

        let params = match parse_authorization_header(&header_map) {
            Some(params) => params,
            None => {
                return Err(s3_error_response(
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "Access Denied",
                )
                .into());
            }
        };

        let access_key = match metadata_store.access_key(params.access_key).await? {
            Some(access_key) if access_key.status.as_str() == "Active" => access_key,
            None => {
                return Err(s3_error_response(
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "Access Denied",
                )
                .into())
            }
            Some(_) => {
                return Err(s3_error_response(
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "Access Denied",
                )
                .into())
            }
        };

        if !verify_headers(
            &header_map,
            &params,
            http_method,
            &format!("{external_host}{original_uri}"),
            &access_key.secret_key,
            &bytes,
        ) {
            return Err(s3_error_response(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            )
            .into());
        };

        if !crate::quota::allow_request(&config.quotas, &access_key.principal_id) {
            return Err(s3_error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "SlowDown",
                "The request quota for this principal has been exceeded.",
            )
            .into());
        }

        tracing::info!(
            audit = true,
            event = "authenticated_request",
            access_key = %params.access_key,
            principal = %access_key.principal_id,
        );

        metadata_store
            .record_access_key_use(params.access_key)
            .await?;
        Ok(VerifiedRequest {
            access_key: params.access_key.to_string(),
            namespace: access_key.principal_id,
            bytes,
        })
    }
}

pub(crate) fn has_presigned_query(uri: &axum::http::Uri) -> bool {
    parse_presigned_query(uri).is_some()
}

#[cfg(feature = "management")]
pub(crate) fn presign_get_url(
    external_host: &str,
    bucket: &str,
    key: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    expires_in: u64,
) -> anyhow::Result<String> {
    if expires_in == 0 || expires_in > 604_800 {
        anyhow::bail!("presigned URL expiry must be between 1 and 604800 seconds")
    }

    let encoded_key = key
        .split('/')
        .map(urlencoding::encode)
        .collect::<Vec<_>>()
        .join("/");
    let url = format!(
        "{}/{}/{}",
        external_host.trim_end_matches('/'),
        urlencoding::encode(bucket),
        encoded_key
    );
    let uri = url.parse::<axum::http::Uri>()?;
    let host = uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("external server host must include an authority"))?
        .as_str()
        .to_string();
    let identity = Credentials::new(access_key, secret_key, None, None, "management").into();
    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(Duration::from_secs(expires_in));
    settings.session_token_mode = SessionTokenMode::Include;
    settings.excluded_headers = Some(vec![
        "authorization".into(),
        "user-agent".into(),
        "x-amzn-trace-id".into(),
    ]);
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    let signer = SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("s3")
        .time(SystemTime::now())
        .settings(settings)
        .build()?;
    let request = SignableRequest::new(
        "GET",
        &url,
        std::iter::once(("host", host.as_str())),
        SignableBody::UnsignedPayload,
    )?;
    let (instructions, _) = aws_sigv4::http_request::sign(request, &signer.into())?.into_parts();
    let mut signed_request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("host", host)
        .body(())?;
    instructions.apply_to_request_http1x(&mut signed_request);
    Ok(signed_request.uri().to_string())
}

fn parse_presigned_query(uri: &axum::http::Uri) -> Option<PresignedV4Params> {
    let query = uri.query()?;
    let value = |key: &str| {
        query.split('&').find_map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            if name == key {
                urlencoding::decode(value)
                    .ok()
                    .map(|value| value.into_owned())
            } else {
                None
            }
        })
    };
    let credential = value("X-Amz-Credential")?;
    let mut credential_parts = credential.split('/');
    let access_key = credential_parts.next()?.to_string();
    let credential_date = credential_parts.next()?;
    let region = credential_parts.next()?.to_string();
    let service = credential_parts.next()?.to_string();
    if credential_parts.next()? != "aws4_request" {
        return None;
    }
    let signed_headers = value("X-Amz-SignedHeaders")?
        .split(';')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if value("X-Amz-Algorithm")?.as_str() != "AWS4-HMAC-SHA256" {
        return None;
    }
    let date = value("X-Amz-Date")?;
    if !date.starts_with(credential_date) {
        return None;
    }
    let expires = value("X-Amz-Expires")?.parse().ok()?;
    let signature = value("X-Amz-Signature")?;
    if access_key.is_empty()
        || date.is_empty()
        || region.is_empty()
        || service.is_empty()
        || signed_headers.is_empty()
        || signature.is_empty()
    {
        return None;
    }
    Some(PresignedV4Params {
        access_key,
        date,
        region,
        service,
        signed_headers,
        signature,
        expires,
    })
}

fn strip_presign_query(uri: &axum::http::Uri) -> String {
    let Some(query) = uri.query() else {
        return uri.path().to_string();
    };
    let query = query
        .split('&')
        .filter(|part| {
            let name = part.split_once('=').map_or(*part, |(name, _)| name);
            !name.starts_with("X-Amz-")
        })
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        uri.path().to_string()
    } else {
        format!("{}?{query}", uri.path())
    }
}

fn verify_presigned_query(
    header_map: &HeaderMap,
    params: &PresignedV4Params,
    http_method: &Method,
    full_uri: &str,
    secret_key: &str,
) -> bool {
    if params.expires == 0 || params.expires > 604_800 {
        return false;
    }
    let datetime = match parse_date_time(&params.date) {
        Ok(datetime) => datetime,
        Err(_) => return false,
    };
    let expires_at = datetime + Duration::from_secs(params.expires);
    if SystemTime::now() > expires_at {
        return false;
    }
    if params
        .signed_headers
        .iter()
        .any(|header| !header_map.contains_key(header.as_str()))
    {
        return false;
    }

    let identity =
        Credentials::new(params.access_key.clone(), secret_key, None, None, "test").into();
    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(Duration::from_secs(params.expires));
    settings.session_token_mode = SessionTokenMode::Include;
    settings.excluded_headers = Some(vec![
        "authorization".into(),
        "user-agent".into(),
        "x-amzn-trace-id".into(),
    ]);
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    let signer = match SigningParams::builder()
        .identity(&identity)
        .region(&params.region)
        .name(&params.service)
        .time(datetime)
        .settings(settings)
        .build()
    {
        Ok(signer) => signer,
        Err(_) => return false,
    };
    let request = match SignableRequest::new(
        http_method.as_str(),
        full_uri,
        header_map
            .iter()
            .filter(|(key, _)| {
                params
                    .signed_headers
                    .iter()
                    .any(|header| header == key.as_str())
            })
            .filter_map(|(key, value)| value.to_str().ok().map(|value| (key.as_str(), value))),
        SignableBody::UnsignedPayload,
    ) {
        Ok(request) => request,
        Err(_) => return false,
    };
    aws_sigv4::http_request::sign(request, &signer.into())
        .map(|output| output.signature() == params.signature)
        .unwrap_or(false)
}

/// Parses `YYYYMMDD'T'HHMMSS'Z'` formatted dates into a `SystemTime`.
pub(crate) fn parse_date_time(date_time_str: &str) -> Result<SystemTime, Parse> {
    let date_time = PrimitiveDateTime::parse(date_time_str, &DATE_TIME_FORMAT)?.assume_utc();
    Ok(date_time.into())
}

pub fn verify_headers(
    header_map: &HeaderMap,
    params: &S3V4Params,
    http_method: &Method,
    full_host: &str,
    secret_key: &str,
    bytes: &[u8],
) -> bool {
    let payload = match header_map.get("x-amz-content-sha256") {
        Some(header_value) if header_value == HeaderValue::from_static("UNSIGNED-PAYLOAD") => {
            SignableBody::UnsignedPayload
        }
        _ => SignableBody::Bytes(bytes),
    };

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
        http_method.as_str(),
        full_host,
        header_map
            .iter()
            .filter(|(key, _)| params.signed_headers.contains(&key.as_str()))
            .map(|(key, value)| (key.as_str(), value.to_str().unwrap())),
        payload,
    )
    .expect("host is not valid");

    if let Ok(output) = aws_sigv4::http_request::sign(request, &signer.into()) {
        return output.signature() == params.signature;
    }

    false
}

pub fn parse_authorization_header(header_map: &HeaderMap) -> Option<S3V4Params<'_>> {
    let mut params = S3V4Params::default();
    let authorization = header_map
        .get(AUTHORIZATION)
        .and_then(|x| x.to_str().ok())?;
    let (_, rest) = authorization.split_once(" ")?;

    for item in rest.split(",") {
        let item = item.trim();

        match item.split_once("=") {
            Some(("Credential", credential_string)) => {
                let mut credential_parts = credential_string.split('/');
                params.access_key = credential_parts.next()?;
                params.date = credential_parts.next()?;
                params.region = credential_parts.next()?;
                params.service = credential_parts.next()?;
                params.postfix = credential_parts.next()?;
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

    if params.access_key.is_empty() {
        return None;
    }
    if params
        .access_key
        .chars()
        .any(|ch| !ch.is_ascii_alphanumeric())
    {
        return None;
    }

    if params
        .signed_headers
        .iter()
        .any(|header| !header_map.contains_key(*header))
    {
        return None;
    }

    Some(params)
}

#[test]
fn verify_headers_correct_secret_test() {
    let secret_key = "notrealrnrELgWzOk3IfjzDKtFBhDby";
    let mut header_map = HeaderMap::new();
    header_map.insert(
        "user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert(
        "x-amz-user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 api/s3/1.14.0 os/windows lang/rust/1.71.1"),
    );
    header_map.insert("x-amz-date", HeaderValue::from_static("20240203T125727Z"));
    header_map.insert("authorization", HeaderValue::from_static("AWS4-HMAC-SHA256 Credential=ANOTREAL/20240203/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1"));
    header_map.insert(
        "x-amz-content-sha256",
        HeaderValue::from_static(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    );
    header_map.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=3"),
    );
    header_map.insert(
        "amz-sdk-invocation-id",
        HeaderValue::from_static("45ae4a4d-e614-4cfa-854e-108d396d444b"),
    );
    header_map.insert("host", HeaderValue::from_static("127.0.0.1:3000"));

    assert!(verify_headers(
        &header_map,
        &parse_authorization_header(&header_map).unwrap(),
        &Method::GET,
        "http://127.0.0.1:3000/?x-id=ListBuckets",
        secret_key,
        &[]
    ))
}

#[test]
fn verify_headers_incorrect_secret_test() {
    let secret_key = "test1234";
    let mut header_map = HeaderMap::new();
    header_map.insert(
        "user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert(
        "x-amz-user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert("x-amz-date", HeaderValue::from_static("20240203T125727Z"));
    header_map.insert("authorization", HeaderValue::from_static("AWS4-HMAC-SHA256 Credential=ANOTREAL/20240203/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1"));
    header_map.insert(
        "x-amz-content-sha256",
        HeaderValue::from_static(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    );
    header_map.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=3"),
    );
    header_map.insert(
        "amz-sdk-invocation-id",
        HeaderValue::from_static("45ae4a4d-e614-4cfa-854e-108d396d444b"),
    );
    header_map.insert("host", HeaderValue::from_static("127.0.0.1:3000"));

    assert!(!verify_headers(
        &header_map,
        &parse_authorization_header(&header_map).unwrap(),
        &Method::GET,
        "http://127.0.0.1:3000/?x-id=ListBuckets",
        secret_key,
        &[]
    ))
}

#[test]
fn parse_authorization_header_valid_test() {
    let mut header_map = HeaderMap::new();
    header_map.insert(
        "user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert(
        "x-amz-user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert("x-amz-date", HeaderValue::from_static("20240203T125727Z"));
    header_map.insert("authorization", HeaderValue::from_static("AWS4-HMAC-SHA256 Credential=ANOTREAL/20240203/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1"));
    header_map.insert(
        "x-amz-content-sha256",
        HeaderValue::from_static(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    );
    header_map.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=3"),
    );
    header_map.insert(
        "amz-sdk-invocation-id",
        HeaderValue::from_static("45ae4a4d-e614-4cfa-854e-108d396d444b"),
    );
    header_map.insert("host", HeaderValue::from_static("127.0.0.1:3000"));

    let out = parse_authorization_header(&header_map).unwrap();

    let expected = S3V4Params {
        access_key: "ANOTREAL",
        date: "20240203",
        region: "us-west-2",
        service: "s3",
        postfix: "aws4_request",
        signed_headers: vec![
            "host",
            "x-amz-content-sha256",
            "x-amz-date",
            "x-amz-user-agent",
        ],
        signature: "e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1",
    };

    assert_eq!(expected, out);
}

#[test]
fn parse_authorization_header_invalid_access_key_test() {
    let mut header_map = HeaderMap::new();
    header_map.insert(
        "user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert(
        "x-amz-user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert("x-amz-date", HeaderValue::from_static("20240203T125727Z"));
    header_map.insert("authorization", HeaderValue::from_static("AWS4-HMAC-SHA256 Credential=/20240203/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1"));
    header_map.insert(
        "x-amz-content-sha256",
        HeaderValue::from_static(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    );
    header_map.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=3"),
    );
    header_map.insert(
        "amz-sdk-invocation-id",
        HeaderValue::from_static("45ae4a4d-e614-4cfa-854e-108d396d444b"),
    );
    header_map.insert("host", HeaderValue::from_static("127.0.0.1:3000"));

    assert!(parse_authorization_header(&header_map).is_none());
}

#[test]
fn parse_authorization_header_missing_signed_headers_test() {
    let mut header_map = HeaderMap::new();
    header_map.insert(
        "user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert(
        "x-amz-user-agent",
        HeaderValue::from_static("aws-sdk-rust/1.1.4 os/windows lang/rust/1.71.1"),
    );
    header_map.insert("x-amz-date", HeaderValue::from_static("20240203T125727Z"));
    header_map.insert("authorization", HeaderValue::from_static("AWS4-HMAC-SHA256 Credential=ANOTREAL/20240203/us-west-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-user-agent, Signature=e5ad066e3aed7348f9151288c8e4fba48978931ae15f3d9f1247da06131e72e1"));
    header_map.insert(
        "amz-sdk-request",
        HeaderValue::from_static("attempt=1; max=3"),
    );
    header_map.insert(
        "amz-sdk-invocation-id",
        HeaderValue::from_static("45ae4a4d-e614-4cfa-854e-108d396d444b"),
    );
    header_map.insert("host", HeaderValue::from_static("127.0.0.1:3000"));

    assert!(parse_authorization_header(&header_map).is_none());
}
