use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{metadata::ObjectMetadata, AppState};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{HeaderName, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use std::collections::HashMap;
use std::str::FromStr;

pub async fn create_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    header_map: HeaderMap,
    State(AppState {
        metadata_store,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = signature.namespace;

    if !opendal_operator
        .exists(&format!("{}/{}/", namespace, bucket_name))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    let content_length = signature.bytes.len() as u64;
    let content_type = header_map
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let mut writer = opendal_operator.write_with(&filepath, signature.bytes);

    if let Some(content_type) = content_type.as_deref() {
        writer = writer.content_type(content_type);
    }

    writer.await?;

    let user_metadata: HashMap<String, String> = header_map
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
    let has_explicit_content_type = content_type
        .as_deref()
        .is_some_and(|value| value != "application/octet-stream");
    if has_explicit_content_type || !user_metadata.is_empty() {
        metadata_store
            .set_object_metadata(
                &namespace,
                &bucket_name,
                &object_name,
                &ObjectMetadata {
                    content_type,
                    content_length: Some(content_length),
                    user_metadata,
                    ..Default::default()
                },
            )
            .await?;
    }

    Ok("OK".into_response())
}

pub async fn get_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        metadata_store,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = signature.namespace;

    if !opendal_operator
        .exists(&format!("{}/{}/", namespace, bucket_name))
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
        if !metadata.is_file() {
            return Ok(s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            ));
        }
        metadata
    } else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
        ));
    };
    let stored_metadata = metadata_store
        .object_metadata(&namespace, &bucket_name, &object_name)
        .await?;

    let reader = opendal_operator.reader(&filepath).await?;

    let mut response_headers = HeaderMap::new();

    if let Some(content_type) = metadata
        .content_type()
        .or(stored_metadata.content_type.as_deref())
    {
        response_headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    if let Some(etag) = metadata.etag().or(stored_metadata.etag.as_deref()) {
        response_headers.insert(ETAG, HeaderValue::from_str(etag)?);
    }
    if let Some(last_modified) = metadata.last_modified() {
        response_headers.insert(
            LAST_MODIFIED,
            HeaderValue::from_str(&httpdate::fmt_http_date(last_modified.into()))?,
        );
    }
    add_user_metadata(&mut response_headers, &stored_metadata).await?;

    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.content_length().to_string())?,
    );

    let stream = reader.into_bytes_stream(..).await?;
    Ok((response_headers, Body::from_stream(stream)).into_response())
}

pub async fn head_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        metadata_store,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = signature.namespace;
    let bucket_path = format!("{}/{}/", namespace, bucket_name);
    if !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    let metadata = match opendal_operator.stat(&filepath).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) | Err(_) => {
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
    let stored_metadata = metadata_store
        .object_metadata(&namespace, &bucket_name, &object_name)
        .await?;
    if let Some(content_type) = metadata
        .content_type()
        .or(stored_metadata.content_type.as_deref())
    {
        headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    if let Some(etag) = metadata.etag().or(stored_metadata.etag.as_deref()) {
        headers.insert(ETAG, HeaderValue::from_str(etag)?);
    }
    if let Some(last_modified) = metadata.last_modified() {
        headers.insert(
            LAST_MODIFIED,
            HeaderValue::from_str(&httpdate::fmt_http_date(last_modified.into()))?,
        );
    }
    add_user_metadata(&mut headers, &stored_metadata).await?;
    Ok((StatusCode::OK, headers).into_response())
}

pub async fn delete_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        metadata_store,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = signature.namespace;
    if !opendal_operator
        .exists(&format!("{}/{}/", namespace, bucket_name))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    if opendal_operator.exists(&filepath).await? {
        opendal_operator.delete(&filepath).await?;
    }
    metadata_store
        .delete_object_metadata(&namespace, &bucket_name, &object_name)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn add_user_metadata(
    headers: &mut HeaderMap,
    metadata: &ObjectMetadata,
) -> Result<(), RouteError> {
    for (key, value) in &metadata.user_metadata {
        headers.insert(
            HeaderName::from_str(&format!("x-amz-meta-{key}"))?,
            HeaderValue::from_str(value)?,
        );
    }
    Ok(())
}
