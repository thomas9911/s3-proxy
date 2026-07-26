use crate::signature::s3_error_response;
use crate::signature::VerifiedRequest;
use crate::{metadata::ObjectMetadata, templates, AppState};
use aws_sigv4::sign::v4::{calculate_signature, generate_signing_key};
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::header::HeaderName;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use base64::Engine;
use futures_util::stream::StreamExt as FuturesStreamExt;
use md5::{Digest, Md5};
use std::collections::HashMap;

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
    if is_delete {
        let content_md5 = request.headers().get(CONTENT_MD5).cloned();
        let verified = match VerifiedRequest::from_request(request, &state).await {
            Ok(verified) => verified,
            Err(error) => return Ok(error.into_response()),
        };
        return delete_objects(bucket_name, state, verified, content_md5).await;
    }

    let multipart = Multipart::from_request(request, &state).await?;
    post_object(Path(bucket_name), State(state), multipart).await
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
            Ok(keys) => deleted.extend(keys),
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
) -> Result<Vec<String>, templates::DeleteObjectError> {
    let mut deleter = opendal_operator
        .deleter()
        .await
        .map_err(|error| delete_error(String::new(), error))?;
    for object in &objects {
        let filepath = format!("{namespace}/{bucket_name}/{}", object.key);
        deleter
            .delete(filepath)
            .await
            .map_err(|error| delete_error(object.key.clone(), error))?;
    }
    deleter
        .close()
        .await
        .map_err(|error| delete_error(String::new(), error))?;

    let object_names = objects
        .iter()
        .map(|object| object.key.as_str())
        .collect::<Vec<_>>();
    metadata_store
        .delete_many_object_metadata(namespace, bucket_name, &object_names)
        .await
        .map_err(|error| delete_error(String::new(), error))?;
    Ok(objects.into_iter().map(|object| object.key).collect())
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
