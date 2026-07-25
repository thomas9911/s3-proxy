use crate::signature::s3_error_response;
use crate::AppState;
use aws_sigv4::sign::v4::{calculate_signature, generate_signing_key};
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use base64::Engine;
use std::collections::HashMap;

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
