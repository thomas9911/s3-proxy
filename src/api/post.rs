use crate::signature::s3_error_response;
use crate::signature::VerifiedRequest;
use crate::{
    metadata::{AccessKeyStatus, ObjectMetadata},
    templates, AppState,
};
use aws_sigv4::sign::v4::{calculate_signature, generate_signing_key};
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::header::HeaderName;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use base64::Engine;
use futures_util::stream::{StreamExt as FuturesStreamExt, TryStreamExt};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const CONTENT_MD5: HeaderName = HeaderName::from_static("content-md5");

pub async fn post_bucket(
    Path(bucket_name): Path<String>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, RouteError> {
    let is_delete = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|parameter| parameter == "delete" || parameter.starts_with("delete="));
    let is_uploads = query_value(request.uri().query(), "uploads").is_some();
    if is_delete {
        let content_md5 = request.headers().get(CONTENT_MD5).cloned();
        let verified = match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return Ok(error.into_response()),
        };
        return delete_objects(bucket_name, state, verified, content_md5).await;
    }
    if is_uploads {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "A multipart upload must specify an object key.",
        ));
    }

    let multipart = Multipart::from_request(request, &state).await?;
    post_object(Path(bucket_name), State(state), multipart).await
}

pub async fn post_object_route(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(state): State<AppState>,
    request: Request,
) -> Response {
    if query_value(request.uri().query(), "uploads").is_some() {
        let verified = match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return error.into_response(),
        };
        return initiate_multipart(bucket_name, object_name, State(state), verified)
            .await
            .map_or_else(IntoResponse::into_response, |response| response);
    }
    let Some(upload_id) = query_value(request.uri().query(), "uploadId") else {
        return s3_error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "The requested method is not supported for this resource.",
        );
    };
    let verified = match VerifiedRequest::from_request(request, &state).await {
        Ok(verified) => verified,
        Err(error) => return error.into_response(),
    };
    complete_multipart(bucket_name, object_name, state, verified, upload_id)
        .await
        .map_or_else(IntoResponse::into_response, |response| response)
}

pub async fn initiate_multipart(
    bucket_name: String,
    object_name: String,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    let bucket_path = format!("{}/{}/", signature.namespace, bucket_name);
    if !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    let upload_id = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos()
    );
    opendal_operator
        .create_dir(&format!(
            "{}/{}/",
            signature.namespace,
            multipart_prefix(&upload_id)
        ))
        .await?;
    let manifest = MultipartManifest {
        bucket: bucket_name.clone(),
        object: object_name.clone(),
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_secs(),
    };
    opendal_operator
        .write(
            &multipart_manifest_path(&signature.namespace, &upload_id),
            serde_json::to_vec(&manifest)?,
        )
        .await?;
    Ok(templates::xml_response(
        StatusCode::OK,
        templates::InitiateMultipartTemplate {
            bucket: &bucket_name,
            key: &object_name,
            upload_id: &upload_id,
        },
    ))
}

pub async fn upload_part(
    bucket_name: String,
    object_name: String,
    state: AppState,
    signature: VerifiedRequest,
    upload_id: String,
    part_number: u32,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    if !(1..=10_000).contains(&part_number) || !valid_upload_id(&upload_id) {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPart",
            "The part number or upload ID is invalid.",
        ));
    }
    let AppState {
        opendal_operator, ..
    } = state;
    let bucket_path = format!("{}/{}/", signature.namespace, bucket_name);
    if !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    if !manifest_matches(
        &opendal_operator,
        &signature.namespace,
        &upload_id,
        &bucket_name,
        &object_name,
    )
    .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let part_path = multipart_part_path(&signature.namespace, &upload_id, part_number);
    let etag = format!("{:x}", Md5::digest(&signature.bytes));
    opendal_operator.write(&part_path, signature.bytes).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("etag", format!("\"{etag}\""))
        .body(Body::empty())
        .expect("static upload part response headers are valid"))
}

pub async fn complete_multipart(
    bucket_name: String,
    object_name: String,
    state: AppState,
    signature: VerifiedRequest,
    upload_id: String,
) -> Result<Response, RouteError> {
    crate::metrics::record_storage();
    if !valid_upload_id(&upload_id) {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let request: CompleteMultipartUpload = quick_xml::de::from_str(
        std::str::from_utf8(&signature.bytes).map_err(|_| RouteError::new_internal_server())?,
    )?;
    let AppState {
        opendal_operator,
        metadata_store,
        config,
        ..
    } = state;
    let bucket_path = format!("{}/{}/", signature.namespace, bucket_name);
    if !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    if !manifest_matches(
        &opendal_operator,
        &signature.namespace,
        &upload_id,
        &bucket_name,
        &object_name,
    )
    .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let mut parts = request.parts;
    parts.sort_by_key(|part| part.part_number);
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| !(1..=10_000).contains(&part.part_number))
        || parts
            .windows(2)
            .any(|parts| parts[0].part_number == parts[1].part_number)
    {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPart",
            "The multipart completion request is invalid.",
        ));
    }
    let object_path = format!("{}/{}/{}", signature.namespace, bucket_name, object_name);
    let mut expected_length = 0u64;
    for part in &parts {
        let path = multipart_part_path(&signature.namespace, &upload_id, part.part_number);
        expected_length += match opendal_operator.stat(&path).await {
            Ok(metadata) => metadata.content_length(),
            Err(_) => {
                return Ok(s3_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "One or more parts are missing.",
                ))
            }
        };
    }
    if !crate::quota::allows_storage(
        &config.quotas,
        &opendal_operator,
        &signature.namespace,
        Some(&object_path),
        expected_length,
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
        &signature.namespace,
        &bucket_name,
        &object_name,
        &object_path,
    )
    .await?;
    let mut writer = opendal_operator.writer(&object_path).await?;
    let mut content_length = 0u64;
    for part in &parts {
        let path = multipart_part_path(&signature.namespace, &upload_id, part.part_number);
        let reader = match opendal_operator.reader(&path).await {
            Ok(reader) => reader,
            Err(_) => {
                let _ = writer.abort().await;
                return Ok(s3_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "One or more parts are missing.",
                ));
            }
        };
        let mut stream = match reader.into_stream(..).await {
            Ok(stream) => stream,
            Err(_) => {
                let _ = writer.abort().await;
                return Ok(s3_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "One or more parts are missing.",
                ));
            }
        };
        let mut digest = Md5::new();
        while let Some(buffer) = stream.try_next().await? {
            let bytes = buffer.to_vec();
            digest.update(&bytes);
            content_length += bytes.len() as u64;
            writer.write(buffer).await?;
        }
        let actual_etag = format!("{:x}", digest.finalize());
        let expected_etag = part.etag.trim_matches('"');
        if expected_etag.is_empty() || expected_etag != actual_etag {
            let _ = writer.abort().await;
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidPart",
                "The part ETag does not match the uploaded part.",
            ));
        }
    }
    writer.close().await?;
    crate::retry::retry("replace_multipart_object_metadata", || {
        metadata_store.delete_object_metadata(&signature.namespace, &bucket_name, &object_name)
    })
    .await?;
    crate::retry::retry("reset_multipart_object_public_acl", || {
        metadata_store.set_object_public(&signature.namespace, &bucket_name, &object_name, false)
    })
    .await?;
    let metadata = ObjectMetadata {
        content_length: Some(content_length),
        last_modified: Some(SystemTime::now()),
        ..Default::default()
    };
    crate::retry::retry("set_multipart_object_metadata", || {
        metadata_store.set_object_metadata(
            &signature.namespace,
            &bucket_name,
            &object_name,
            &metadata,
        )
    })
    .await?;
    let version_id = crate::versioning::record_put(
        &opendal_operator,
        &signature.namespace,
        &bucket_name,
        &object_name,
        &object_path,
    )
    .await?;
    let prefix = format!("{}/{}/", signature.namespace, multipart_prefix(&upload_id));
    opendal_operator
        .delete_with(&prefix)
        .recursive(true)
        .await?;
    let location = format!("/{bucket_name}/{object_name}");
    let mut response = templates::xml_response(
        StatusCode::OK,
        templates::CompleteMultipartTemplate {
            location: &location,
            bucket: &bucket_name,
            key: &object_name,
        },
    );
    if let Some(version_id) = version_id {
        response
            .headers_mut()
            .insert("x-amz-version-id", HeaderValue::from_str(&version_id)?);
    }
    Ok(response)
}

pub async fn abort_multipart(
    bucket_name: String,
    state: AppState,
    signature: VerifiedRequest,
    upload_id: String,
) -> Result<Response, RouteError> {
    if !valid_upload_id(&upload_id) {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let AppState {
        opendal_operator, ..
    } = state;
    let bucket_path = format!("{}/{}/", signature.namespace, bucket_name);
    if !opendal_operator.exists(&bucket_path).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }
    if !manifest_matches(
        &opendal_operator,
        &signature.namespace,
        &upload_id,
        &bucket_name,
        "",
    )
    .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let prefix = format!("{}/{}/", signature.namespace, multipart_prefix(&upload_id));
    opendal_operator
        .delete_with(&prefix)
        .recursive(true)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn list_parts(
    bucket_name: String,
    object_name: String,
    state: AppState,
    signature: VerifiedRequest,
    upload_id: String,
) -> Result<Response, RouteError> {
    if !valid_upload_id(&upload_id) {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let AppState {
        opendal_operator, ..
    } = state;
    if !manifest_matches(
        &opendal_operator,
        &signature.namespace,
        &upload_id,
        &bucket_name,
        &object_name,
    )
    .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified upload does not exist.",
        ));
    }
    let prefix = format!("{}/{}/", signature.namespace, multipart_prefix(&upload_id));
    let mut lister = opendal_operator.lister_with(&prefix).await?;
    let mut parts = Vec::new();
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if entry.metadata().is_file() {
            if let Ok(part_number) = entry.name().parse::<u32>() {
                parts.push((part_number, entry.metadata().content_length()));
            }
        }
    }
    parts.sort_by_key(|(part_number, _)| *part_number);
    let parts = parts
        .into_iter()
        .map(|(part_number, size)| templates::ListPartItem { part_number, size })
        .collect::<Vec<_>>();
    Ok(templates::xml_response(
        StatusCode::OK,
        templates::ListPartsTemplate {
            bucket: &bucket_name,
            key: &object_name,
            upload_id: &upload_id,
            parts: &parts,
        },
    ))
}

#[derive(Debug, Deserialize)]
struct CompleteMultipartUpload {
    #[serde(rename = "Part", default)]
    parts: Vec<CompletedPart>,
}

#[derive(Debug, Deserialize)]
struct CompletedPart {
    #[serde(rename = "PartNumber")]
    part_number: u32,
    #[serde(rename = "ETag", default)]
    etag: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct MultipartManifest {
    bucket: String,
    object: String,
    #[serde(default)]
    created_at: u64,
}

fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|part| {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        (name == key).then(|| value.to_string())
    })
}

fn valid_upload_id(upload_id: &str) -> bool {
    !upload_id.is_empty() && upload_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn multipart_prefix(upload_id: &str) -> String {
    format!(".multipart/{upload_id}")
}

fn multipart_part_path(namespace: &str, upload_id: &str, part_number: u32) -> String {
    format!("{namespace}/{}/{part_number}", multipart_prefix(upload_id))
}

fn multipart_manifest_path(namespace: &str, upload_id: &str) -> String {
    format!("{namespace}/{}/manifest.json", multipart_prefix(upload_id))
}

async fn manifest_matches(
    operator: &opendal::Operator,
    namespace: &str,
    upload_id: &str,
    bucket: &str,
    object: &str,
) -> anyhow::Result<bool> {
    let path = multipart_manifest_path(namespace, upload_id);
    if !operator.exists(&path).await? {
        return Ok(false);
    }
    let manifest: MultipartManifest =
        serde_json::from_slice(&operator.read(&path).await?.to_vec())?;
    Ok(manifest.bucket == bucket && (object.is_empty() || manifest.object == object))
}

pub async fn post_object(
    Path(bucket_name): Path<String>,
    State(AppState {
        metadata_store,
        opendal_operator,
        config,
        ..
    }): State<AppState>,
    mut multipart: Multipart,
) -> Result<Response, RouteError> {
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

    let access_key_record = metadata_store.access_key(access_key).await?;
    let Some(access_key_record) = access_key_record else {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied",
        ));
    };
    if access_key_record.status != AccessKeyStatus::Active {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied",
        ));
    }
    let namespace = access_key_record.principal_id;
    if !crate::quota::allow_request(&config.quotas, &namespace) {
        return Ok(s3_error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "SlowDown",
            "The request quota for this principal has been exceeded.",
        ));
    }
    if !opendal_operator
        .exists(&format!("{namespace}/{bucket_name}/"))
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
        generate_signing_key(&access_key_record.secret_key, signing_time, region, service),
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
    let Some(expiration) = policy.get("expiration").and_then(serde_json::Value::as_str) else {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPolicyDocument",
            "The policy expiration is missing.",
        ));
    };
    let expiration = match OffsetDateTime::parse(expiration, &Rfc3339) {
        Ok(expiration) => expiration,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidPolicyDocument",
                "The policy expiration is invalid.",
            ))
        }
    };
    if expiration <= OffsetDateTime::now_utc() {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "The presigned POST policy has expired.",
        ));
    }
    let key_template = fields.get("key").cloned().unwrap_or_default();
    let key = key_template.replace("${filename}", filename.as_deref().unwrap_or_default());
    let mut validation_fields = fields.clone();
    validation_fields.insert("key".to_string(), key.clone());
    if !policy_allows(
        &policy,
        &validation_fields,
        &bucket_name,
        file.as_ref().map(|body| body.len()).unwrap_or(0),
    ) {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPolicyDocument",
            "Policy conditions failed",
        ));
    }

    let Some(file) = file else {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "MalformedPOSTRequest",
            "Missing file",
        ));
    };
    let content_length = file.len() as u64;
    let content_type = fields.get("content-type").cloned();
    metadata_store.record_access_key_use(access_key).await?;
    let filepath = format!("{namespace}/{bucket_name}/{key}");
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
        &key,
        &filepath,
    )
    .await?;
    let mut writer = opendal_operator.write_with(&filepath, file);
    if let Some(content_type) = content_type.as_deref() {
        writer = writer.content_type(content_type);
    }
    writer.await?;
    crate::retry::retry("replace_presigned_post_metadata", || {
        metadata_store.delete_object_metadata(&namespace, &bucket_name, &key)
    })
    .await?;
    crate::retry::retry("reset_presigned_post_public_acl", || {
        metadata_store.set_object_public(&namespace, &bucket_name, &key, false)
    })
    .await?;
    let user_metadata = fields
        .iter()
        .filter_map(|(name, value)| {
            name.strip_prefix("x-amz-meta-")
                .map(|key| (key.to_string(), value.to_string()))
        })
        .collect();
    metadata_store
        .set_object_metadata(
            &namespace,
            &bucket_name,
            &key,
            &ObjectMetadata {
                content_type,
                content_length: Some(content_length),
                last_modified: Some(SystemTime::now()),
                user_metadata,
                ..Default::default()
            },
        )
        .await?;
    let version_id =
        crate::versioning::record_put(&opendal_operator, &namespace, &bucket_name, &key, &filepath)
            .await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Some(version_id) = version_id {
        response
            .headers_mut()
            .insert("x-amz-version-id", HeaderValue::from_str(&version_id)?);
    }
    Ok(response)
}

async fn delete_objects(
    bucket_name: String,
    AppState {
        metadata_store,
        opendal_operator,
        ..
    }: AppState,
    signature: VerifiedRequest,
    content_md5: Option<HeaderValue>,
) -> Result<Response, RouteError> {
    let expected_md5 = content_md5.and_then(|value| value.to_str().ok().map(ToOwned::to_owned));
    let actual_md5 =
        base64::engine::general_purpose::STANDARD.encode(Md5::digest(&signature.bytes));
    if expected_md5
        .as_deref()
        .is_some_and(|expected| expected != actual_md5)
    {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "BadDigest",
            "The Content-MD5 you specified did not match what we received.",
        ));
    }

    let namespace = signature.namespace;
    if !opendal_operator
        .exists(&format!("{namespace}/{bucket_name}/"))
        .await?
    {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let request: templates::DeleteObjectsRequest =
        match quick_xml::de::from_reader(signature.bytes.as_ref()) {
            Ok(request) => request,
            Err(_) => {
                return Ok(s3_error_response(
                    StatusCode::BAD_REQUEST,
                    "MalformedXML",
                    "The XML you provided was not well-formed or did not validate against our published schema.",
                ))
            }
        };
    if request.objects.is_empty() || request.objects.len() > 1000 {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "MalformedXML",
            "You must specify between 1 and 1000 objects.",
        ));
    }

    let quiet = request.quiet;
    let object_batches = request
        .objects
        .chunks(20)
        .map(|batch| batch.to_vec())
        .collect::<Vec<_>>();
    let results = tokio_stream::iter(object_batches.into_iter().map(|objects| {
        let metadata_store = metadata_store.clone();
        let opendal_operator = opendal_operator.clone();
        let namespace = namespace.clone();
        let bucket_name = bucket_name.clone();
        async move {
            delete_batch(
                &opendal_operator,
                &metadata_store,
                &namespace,
                &bucket_name,
                objects,
            )
            .await
        }
    }))
    .buffered(20)
    .collect::<Vec<_>>()
    .await;

    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok((keys, batch_errors)) => {
                deleted.extend(keys);
                errors.extend(batch_errors);
            }
            Err(error) => errors.push(error),
        }
    }
    let template = templates::DeleteObjectsTemplate {
        deleted: if quiet { &[] } else { &deleted },
        errors: &errors,
    };
    Ok(template.into_response())
}

async fn delete_batch(
    opendal_operator: &opendal::Operator,
    metadata_store: &std::sync::Arc<dyn crate::metadata::MetadataStore>,
    namespace: &str,
    bucket_name: &str,
    objects: Vec<templates::DeleteObjectIdentifier>,
) -> Result<(Vec<String>, Vec<templates::DeleteObjectError>), templates::DeleteObjectError> {
    let results = tokio_stream::iter(objects.into_iter().map(|object| {
        let opendal_operator = opendal_operator.clone();
        let namespace = namespace.to_string();
        let bucket_name = bucket_name.to_string();
        async move {
            let filepath = format!("{namespace}/{bucket_name}/{}", object.key);
            let mut deleter = opendal_operator
                .deleter()
                .await
                .map_err(|error| delete_error(object.key.clone(), error))?;
            deleter
                .delete(filepath)
                .await
                .map_err(|error| delete_error(object.key.clone(), error))?;
            deleter
                .close()
                .await
                .map_err(|error| delete_error(object.key.clone(), error))?;
            Ok::<_, templates::DeleteObjectError>(object.key)
        }
    }))
    .buffered(20)
    .collect::<Vec<_>>()
    .await;
    let mut object_names = Vec::new();
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(object) => {
                deleted.push(object.clone());
                object_names.push(object);
            }
            Err(error) => errors.push(error),
        }
    }
    let object_names = object_names.iter().map(String::as_str).collect::<Vec<_>>();
    crate::retry::retry("delete_many_object_metadata", || {
        metadata_store.delete_many_object_metadata(namespace, bucket_name, &object_names)
    })
    .await
    .map_err(|error| delete_error(String::new(), error))?;
    for object in &object_names {
        crate::versioning::record_delete_marker(opendal_operator, namespace, bucket_name, object)
            .await
            .map_err(|error| delete_error((*object).to_string(), error))?;
        crate::retry::retry("delete_object_public_acl", || {
            metadata_store.set_object_public(namespace, bucket_name, object, false)
        })
        .await
        .map_err(|error| delete_error((*object).to_string(), error))?;
    }
    Ok((deleted, errors))
}

fn delete_error(key: String, error: impl std::fmt::Display) -> templates::DeleteObjectError {
    templates::DeleteObjectError {
        key,
        code: "InternalError".to_string(),
        message: error.to_string(),
    }
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
                let Some(expected) = value.as_str() else {
                    return false;
                };
                if field == "bucket" {
                    if expected != bucket {
                        return false;
                    }
                } else if fields.get(&field).map(String::as_str) != Some(expected) {
                    return false;
                }
            }
        } else if let Some(array) = condition.as_array() {
            let Some(operator) = array.first().and_then(serde_json::Value::as_str) else {
                return false;
            };
            match operator {
                "content-length-range" => {
                    let Some(min) = array.get(1).and_then(serde_json::Value::as_u64) else {
                        return false;
                    };
                    let Some(max) = array.get(2).and_then(serde_json::Value::as_u64) else {
                        return false;
                    };
                    if size < min as usize || size > max as usize {
                        return false;
                    }
                }
                "starts-with" => {
                    let Some(field) = array.get(1).and_then(serde_json::Value::as_str) else {
                        return false;
                    };
                    let Some(prefix) = array.get(2).and_then(serde_json::Value::as_str) else {
                        return false;
                    };
                    let field = field.trim_start_matches('$').to_ascii_lowercase();
                    let Some(actual) = fields.get(&field) else {
                        return false;
                    };
                    if !actual.starts_with(prefix) {
                        return false;
                    }
                }
                "eq" => {
                    let Some(field) = array.get(1).and_then(serde_json::Value::as_str) else {
                        return false;
                    };
                    let Some(expected) = array.get(2).and_then(serde_json::Value::as_str) else {
                        return false;
                    };
                    let field = field.trim_start_matches('$').to_ascii_lowercase();
                    if fields.get(&field).map(String::as_str) != Some(expected) {
                        return false;
                    }
                }
                _ => return false,
            }
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{
        abort_multipart, calculate_signature, complete_multipart, delete_batch,
        generate_signing_key, list_parts, multipart_manifest_path, multipart_prefix, policy_allows,
        post_object, upload_part, MultipartManifest,
    };
    use crate::metadata::{MetadataStore, ObjectMetadata, SqliteMetadataStore};
    use crate::{AppState, Config, SqliteConfig};
    use axum::body::Body;
    use axum::body::Bytes;
    use axum::extract::{FromRequest, State};
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use base64::Engine;
    use md5::{Digest, Md5};
    use opendal::services::Memory;
    use opendal::Operator;
    use std::collections::HashMap;
    use std::sync::Arc;
    use time::{Duration, OffsetDateTime};

    fn fields(key: &str) -> HashMap<String, String> {
        HashMap::from([(String::from("key"), key.to_string())])
    }

    #[test]
    fn enforces_starts_with_conditions() {
        let policy = serde_json::json!({
            "conditions": [["starts-with", "$key", "uploads/"]]
        });
        assert!(policy_allows(
            &policy,
            &fields("uploads/file.txt"),
            "bucket",
            1
        ));
        assert!(!policy_allows(
            &policy,
            &fields("private/file.txt"),
            "bucket",
            1
        ));
    }

    #[test]
    fn enforces_eq_conditions() {
        let policy = serde_json::json!({
            "conditions": [["eq", "$key", "uploads/file.txt"]]
        });
        assert!(policy_allows(
            &policy,
            &fields("uploads/file.txt"),
            "bucket",
            1
        ));
        assert!(!policy_allows(
            &policy,
            &fields("uploads/other.txt"),
            "bucket",
            1
        ));
    }

    #[test]
    fn rejects_unknown_array_conditions() {
        let policy = serde_json::json!({
            "conditions": [["unknown", "$key", "uploads/"]]
        });
        assert!(!policy_allows(
            &policy,
            &fields("uploads/file.txt"),
            "bucket",
            1
        ));
    }

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
            management: None,
            quotas: crate::quota::QuotaConfig::default(),
            opendal_provider: "memory".to_string(),
            opendal: HashMap::new(),
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

    #[tokio::test]
    async fn multipart_handlers_store_and_complete_parts() {
        let (state, metadata_store, operator) = test_state().await;
        operator.create_dir("namespace/bucket/").await.unwrap();
        metadata_store
            .set_object_metadata(
                "namespace",
                "bucket",
                "object.txt",
                &ObjectMetadata {
                    user_metadata: HashMap::from([(String::from("stale"), String::from("yes"))]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        metadata_store
            .set_object_public("namespace", "bucket", "object.txt", true)
            .await
            .unwrap();

        let upload_id = "abc123";
        operator
            .create_dir(&format!("namespace/{}/", multipart_prefix(upload_id)))
            .await
            .unwrap();
        operator
            .write(
                &multipart_manifest_path("namespace", upload_id),
                serde_json::to_vec(&MultipartManifest {
                    bucket: "bucket".to_string(),
                    object: "object.txt".to_string(),
                    created_at: 0,
                })
                .unwrap(),
            )
            .await
            .unwrap();

        let part = Bytes::from_static(b"multipart body");
        let etag = format!("{:x}", Md5::digest(&part));
        let upload_response = upload_part(
            "bucket".to_string(),
            "object.txt".to_string(),
            state.clone(),
            crate::signature::VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: part,
            },
            upload_id.to_string(),
            1,
        )
        .await
        .unwrap();
        assert_eq!(upload_response.status(), StatusCode::OK);

        let list_response = list_parts(
            "bucket".to_string(),
            "object.txt".to_string(),
            state.clone(),
            crate::signature::VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: Bytes::new(),
            },
            upload_id.to_string(),
        )
        .await
        .unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);

        let complete_response = complete_multipart(
            "bucket".to_string(),
            "object.txt".to_string(),
            state,
            crate::signature::VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: Bytes::from(format!(
                    "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"{etag}\"</ETag></Part></CompleteMultipartUpload>"
                )),
            },
            upload_id.to_string(),
        )
        .await
        .unwrap();
        assert_eq!(complete_response.status(), StatusCode::OK);

        let stored = metadata_store
            .object_metadata("namespace", "bucket", "object.txt")
            .await
            .unwrap();
        assert!(stored.user_metadata.is_empty());
        assert_eq!(stored.content_length, Some(14));
        assert_eq!(
            metadata_store
                .public_object_namespace("bucket", "object.txt")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            operator
                .read("namespace/bucket/object.txt")
                .await
                .unwrap()
                .to_vec(),
            b"multipart body"
        );

        operator
            .write("namespace/bucket/delete-me.txt", b"delete me".to_vec())
            .await
            .unwrap();
        let metadata_trait: Arc<dyn MetadataStore> = metadata_store.clone();
        let (deleted, errors) = delete_batch(
            &operator,
            &metadata_trait,
            "namespace",
            "bucket",
            vec![crate::templates::DeleteObjectIdentifier {
                key: "delete-me.txt".to_string(),
            }],
        )
        .await
        .unwrap();
        assert_eq!(deleted, vec!["delete-me.txt"]);
        assert!(errors.is_empty());

        let (abort_state, _, abort_operator) = test_state().await;
        abort_operator
            .create_dir("namespace/bucket/")
            .await
            .unwrap();
        let abort_upload_id = "def456";
        abort_operator
            .create_dir(&format!("namespace/{}/", multipart_prefix(abort_upload_id)))
            .await
            .unwrap();
        abort_operator
            .write(
                &multipart_manifest_path("namespace", abort_upload_id),
                serde_json::to_vec(&MultipartManifest {
                    bucket: "bucket".to_string(),
                    object: "aborted.txt".to_string(),
                    created_at: 0,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let abort_response = abort_multipart(
            "bucket".to_string(),
            abort_state,
            crate::signature::VerifiedRequest {
                access_key: "access".to_string(),
                namespace: "namespace".to_string(),
                bytes: Bytes::new(),
            },
            abort_upload_id.to_string(),
        )
        .await
        .unwrap();
        assert_eq!(abort_response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn presigned_post_handler_accepts_a_signed_upload() {
        let (state, metadata_store, operator) = test_state().await;
        let access_key = "access";
        let secret_key = "secret";
        metadata_store
            .set_secret_key(access_key, secret_key)
            .await
            .unwrap();
        operator.create_dir("access/bucket/").await.unwrap();

        let now = OffsetDateTime::now_utc();
        let date_format = time::macros::format_description!("[year][month][day]");
        let timestamp_format =
            time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
        let date = now.format(date_format).unwrap();
        let timestamp = now.format(timestamp_format).unwrap();
        let credential = format!("{access_key}/{date}/us-east-1/s3/aws4_request");
        let policy = serde_json::json!({
            "expiration": (now + Duration::hours(1)).format(&time::format_description::well_known::Rfc3339).unwrap(),
            "conditions": [
                { "bucket": "bucket" },
                ["starts-with", "$key", "uploads/"],
                ["content-length-range", 1, 100]
            ]
        });
        let policy =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&policy).unwrap());
        let signing_time = crate::signature::parse_date_time(&timestamp).unwrap();
        let signature = calculate_signature(
            generate_signing_key(secret_key, signing_time, "us-east-1", "s3"),
            policy.as_bytes(),
        );
        let fields = [
            ("key", "uploads/${filename}"),
            ("x-amz-credential", credential.as_str()),
            ("x-amz-date", timestamp.as_str()),
            ("policy", policy.as_str()),
            ("x-amz-signature", signature.as_str()),
        ];
        let boundary = "regression-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"posted.txt\"\r\nContent-Type: text/plain\r\n\r\nposted body\r\n--{boundary}--\r\n"
            )
            .as_bytes(),
        );
        let request = Request::builder()
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let multipart = axum::extract::Multipart::from_request(request, &state)
            .await
            .unwrap();
        let response = post_object(
            axum::extract::Path("bucket".to_string()),
            State(state),
            multipart,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            operator
                .read("access/bucket/uploads/posted.txt")
                .await
                .unwrap()
                .to_vec(),
            b"posted body"
        );
    }

    async fn submit_post_form(
        state: &AppState,
        fields: HashMap<String, String>,
        include_file: bool,
    ) -> Response {
        let boundary = "failure-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        if include_file {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"posted.txt\"\r\n\r\nposted body\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let request = Request::builder()
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let multipart = axum::extract::Multipart::from_request(request, state)
            .await
            .unwrap();
        post_object(
            axum::extract::Path("bucket".to_string()),
            State(state.clone()),
            multipart,
        )
        .await
        .unwrap()
    }

    fn signed_post_fields(policy: &str, signature: Option<&str>) -> HashMap<String, String> {
        let now = OffsetDateTime::now_utc();
        let date_format = time::macros::format_description!("[year][month][day]");
        let timestamp_format =
            time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
        let date = now.format(date_format).unwrap();
        let timestamp = now.format(timestamp_format).unwrap();
        let credential = format!("access/{date}/us-east-1/s3/aws4_request");
        let encoded_policy = base64::engine::general_purpose::STANDARD.encode(policy.as_bytes());
        let signing_time = crate::signature::parse_date_time(&timestamp).unwrap();
        let calculated_signature = calculate_signature(
            generate_signing_key("secret", signing_time, "us-east-1", "s3"),
            encoded_policy.as_bytes(),
        );
        HashMap::from([
            ("key".to_string(), "uploads/${filename}".to_string()),
            ("x-amz-credential".to_string(), credential),
            ("x-amz-date".to_string(), timestamp),
            ("policy".to_string(), encoded_policy),
            (
                "x-amz-signature".to_string(),
                signature.unwrap_or(&calculated_signature).to_string(),
            ),
        ])
    }

    #[tokio::test]
    async fn presigned_post_handler_rejects_invalid_requests() {
        let (state, metadata_store, operator) = test_state().await;
        metadata_store
            .set_secret_key("access", "secret")
            .await
            .unwrap();
        operator.create_dir("access/bucket/").await.unwrap();

        assert_eq!(
            submit_post_form(&state, HashMap::new(), false)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut invalid_service = signed_post_fields("not json", None);
        invalid_service.insert(
            "x-amz-credential".to_string(),
            "access/20260727/us-east-1/ec2/aws4_request".to_string(),
        );
        assert_eq!(
            submit_post_form(&state, invalid_service, false)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );

        let mut missing_policy = signed_post_fields("not json", None);
        missing_policy.remove("policy");
        assert_eq!(
            submit_post_form(&state, missing_policy, false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut missing_signature = signed_post_fields("not json", None);
        missing_signature.remove("x-amz-signature");
        assert_eq!(
            submit_post_form(&state, missing_signature, false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut invalid_date = signed_post_fields("not json", None);
        invalid_date.insert("x-amz-date".to_string(), "invalid".to_string());
        assert_eq!(
            submit_post_form(&state, invalid_date, false).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            submit_post_form(&state, signed_post_fields("not json", Some("wrong")), false,)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            submit_post_form(&state, signed_post_fields("!!!", None), false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            submit_post_form(&state, signed_post_fields("not json", None), false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );

        let missing_expiration = r#"{"conditions":[]}"#;
        assert_eq!(
            submit_post_form(&state, signed_post_fields(missing_expiration, None), false,)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let invalid_expiration = r#"{"expiration":"invalid","conditions":[]}"#;
        assert_eq!(
            submit_post_form(&state, signed_post_fields(invalid_expiration, None), false,)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let expired = r#"{"expiration":"2020-01-01T00:00:00Z","conditions":[]}"#;
        assert_eq!(
            submit_post_form(&state, signed_post_fields(expired, None), false)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let failed_conditions = r#"{"expiration":"2099-01-01T00:00:00Z","conditions":[["starts-with","$key","private/"]]}"#;
        assert_eq!(
            submit_post_form(&state, signed_post_fields(failed_conditions, None), true,)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let no_file = r#"{"expiration":"2099-01-01T00:00:00Z","conditions":[]}"#;
        assert_eq!(
            submit_post_form(&state, signed_post_fields(no_file, None), false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
