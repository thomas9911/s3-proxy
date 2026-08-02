use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{metadata::ObjectMetadata, storage, templates, AppState};
use axum::body::Body;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::header::{
    HeaderName, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::SystemTime;

pub async fn create_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    header_map: HeaderMap,
    State(AppState {
        metadata_store,
        opendal_operator,
        config,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    let namespace = signature.namespace;

    let Some(bucket_path) = storage::bucket_prefix(&config, &namespace, &bucket_name) else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    };
    if !storage::is_single_bucket(&config) && !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let filepath = storage::object_path(&config, &namespace, &bucket_name, &object_name)
        .expect("bucket path was validated above");
    let content_length = signature.bytes.len() as u64;
    if !crate::quota::allows_storage(
        &config.quotas,
        &opendal_operator,
        &namespace,
        Some(&filepath),
        content_length,
    )
    .await?
    {
        return Ok(s3_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "QuotaExceeded",
            "The storage quota for this principal would be exceeded.",
        ));
    }
    crate::versioning::prepare_overwrite(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
        &filepath,
    )
    .await?;
    let content_type = header_map
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let mut writer = opendal_operator.write_with(&filepath, signature.bytes);

    if let Some(content_type) = content_type.as_deref() {
        writer = writer.content_type(content_type);
    }

    writer.await?;

    if let Err(error) = crate::retry::retry("replace_object_metadata", || {
        metadata_store.delete_object_metadata(&namespace, &bucket_name, &object_name)
    })
    .await
    {
        let _ = opendal_operator.delete(&filepath).await;
        return Err(error.into());
    }
    if let Err(error) = crate::retry::retry("reset_object_public_acl", || {
        metadata_store.set_object_public(&namespace, &bucket_name, &object_name, false)
    })
    .await
    {
        let _ = opendal_operator.delete(&filepath).await;
        return Err(error.into());
    }

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
    if let Err(error) = metadata_store
        .set_object_metadata(
            &namespace,
            &bucket_name,
            &object_name,
            &ObjectMetadata {
                content_type,
                content_length: Some(content_length),
                last_modified: Some(SystemTime::now()),
                user_metadata,
                ..Default::default()
            },
        )
        .await
    {
        let _ = opendal_operator.delete(&filepath).await;
        return Err(error.into());
    }
    if let Some(public) = public_acl(&header_map) {
        if let Err(error) = metadata_store
            .set_object_public(&namespace, &bucket_name, &object_name, public)
            .await
        {
            let _ = opendal_operator.delete(&filepath).await;
            let _ = metadata_store
                .delete_object_metadata(&namespace, &bucket_name, &object_name)
                .await;
            return Err(error.into());
        }
    }

    let version_id = crate::versioning::record_put(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
        &filepath,
    )
    .await?;
    let mut response = StatusCode::OK.into_response();
    if let Some(version_id) = version_id {
        response
            .headers_mut()
            .insert("x-amz-version-id", HeaderValue::from_str(&version_id)?);
    }
    Ok(response)
}

pub async fn put_object(
    path: Path<(String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let has_authorization = headers.contains_key(AUTHORIZATION);
    let has_presigned_query = crate::signature::has_presigned_query(request.uri());
    let (bucket_name, object_name) = path.0.clone();
    let part_number = query_value(request.uri().query(), "partNumber")
        .and_then(|value| value.parse::<u32>().ok());
    let upload_id = query_value(request.uri().query(), "uploadId");
    let mut verified = if has_authorization || has_presigned_query {
        match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return error.into_response(),
        }
    } else {
        let bytes = match axum::body::Bytes::from_request(request, &state).await {
            Ok(bytes) => bytes,
            Err(error) => return error.into_response(),
        };
        let Some(namespace) =
            resolve_policy_namespace(&state, &bucket_name, &object_name, "s3:PutObject", None)
                .await
        else {
            return s3_error_response(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        };
        VerifiedRequest {
            access_key: String::new(),
            namespace,
            bytes,
        }
    };
    if has_authorization {
        let namespace = match authorize_policy_namespace(
            &state,
            &bucket_name,
            &object_name,
            "s3:PutObject",
            &verified.namespace,
            &verified.namespace,
        )
        .await
        {
            Ok(namespace) => namespace,
            Err(response) => return response,
        };
        verified.namespace = namespace;
    }
    if let (Some(part_number), Some(upload_id)) = (part_number, upload_id) {
        return super::post::upload_part(
            bucket_name,
            object_name,
            state,
            verified,
            upload_id,
            part_number,
        )
        .await
        .map_or_else(IntoResponse::into_response, |response| response);
    }
    if let Some(source) = headers.get("x-amz-copy-source").cloned() {
        return copy_object(path, headers, state, verified, &source).await;
    }
    create_object(path, headers, State(state), verified)
        .await
        .map_or_else(IntoResponse::into_response, |response| response)
}

async fn copy_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    headers: HeaderMap,
    state: AppState,
    signature: VerifiedRequest,
    source_header: &HeaderValue,
) -> Response {
    let source = match source_header
        .to_str()
        .ok()
        .and_then(|source| urlencoding::decode(source).ok())
    {
        Some(source) => source.trim_start_matches('/').to_string(),
        None => {
            return s3_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "The copy source is invalid.",
            )
        }
    };
    let Some((source_bucket, source_object)) = source.split_once('/') else {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "The copy source is invalid.",
        );
    };
    let AppState {
        metadata_store,
        opendal_operator,
        config,
        ..
    } = state;
    let namespace = signature.namespace;
    let Some(destination_bucket) = storage::bucket_prefix(&config, &namespace, &bucket_name) else {
        return s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        );
    };
    if !storage::is_single_bucket(&config)
        && !opendal_operator
            .exists(&destination_bucket)
            .await
            .unwrap_or(false)
    {
        return s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        );
    }
    let Some(from) = storage::object_path(&config, &namespace, source_bucket, source_object) else {
        return s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified source bucket does not exist.",
        );
    };
    let to = storage::object_path(&config, &namespace, &bucket_name, &object_name)
        .expect("destination bucket path was validated above");
    if !opendal_operator.exists(&from).await.unwrap_or(false) {
        return s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
        );
    }
    let source_size = match opendal_operator.stat(&from).await {
        Ok(metadata) => metadata.content_length(),
        Err(error) => {
            tracing::error!(%error, "failed to stat copy source");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !crate::quota::allows_storage(
        &config.quotas,
        &opendal_operator,
        &namespace,
        Some(&to),
        source_size,
    )
    .await
    .unwrap_or(false)
    {
        return s3_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "QuotaExceeded",
            "The storage quota for this principal would be exceeded.",
        );
    }
    if let Err(error) = crate::versioning::prepare_overwrite(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
        &to,
    )
    .await
    {
        tracing::error!(%error, "failed to prepare copied object version");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(error) = opendal_operator.copy(&from, &to).await {
        tracing::debug!(%error, "native copy unavailable, falling back to read/write");
        let bytes = match opendal_operator.read(&from).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(%error, "failed to read copy source");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        if let Err(error) = opendal_operator.write(&to, bytes).await {
            tracing::error!(%error, "failed to write copied object");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    if let Err(error) = crate::retry::retry("replace_copied_object_metadata", || {
        metadata_store.delete_object_metadata(&namespace, &bucket_name, &object_name)
    })
    .await
    {
        tracing::error!(%error, "failed to replace copied object metadata");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(error) = crate::retry::retry("reset_copied_object_public_acl", || {
        metadata_store.set_object_public(&namespace, &bucket_name, &object_name, false)
    })
    .await
    {
        tracing::error!(%error, "failed to reset copied object visibility");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Ok(metadata) = metadata_store
        .object_metadata(&namespace, source_bucket, source_object)
        .await
    {
        if let Err(error) = metadata_store
            .set_object_metadata(&namespace, &bucket_name, &object_name, &metadata)
            .await
        {
            tracing::error!(%error, "failed to copy object metadata");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    if let Some(public) = public_acl(&headers) {
        if let Err(error) = metadata_store
            .set_object_public(&namespace, &bucket_name, &object_name, public)
            .await
        {
            tracing::error!(%error, "failed to set copied object visibility");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let version_id = match crate::versioning::record_put(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
        &to,
    )
    .await
    {
        Ok(version_id) => version_id,
        Err(error) => {
            tracing::error!(%error, "failed to store copied object version");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut response = templates::xml_response(StatusCode::OK, templates::CopyObjectTemplate);
    if let Some(version_id) = version_id {
        let Ok(version_id) = HeaderValue::from_str(&version_id) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        response
            .headers_mut()
            .insert("x-amz-version-id", version_id);
    }
    response
}

pub async fn get_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    if let Some(upload_id) = query_value(request.uri().query(), "uploadId") {
        let verified = match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return Ok(error.into_response()),
        };
        return super::post::list_parts(bucket_name, object_name, state, verified, upload_id).await;
    }
    let range_header = request.headers().get(RANGE).cloned();
    let version_id = query_value(request.uri().query(), "versionId");
    let namespace = match resolve_read_namespace(request, &state, &bucket_name, &object_name).await
    {
        Ok(namespace) => namespace,
        Err(response) => return Ok(response),
    };
    let AppState {
        metadata_store,
        opendal_operator,
        config,
        ..
    } = state;

    let Some(bucket_path) = storage::bucket_prefix(&config, &namespace, &bucket_name) else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    };
    if !storage::is_single_bucket(&config) && !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let (filepath, selected_version_id) = match crate::versioning::version(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
        version_id.as_deref(),
    )
    .await
    {
        Ok(Some(version)) if version.is_delete_marker => {
            return Ok(s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            ))
        }
        Ok(Some(version)) => (
            version.data_path.expect("non-marker version has data"),
            Some(version.version_id),
        ),
        Ok(None) if version_id.is_some() => {
            return Ok(s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchVersion",
                "The specified version does not exist.",
            ))
        }
        Ok(None) => (
            storage::object_path(&config, &namespace, &bucket_name, &object_name)
                .expect("bucket path was validated above"),
            None,
        ),
        Err(error) => {
            tracing::error!(%error, "failed to resolve object version");
            return Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
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

    let object_length = metadata.content_length();
    let range = match range_header
        .as_ref()
        .map(|value| parse_range(value, object_length))
        .transpose()
    {
        Ok(range) => range,
        Err(response) => return Ok(response),
    };
    let reader = opendal_operator.reader(&filepath).await?;

    let mut response_headers = HeaderMap::new();

    if let Some(version_id) = selected_version_id {
        response_headers.insert("x-amz-version-id", HeaderValue::from_str(&version_id)?);
    }

    if let Some(content_type) = metadata
        .content_type()
        .or(stored_metadata.content_type.as_deref())
    {
        response_headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    if let Some(etag) = metadata.etag().or(stored_metadata.etag.as_deref()) {
        response_headers.insert(ETAG, HeaderValue::from_str(etag)?);
    }
    let last_modified = metadata
        .last_modified()
        .map(SystemTime::from)
        .or(stored_metadata.last_modified)
        .unwrap_or_else(SystemTime::now);
    response_headers.insert(
        LAST_MODIFIED,
        HeaderValue::from_str(&httpdate::fmt_http_date(last_modified.into()))?,
    );
    add_user_metadata(&mut response_headers, &stored_metadata).await?;

    let response_length = range
        .as_ref()
        .map_or(object_length, |range| range.end - range.start);
    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&response_length.to_string())?,
    );
    response_headers.insert("accept-ranges", HeaderValue::from_static("bytes"));
    if let Some(range) = &range {
        response_headers.insert(
            "content-range",
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                range.start,
                range.end - 1,
                object_length
            ))?,
        );
    }

    let stream = reader
        .into_bytes_stream(range.clone().unwrap_or(0..object_length))
        .await?;
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    Ok((status, response_headers, Body::from_stream(stream)).into_response())
}

fn parse_range(value: &HeaderValue, length: u64) -> Result<std::ops::Range<u64>, Response> {
    let value = value.to_str().ok();
    let Some(value) = value.and_then(|value| value.strip_prefix("bytes=")) else {
        return Err(range_not_satisfiable(length));
    };
    if value.contains(',') {
        return Err(range_not_satisfiable(length));
    }
    let Some((start, end)) = value.split_once('-') else {
        return Err(range_not_satisfiable(length));
    };
    let range = if start.is_empty() {
        let suffix = end.parse::<u64>().ok();
        let Some(suffix) = suffix.filter(|suffix| *suffix > 0) else {
            return Err(range_not_satisfiable(length));
        };
        length.saturating_sub(suffix)..length
    } else {
        let Some(start) = start.parse::<u64>().ok().filter(|start| *start < length) else {
            return Err(range_not_satisfiable(length));
        };
        let end = end
            .parse::<u64>()
            .ok()
            .map_or(length - 1, |end| end.min(length - 1));
        if end < start {
            return Err(range_not_satisfiable(length));
        }
        start..end + 1
    };
    Ok(range)
}

fn range_not_satisfiable(length: u64) -> Response {
    let mut response = templates::xml_response(
        StatusCode::RANGE_NOT_SATISFIABLE,
        templates::InvalidRangeTemplate,
    );
    response.headers_mut().insert(
        "content-range",
        HeaderValue::from_str(&format!("bytes */{length}"))
            .expect("content range generated from a u64 is valid"),
    );
    response
}

pub async fn head_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    let version_id = query_value(request.uri().query(), "versionId");
    let namespace = match resolve_read_namespace(request, &state, &bucket_name, &object_name).await
    {
        Ok(namespace) => namespace,
        Err(response) => return Ok(response),
    };
    let AppState {
        metadata_store,
        opendal_operator,
        config,
        ..
    } = state;
    let Some(bucket_path) = storage::bucket_prefix(&config, &namespace, &bucket_name) else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    };
    if !storage::is_single_bucket(&config) && !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let (filepath, selected_version_id) = match crate::versioning::version(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
        version_id.as_deref(),
    )
    .await
    {
        Ok(Some(version)) if version.is_delete_marker => {
            return Ok(s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "The specified key does not exist.",
            ))
        }
        Ok(Some(version)) => (
            version.data_path.expect("non-marker version has data"),
            Some(version.version_id),
        ),
        Ok(None) if version_id.is_some() => {
            return Ok(s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchVersion",
                "The specified version does not exist.",
            ))
        }
        Ok(None) => (
            storage::object_path(&config, &namespace, &bucket_name, &object_name)
                .expect("bucket path was validated above"),
            None,
        ),
        Err(error) => {
            tracing::error!(%error, "failed to resolve object version");
            return Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
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
    if let Some(version_id) = selected_version_id {
        headers.insert("x-amz-version-id", HeaderValue::from_str(&version_id)?);
    }
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
    let last_modified = metadata
        .last_modified()
        .map(SystemTime::from)
        .or(stored_metadata.last_modified)
        .unwrap_or_else(SystemTime::now);
    headers.insert(
        LAST_MODIFIED,
        HeaderValue::from_str(&httpdate::fmt_http_date(last_modified.into()))?,
    );
    add_user_metadata(&mut headers, &stored_metadata).await?;
    Ok((StatusCode::OK, headers).into_response())
}

pub async fn delete_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        metadata_store,
        opendal_operator,
        config,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    let namespace = signature.namespace;
    let Some(bucket_path) = storage::bucket_prefix(&config, &namespace, &bucket_name) else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    };
    if !storage::is_single_bucket(&config) && !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    let filepath = storage::object_path(&config, &namespace, &bucket_name, &object_name)
        .expect("bucket path was validated above");
    if opendal_operator.exists(&filepath).await? {
        crate::retry::retry("delete_object", || opendal_operator.delete(&filepath)).await?;
    }
    crate::retry::retry("delete_object_metadata", || {
        metadata_store.delete_object_metadata(&namespace, &bucket_name, &object_name)
    })
    .await?;
    metadata_store
        .set_object_public(&namespace, &bucket_name, &object_name, false)
        .await?;
    let version_id = crate::versioning::record_delete_marker(
        &opendal_operator,
        &namespace,
        &bucket_name,
        &object_name,
    )
    .await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Some(version_id) = version_id {
        response
            .headers_mut()
            .insert("x-amz-version-id", HeaderValue::from_str(&version_id)?);
        response
            .headers_mut()
            .insert("x-amz-delete-marker", HeaderValue::from_static("true"));
    }
    Ok(response)
}

pub async fn delete_object_route(
    path: Path<(String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let (bucket_name, object_name) = path.0.clone();
    let upload_id = query_value(request.uri().query(), "uploadId");
    let mut verified = if headers.contains_key(AUTHORIZATION)
        || crate::signature::has_presigned_query(request.uri())
    {
        match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return error.into_response(),
        }
    } else {
        if upload_id.is_some() {
            return s3_error_response(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        }
        let Some(namespace) =
            resolve_policy_namespace(&state, &bucket_name, &object_name, "s3:DeleteObject", None)
                .await
        else {
            return s3_error_response(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        };
        VerifiedRequest {
            access_key: String::new(),
            namespace,
            bytes: axum::body::Bytes::new(),
        }
    };
    if !verified.access_key.is_empty() {
        let namespace = match authorize_policy_namespace(
            &state,
            &bucket_name,
            &object_name,
            "s3:DeleteObject",
            &verified.namespace,
            &verified.namespace,
        )
        .await
        {
            Ok(namespace) => namespace,
            Err(response) => return response,
        };
        verified.namespace = namespace;
    }
    if let Some(upload_id) = upload_id {
        return super::post::abort_multipart(bucket_name, state, verified, upload_id)
            .await
            .map_or_else(IntoResponse::into_response, |response| response);
    }
    delete_object(path, State(state), verified)
        .await
        .map_or_else(IntoResponse::into_response, |response| response)
}

pub(crate) fn public_acl(headers: &HeaderMap) -> Option<bool> {
    headers
        .get("x-amz-acl")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| match value {
            "public-read" => Some(true),
            "private" => Some(false),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::{copy_object, create_object, get_object, public_acl, resolve_read_namespace};
    use crate::metadata::{MetadataStore, SqliteMetadataStore};
    use crate::signature::VerifiedRequest;
    use crate::{AppState, Config, SqliteConfig};
    use axum::body::{to_bytes, Body, Bytes};
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use opendal::services::Memory;
    use opendal::Operator;
    use std::collections::HashMap;
    use std::sync::Arc;

    async fn test_state() -> (AppState, Arc<SqliteMetadataStore>, Operator) {
        let metadata_store = Arc::new(
            SqliteMetadataStore::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let operator = Operator::new(Memory::default()).unwrap();
        let config = Config {
            server_host: "127.0.0.1:0".to_string(),
            external_server_host: "http://127.0.0.1:0".to_string(),
            max_request_body_bytes: 256 * 1024 * 1024,
            metadata_backend: crate::metadata::MetaDataBackend::Sqlite,
            redis: None,
            sqlite: Some(SqliteConfig {
                url: "sqlite::memory:".to_string(),
            }),
            postgres: None,
            admin: None,
            #[cfg(feature = "management")]
            management: None,
            quotas: crate::quota::QuotaConfig::default(),
            opendal_provider: "memory".to_string(),
            opendal: HashMap::new(),
            storage_layout: crate::StorageLayout::Namespaced,
            single_bucket: None,
        };
        (
            AppState {
                metadata_store: metadata_store.clone(),
                config: Arc::new(config),
                opendal_operator: operator.clone(),
            },
            metadata_store,
            operator,
        )
    }

    #[test]
    fn parses_public_acl_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(public_acl(&headers), None);
        headers.insert("x-amz-acl", HeaderValue::from_static("public-read"));
        assert_eq!(public_acl(&headers), Some(true));
        headers.insert("x-amz-acl", HeaderValue::from_static("private"));
        assert_eq!(public_acl(&headers), Some(false));
    }

    #[tokio::test]
    async fn object_handlers_cover_overwrite_copy_and_range_read() {
        let (state, metadata_store, operator) = test_state().await;
        operator.create_dir("namespace/bucket/").await.unwrap();
        let original = Bytes::from_static(b"original body");
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        headers.insert("x-amz-meta-old", HeaderValue::from_static("value"));
        headers.insert("x-amz-acl", HeaderValue::from_static("public-read"));
        let response = create_object(
            Path(("bucket".to_string(), "source.txt".to_string())),
            headers,
            State(state.clone()),
            VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: original.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        metadata_store
            .set_bucket_public("namespace", "bucket", true)
            .await
            .unwrap();
        let public_bucket_namespace = resolve_read_namespace(
            Request::builder()
                .uri("/bucket/missing")
                .body(Body::empty())
                .unwrap(),
            &state,
            "bucket",
            "missing",
        )
        .await
        .unwrap();
        assert_eq!(public_bucket_namespace, "namespace");
        metadata_store
            .set_bucket_public("namespace", "bucket", false)
            .await
            .unwrap();
        metadata_store
            .set_bucket_policy(
                "policy-namespace",
                "policy-bucket",
                r#"{"Version":"2012-10-17","Statement":{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::policy-bucket/*"}}"#,
            )
            .await
            .unwrap();
        let policy_namespace = resolve_read_namespace(
            Request::builder()
                .uri("/policy-bucket/object")
                .body(Body::empty())
                .unwrap(),
            &state,
            "policy-bucket",
            "object",
        )
        .await
        .unwrap();
        assert_eq!(policy_namespace, "policy-namespace");

        let copied = copy_object(
            Path(("bucket".to_string(), "copied.txt".to_string())),
            HeaderMap::new(),
            state.clone(),
            VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: Bytes::new(),
            },
            &HeaderValue::from_static("bucket/source.txt"),
        )
        .await;
        assert_eq!(copied.status(), StatusCode::OK);

        let copied_metadata = metadata_store
            .object_metadata("namespace", "bucket", "copied.txt")
            .await
            .unwrap();
        assert_eq!(copied_metadata.content_type.as_deref(), Some("text/plain"));
        assert_eq!(
            metadata_store
                .public_object_namespace("bucket", "copied.txt")
                .await
                .unwrap(),
            None
        );

        let range_request = Request::builder()
            .uri("/bucket/source.txt")
            .header("range", "bytes=0-7")
            .body(Body::empty())
            .unwrap();
        let range_response = get_object(
            Path(("bucket".to_string(), "source.txt".to_string())),
            State(state.clone()),
            range_request,
        )
        .await
        .unwrap();
        assert_eq!(range_response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            to_bytes(range_response.into_body(), 1024).await.unwrap(),
            Bytes::from_static(b"original")
        );

        let mut overwrite_headers = HeaderMap::new();
        overwrite_headers.insert("content-type", HeaderValue::from_static("text/plain"));
        let replacement = Bytes::from_static(b"replacement");
        create_object(
            Path(("bucket".to_string(), "source.txt".to_string())),
            overwrite_headers,
            State(state),
            VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: replacement,
            },
        )
        .await
        .unwrap();
        let metadata = metadata_store
            .object_metadata("namespace", "bucket", "source.txt")
            .await
            .unwrap();
        assert!(metadata.user_metadata.is_empty());
        assert_eq!(
            metadata_store
                .public_object_namespace("bucket", "source.txt")
                .await
                .unwrap(),
            None
        );
    }
}

async fn resolve_policy_namespace(
    state: &AppState,
    bucket_name: &str,
    object_name: &str,
    action: &str,
    principal: Option<&str>,
) -> Option<String> {
    let resource = format!("arn:aws:s3:::{bucket_name}/{object_name}");
    let policies = state
        .metadata_store
        .bucket_policies(bucket_name)
        .await
        .ok()?;
    policies.into_iter().find_map(|(namespace, policy)| {
        crate::policy::decision(&policy, principal, action, &resource)
            .ok()
            .filter(|decision| *decision == crate::policy::Decision::Allow)
            .map(|_| namespace)
    })
}

async fn authorize_policy_namespace(
    state: &AppState,
    bucket_name: &str,
    object_name: &str,
    action: &str,
    principal: &str,
    fallback: &str,
) -> Result<String, Response> {
    let resource = format!("arn:aws:s3:::{bucket_name}/{object_name}");
    for (namespace, policy) in state
        .metadata_store
        .bucket_policies(bucket_name)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to resolve bucket policy");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?
    {
        match crate::policy::decision(&policy, Some(principal), action, &resource).map_err(
            |error| {
                tracing::warn!(%error, "invalid bucket policy");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            },
        )? {
            crate::policy::Decision::Deny => {
                return Err(s3_error_response(
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "Access Denied",
                ));
            }
            crate::policy::Decision::Allow => return Ok(namespace),
            crate::policy::Decision::None => {}
        }
    }
    Ok(fallback.to_string())
}

fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|part| {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        (name == key).then(|| value.to_string())
    })
}

async fn resolve_read_namespace(
    request: Request,
    state: &AppState,
    bucket_name: &str,
    object_name: &str,
) -> Result<String, Response<Body>> {
    if request.headers().contains_key(AUTHORIZATION)
        || crate::signature::has_presigned_query(request.uri())
    {
        let verified = VerifiedRequest::from_request(request, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let resource = format!("arn:aws:s3:::{bucket_name}/{object_name}");
        for (namespace, policy) in state
            .metadata_store
            .bucket_policies(bucket_name)
            .await
            .map_err(|error| {
                tracing::error!(%error, "failed to resolve bucket policy");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?
        {
            match crate::policy::decision(
                &policy,
                Some(&verified.namespace),
                "s3:GetObject",
                &resource,
            )
            .map_err(|error| {
                tracing::warn!(%error, "invalid bucket policy");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })? {
                crate::policy::Decision::Deny => {
                    return Err(s3_error_response(
                        StatusCode::FORBIDDEN,
                        "AccessDenied",
                        "Access Denied",
                    ));
                }
                crate::policy::Decision::Allow => return Ok(namespace),
                crate::policy::Decision::None => {}
            }
        }
        return Ok(verified.namespace);
    }

    if crate::policy::allows_public_read_acl("s3:GetObject") {
        if let Some(namespace) = state
            .metadata_store
            .public_object_namespace(bucket_name, object_name)
            .await
            .map_err(|error| {
                tracing::error!(%error, "failed to resolve public object");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?
        {
            return Ok(namespace);
        }
    }

    if crate::policy::allows_public_read_acl("s3:GetObject") {
        if let Some(namespace) = state
            .metadata_store
            .public_bucket_namespace(bucket_name)
            .await
            .map_err(|error| {
                tracing::error!(%error, "failed to resolve public bucket");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?
        {
            return Ok(namespace);
        }
    }

    let resource = format!("arn:aws:s3:::{bucket_name}/{object_name}");
    for (namespace, policy) in state
        .metadata_store
        .bucket_policies(bucket_name)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to resolve bucket policy");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?
    {
        if crate::policy::allows_anonymous(&policy, "s3:GetObject", &resource).map_err(|error| {
            tracing::warn!(%error, "invalid bucket policy");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })? {
            return Ok(namespace);
        }
    }

    Err(s3_error_response(
        StatusCode::FORBIDDEN,
        "AccessDenied",
        "Access Denied",
    ))
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
