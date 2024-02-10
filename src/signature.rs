use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, PercentEncodingMode, SessionTokenMode, SignableBody, SignableRequest,
    SignatureLocation, SigningSettings, UriPathNormalizationMode,
};
use aws_sigv4::sign::v4::SigningParams;
use axum::http::HeaderMap;
use axum::{extract::FromRequestParts, http::request::Parts};

use std::time::SystemTime;

use async_trait::async_trait;
use time::error::Parse;

#[derive(Debug, Default)]
pub struct S3V4Params<'a> {
    pub access_key: &'a str,
    pub date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub postfix: &'a str,
    pub signed_headers: Vec<&'a str>,
    pub signature: &'a str,
}

use time::{format_description, Date, PrimitiveDateTime, Time};

use crate::AppState;

const DATE_TIME_FORMAT: &str = "[year][month][day]T[hour][minute][second]Z";
const DATE_FORMAT: &str = "[year][month][day]";

struct Signature {}

#[async_trait]
impl FromRequestParts<AppState> for Signature {
    type Rejection = ();

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        todo!()
    }
}

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

pub fn verify_headers(
    header_map: &HeaderMap,
    params: &S3V4Params,
    full_host: &str,
    secret_key: &str,
) -> bool {
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

pub fn parse_authorization_header(header_map: &HeaderMap) -> Option<S3V4Params> {
    let mut params = S3V4Params::default();
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

    if params.access_key == "" {
        return None;
    }
    if params
        .access_key
        .chars()
        .any(|ch| !ch.is_ascii_alphanumeric())
    {
        return None;
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
        &parse_authorization_header(&header_map).unwrap(),
        "http://127.0.0.1:3000/?x-id=ListBuckets",
        secret_key
    ))
}
