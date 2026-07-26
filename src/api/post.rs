use crate::signature::s3_error_response;
use crate::signature::VerifiedRequest;
use crate::{metadata::ObjectMetadata, templates, AppState};
use aws_sigv4::sign::v4::{calculate_signature, generate_signing_key};
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::header::HeaderName;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use base64::Engine;
use futures_util::stream::StreamExt as FuturesStreamExt;
use md5::{Digest, Md5};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

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
        let verified = match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return Ok(error.into_response()),
        };
        return initiate_multipart(bucket_name, None, State(state), verified).await;
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
        return initiate_multipart(bucket_name, Some(object_name), State(state), verified)
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
    object_name: Option<String>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
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
    let object_name = object_name.unwrap_or_default();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/xml")
        .body(Body::from(format!(
            "<InitiateMultipartUploadResult><Bucket>{bucket_name}</Bucket><Key>{object_name}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
        )))
        .expect("static multipart response headers are valid"))
}

pub async fn upload_part(
    bucket_name: String,
    _object_name: String,
    state: AppState,
    signature: VerifiedRequest,
    upload_id: String,
    part_number: u32,
) -> Result<Response, RouteError> {
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
    let part_path = multipart_part_path(&signature.namespace, &upload_id, part_number);
    opendal_operator.write(&part_path, signature.bytes).await?;
    Ok(StatusCode::OK.into_response())
}

pub async fn complete_multipart(
    bucket_name: String,
    object_name: String,
    state: AppState,
    signature: VerifiedRequest,
    upload_id: String,
) -> Result<Response, RouteError> {
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
    let mut parts = request.parts;
    parts.sort_by_key(|part| part.part_number);
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| !(1..=10_000).contains(&part.part_number))
    {
        return Ok(s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPart",
            "The multipart completion request is invalid.",
        ));
    }
    let mut content = Vec::new();
    for part in &parts {
        let path = multipart_part_path(&signature.namespace, &upload_id, part.part_number);
        let bytes = match opendal_operator.read(&path).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return Ok(s3_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "One or more parts are missing.",
                ))
            }
        };
        content.extend_from_slice(&bytes.to_vec());
    }
    let object_path = format!("{}/{}/{}", signature.namespace, bucket_name, object_name);
    opendal_operator
        .write(&object_path, content.clone())
        .await?;
    let metadata = ObjectMetadata {
        content_length: Some(content.len() as u64),
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
    let prefix = format!("{}/{}/", signature.namespace, multipart_prefix(&upload_id));
    opendal_operator
        .delete_with(&prefix)
        .recursive(true)
        .await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/xml")
        .body(Body::from(format!(
            "<CompleteMultipartUploadResult><Location>/{bucket_name}/{object_name}</Location><Bucket>{bucket_name}</Bucket><Key>{object_name}</Key><ETag></ETag></CompleteMultipartUploadResult>"
        )))
        .expect("static multipart response headers are valid"))
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
    let entries = parts
        .into_iter()
        .map(|(part_number, size)| {
            format!(
                "<Part><PartNumber>{part_number}</PartNumber><LastModified>1970-01-01T00:00:00Z</LastModified><ETag></ETag><Size>{size}</Size></Part>"
            )
        })
        .collect::<String>();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/xml")
        .body(Body::from(format!(
            "<ListPartsResult><Bucket>{bucket_name}</Bucket><Key>{object_name}</Key><UploadId>{upload_id}</UploadId>{entries}</ListPartsResult>"
        )))
        .expect("static multipart response headers are valid"))
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
    _etag: String,
}

fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|part| {
        let (name, value) = part.split_once('=')?;
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

pub async fn post_object(
    Path(bucket_name): Path<String>,
    State(AppState {
        metadata_store,
        opendal_operator,
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

    let secret_key = metadata_store.secret_key(access_key).await?;
    let Some(secret_key) = secret_key else {
        return Ok(s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied",
        ));
    };
    if !opendal_operator
        .exists(&format!("{access_key}/{bucket_name}/"))
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
    let content_length = file.len() as u64;
    let content_type = fields.get("content-type").cloned();
    let filepath = format!("{access_key}/{bucket_name}/{key}");
    let mut writer = opendal_operator.write_with(&filepath, file);
    if let Some(content_type) = content_type.as_deref() {
        writer = writer.content_type(content_type);
    }
    writer.await?;
    let user_metadata = fields
        .iter()
        .filter_map(|(name, value)| {
            name.strip_prefix("x-amz-meta-")
                .map(|key| (key.to_string(), value.to_string()))
        })
        .collect();
    metadata_store
        .set_object_metadata(
            access_key,
            &bucket_name,
            &key,
            &ObjectMetadata {
                content_type,
                content_length: Some(content_length),
                user_metadata,
                ..Default::default()
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
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
