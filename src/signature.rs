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
use std::time::SystemTime;
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

        let secret_key = match metadata_store.secret_key(params.access_key).await? {
            Some(secret_key) => secret_key,
            None => {
                return Err(s3_error_response(
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "Access Denied",
                )
                .into())
            }
        };

        let external_host = &config.external_server_host;

        if !verify_headers(
            &header_map,
            &params,
            http_method,
            &format!("{external_host}{original_uri}"),
            &secret_key,
            &bytes,
        ) {
            return Err(s3_error_response(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            )
            .into());
        };

        Ok(VerifiedRequest {
            access_key: params.access_key.to_string(),
            namespace: params.access_key.to_string(),
            bytes,
        })
    }
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
