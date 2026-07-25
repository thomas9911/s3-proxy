use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{metadata::MetadataStore, AppState};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{HeaderName, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

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
        metadata_store
            .set_object_metadata(&namespace, &bucket_name, &object_name, &metadata)
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
        metadata_store,
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
        metadata_store,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
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
        metadata_store,
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
) -> Result<Response, RouteError> {
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

async fn add_user_metadata(
    headers: &mut HeaderMap,
    metadata_store: Arc<dyn MetadataStore>,
    namespace: &str,
    bucket: &str,
    object: &str,
) -> Result<(), RouteError> {
    let metadata = metadata_store
        .object_metadata(namespace, bucket, object)
        .await?;
    for (key, value) in metadata {
        headers.insert(
            HeaderName::from_str(&format!("x-amz-meta-{key}"))?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(())
}
