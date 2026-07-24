use std::borrow::Cow;

use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{templates, AppState};
use aws_sigv4::sign::v4::{calculate_signature, generate_signing_key};
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::header::{HeaderName, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum_route_error::RouteError;
use base64::Engine;
use deadpool_redis::redis::AsyncCommands;
use opendal::Metakey;
use std::collections::HashMap;
use std::str::FromStr;
use tokio_stream::StreamExt;

pub async fn list_buckets(
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = &signature.namespace;

    let mut lister = opendal_operator
        .lister_with(&format!("{}/", namespace))
        .await?;

    let mut buckets = Vec::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(entry) => {
                if entry.metadata().is_dir() {
                    buckets.push(templates::ListBucketItem {
                        name: entry.name().trim_end_matches('/').to_string().into(),
                        timestamp: None,
                    })
                }
            }
            Err(error) => {
                tracing::error!("{}", error);
                return Err(RouteError::new_internal_server());
            }
        }
    }

    let template = templates::ListBucketsTemplate {
        owner_name: "Testing",
        owner_id: "1",
        buckets,
    };

    Ok(askama_axum::into_response(&template))
}

pub async fn create_bucket(
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

pub async fn delete_bucket(
    Path(bucket_name): Path<String>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;
    let bucket_path = format!("{}/{}/", namespace, bucket_name);
    if !opendal_operator.is_exist(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    opendal_operator.delete(&bucket_path).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn post_object(
    Path(bucket_name): Path<String>,
    State(AppState {
        metadata_pool,
        opendal_operator,
        ..
    }): State<AppState>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, RouteError> {
    let mut fields = HashMap::new();
    let mut file = None;
    let mut filename = None;
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name.eq_ignore_ascii_case("file") {
            filename = field.file_name().map(ToOwned::to_owned);
            file = Some(field.bytes().await?);
        } else {
            fields.insert(name.to_ascii_lowercase(), field.text().await?);
        }
    }

    let credential = match fields.get("x-amz-credential") {
        Some(value) => value,
        None => {
            return Ok(s3_error_response(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "Access Denied",
            ))
        }
    };
    let mut credential_parts = credential.split('/');
    let access_key = credential_parts.next().unwrap_or_default();
    let _date = credential_parts.next().unwrap_or_default();
    let region = credential_parts.next().unwrap_or_default();
    let service = credential_parts.next().unwrap_or_default();
    if service != "s3" || credential_parts.next() != Some("aws4_request") {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied",
        ));
    }

    let mut conn = metadata_pool.get().await?;
    let secret_key: Option<String> = conn.get(format!("secret_key::{access_key}")).await?;
    let Some(secret_key) = secret_key else {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied",
        ));
    };
    if !opendal_operator
        .is_exist(&format!("{access_key}/{bucket_name}/"))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    let policy = match fields.get("policy") {
        Some(value) => value,
        None => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedPOSTRequest",
                "Invalid policy",
            ))
        }
    };
    let signature = match fields.get("x-amz-signature") {
        Some(value) => value,
        None => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedPOSTRequest",
                "Missing signature",
            ))
        }
    };
    let signing_time = match crate::signature::parse_date_time(
        fields
            .get("x-amz-date")
            .map(String::as_str)
            .unwrap_or_default(),
    ) {
        Ok(time) => time,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedPOSTRequest",
                "Invalid date",
            ))
        }
    };
    let expected = calculate_signature(
        generate_signing_key(&secret_key, signing_time, region, service),
        policy.as_bytes(),
    );
    if expected != *signature {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        ));
    }

    let policy_json = match base64::engine::general_purpose::STANDARD.decode(policy) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedPOSTRequest",
                "Invalid policy",
            ))
        }
    };
    let policy: serde_json::Value = match serde_json::from_slice(&policy_json) {
        Ok(policy) => policy,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedPOSTRequest",
                "Invalid policy",
            ))
        }
    };
    if !policy_allows(
        &policy,
        &fields,
        &bucket_name,
        file.as_ref().map(|body| body.len()).unwrap_or(0),
    ) {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPolicyDocument",
            "Policy conditions failed",
        ));
    }

    let key_template = fields.get("key").cloned().unwrap_or_default();
    let key = key_template.replace("${filename}", filename.as_deref().unwrap_or_default());
    let Some(file) = file else {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "MalformedPOSTRequest",
            "Missing file",
        ));
    };
    let mut writer =
        opendal_operator.write_with(&format!("{access_key}/{bucket_name}/{key}"), file);
    if let Some(content_type) = fields.get("content-type") {
        writer = writer.content_type(content_type);
    }
    writer.await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn policy_allows(
    policy: &serde_json::Value,
    fields: &HashMap<String, String>,
    bucket: &str,
    size: usize,
) -> bool {
    let Some(conditions) = policy
        .get("conditions")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    for condition in conditions {
        if let Some(object) = condition.as_object() {
            for (key, value) in object {
                let field = key.to_ascii_lowercase();
                let expected = value.as_str().unwrap_or_default();
                if field == "bucket" && expected != bucket {
                    return false;
                }
                if field != "bucket" && fields.get(&field).map(String::as_str) != Some(expected) {
                    return false;
                }
            }
        } else if let Some(array) = condition.as_array() {
            if array.first().and_then(serde_json::Value::as_str) == Some("content-length-range") {
                let min = array
                    .get(1)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize;
                let max = array
                    .get(2)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize;
                if size < min || size > max {
                    return false;
                }
            }
        }
    }
    true
}

pub async fn create_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    header_map: HeaderMap,
    State(AppState {
        metadata_pool,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;

    if !opendal_operator
        .is_exist(&format!("{}/{}/", namespace, bucket_name))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let mut writer = opendal_operator.write_with(
        &format!("{}/{}/{}", namespace, bucket_name, object_name),
        signature.bytes,
    );

    writer = if let Some(content_type) = header_map.get(CONTENT_TYPE) {
        if let Ok(content_type) = content_type.to_str() {
            writer.content_type(content_type)
        } else {
            writer
        }
    } else {
        writer
    };

    writer.await?;

    let metadata: HashMap<String, String> = header_map
        .iter()
        .filter_map(|(name, value)| {
            name.as_str().strip_prefix("x-amz-meta-").and_then(|key| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (key.to_string(), value.to_string()))
            })
        })
        .collect();
    if !metadata.is_empty() {
        let mut conn = metadata_pool.get().await?;
        let items: Vec<_> = metadata.iter().collect();
        let _: () = conn
            .hset_multiple(
                object_metadata_key(&namespace, &bucket_name, &object_name),
                &items,
            )
            .await?;
    }

    Ok("OK".into_response())
}

pub async fn get_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        metadata_pool,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;

    if !opendal_operator
        .is_exist(&format!("{}/{}/", namespace, bucket_name))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    let metadata = if let Ok(metadata) = opendal_operator.stat(&filepath).await {
        metadata
    } else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
        ));
    };

    let reader = opendal_operator.reader(&filepath).await?;

    let mut response_headers = HeaderMap::new();

    if let Some(content_type) = metadata.content_type() {
        response_headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    add_user_metadata(
        &mut response_headers,
        metadata_pool,
        &namespace,
        &bucket_name,
        &object_name,
    )
    .await?;

    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.content_length().to_string())?,
    );

    Ok((response_headers, Body::from_stream(reader)).into_response())
}

pub async fn head_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        metadata_pool,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;
    let bucket_path = format!("{}/{}/", namespace, bucket_name);
    if !opendal_operator.is_exist(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    let metadata = match opendal_operator.stat(&filepath).await {
        Ok(metadata) => metadata,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            ))
        }
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.content_length().to_string())?,
    );
    if let Some(content_type) = metadata.content_type() {
        headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    add_user_metadata(
        &mut headers,
        metadata_pool,
        &namespace,
        &bucket_name,
        &object_name,
    )
    .await?;
    Ok((StatusCode::OK, headers).into_response())
}

pub async fn delete_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;
    if !opendal_operator
        .is_exist(&format!("{}/{}/", namespace, bucket_name))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    if opendal_operator.is_exist(&filepath).await? {
        opendal_operator.delete(&filepath).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn list_objects(
    Path(bucket_name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = &signature.namespace;

    if !opendal_operator
        .is_exist(&format!("{}/{}/", namespace, bucket_name))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let mut lister = opendal_operator
        .lister_with(&format!("{}/{}/", namespace, bucket_name))
        .recursive(true)
        .metakey(Metakey::ContentLength)
        .await?;

    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let max_keys = query
        .get("max-keys")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1000)
        .clamp(1, 1000);
    let offset = query
        .get("continuation-token")
        .map(String::as_str)
        .and_then(|token| token.parse::<usize>().ok())
        .unwrap_or(0);
    let mut all_objects = Vec::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(entry) => {
                let metadata = entry.metadata();
                if metadata.is_file() {
                    let key = entry
                        .path()
                        .strip_prefix(&format!("{}/{}/", namespace, bucket_name))
                        .unwrap_or(entry.path());
                    let etag = metadata.etag().map(|y| Cow::from(y.to_string()));
                    let last_modified = metadata
                        .last_modified()
                        .map(|dt| Cow::from(dt.to_rfc3339()));
                    let size = metadata.content_length();
                    if key.starts_with(&prefix) {
                        all_objects.push(templates::ListObjectItem {
                            key: Cow::Owned(key.to_string()),
                            etag,
                            last_modified,
                            size,
                        });
                    }
                }
            }
            Err(error) => {
                tracing::error!("{}", error);
                return Err(RouteError::new_internal_server());
            }
        }
    }

    all_objects.sort_by(|left, right| left.key.cmp(&right.key));
    let start = offset.min(all_objects.len());
    let end = (start + max_keys as usize).min(all_objects.len());
    let is_truncated = end < all_objects.len();
    let next_continuation_token = if is_truncated {
        end.to_string()
    } else {
        String::new()
    };
    let objects = all_objects
        .into_iter()
        .skip(start)
        .take(end - start)
        .collect();

    let template = templates::ListObjectsTemplate {
        objects,
        is_truncated,
        continuation_token: query
            .get("continuation-token")
            .cloned()
            .unwrap_or_default()
            .into(),
        next_continuation_token: next_continuation_token.into(),
        key_count: (end - start) as u64,
        bucket_name: Cow::from(bucket_name),
        prefix: prefix.into(),
        max_keys,
    };

    Ok(askama_axum::into_response(&template))
}

fn object_metadata_key(namespace: &str, bucket: &str, object: &str) -> String {
    format!("object_metadata::{namespace}/{bucket}/{object}")
}

async fn add_user_metadata(
    headers: &mut HeaderMap,
    metadata_pool: deadpool_redis::Pool,
    namespace: &str,
    bucket: &str,
    object: &str,
) -> Result<(), RouteError> {
    let mut conn = metadata_pool.get().await?;
    let metadata: HashMap<String, String> = conn
        .hgetall(object_metadata_key(namespace, bucket, object))
        .await?;
    for (key, value) in metadata {
        headers.insert(
            HeaderName::from_str(&format!("x-amz-meta-{key}"))?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(())
}
