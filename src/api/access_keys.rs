use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{metadata::AccessKeyStatus, templates, AppState};
use axum::extract::{FromRequest, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use rand::random;
use std::collections::HashMap;

const IAM_VERSION: &str = "2010-05-08";

pub async fn create_access_key(State(state): State<AppState>, request: Request) -> Response {
    let signature = match VerifiedRequest::from_request(request, &state).await {
        Ok(signature) => signature,
        Err(error) => return error.into_response(),
    };
    let Some(admin) = state.config.admin.as_ref() else {
        return s3_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "The administrative API is not configured.",
        );
    };
    if signature.access_key != admin.access_key {
        return s3_error_response(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
    }

    let parameters = parse_form(&signature.bytes);
    if parameters.get("Version").map(String::as_str) != Some(IAM_VERSION) {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidParameterValue",
            "The Version parameter is invalid.",
        );
    }

    match parameters.get("Action").map(String::as_str) {
        Some("CreateAccessKey") => {
            create_access_key_response(&state, &signature.namespace, &parameters).await
        }
        Some("DeleteAccessKey") => delete_access_key(&state, &parameters).await,
        Some("ListAccessKeys") => list_access_keys(&state, &signature.namespace, &parameters).await,
        Some("UpdateAccessKey") => update_access_key(&state, &parameters).await,
        Some("GetAccessKeyLastUsed") => get_access_key_last_used(&state, &parameters).await,
        _ => s3_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidAction",
            "The action is not supported.",
        ),
    }
}

async fn list_access_keys(
    state: &AppState,
    caller_principal: &str,
    parameters: &HashMap<String, String>,
) -> Response {
    let principal_id = parameters
        .get("UserName")
        .map(String::as_str)
        .unwrap_or(caller_principal);
    if !valid_user_name(principal_id) {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The UserName parameter is invalid.",
        );
    }
    let owner = match state.metadata_store.namespace_owner(principal_id).await {
        Ok(owner) => owner,
        Err(error) => {
            tracing::error!(%error, "failed to look up principal");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let access_keys = match state.metadata_store.list_access_keys(principal_id).await {
        Ok(access_keys) => access_keys,
        Err(error) => {
            tracing::error!(%error, "failed to list access keys");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let items = access_keys
        .iter()
        .map(|access_key| templates::AccessKeyListItem {
            id: &access_key.id,
            status: access_key.status.as_str(),
            created_at: access_key.created_at.as_deref(),
        })
        .collect::<Vec<_>>();
    templates::xml_response(
        StatusCode::OK,
        templates::ListAccessKeysTemplate {
            user_name: &owner.display_name,
            access_keys: &items,
        },
    )
}

async fn update_access_key(state: &AppState, parameters: &HashMap<String, String>) -> Response {
    let Some(access_key) = parameters.get("AccessKeyId") else {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The AccessKeyId parameter is required.",
        );
    };
    let Some(status) = parameters
        .get("Status")
        .and_then(|status| match status.as_str() {
            "Active" => Some(AccessKeyStatus::Active),
            "Inactive" => Some(AccessKeyStatus::Inactive),
            _ => None,
        })
    else {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The Status parameter must be Active or Inactive.",
        );
    };
    let Some(record) = access_key_for_request(state, access_key, parameters).await else {
        return no_such_access_key();
    };
    match state
        .metadata_store
        .set_access_key_status(&record.id, status)
        .await
    {
        Ok(true) => templates::xml_response(StatusCode::OK, templates::UpdateAccessKeyTemplate),
        Ok(false) => no_such_access_key(),
        Err(error) => {
            tracing::error!(%error, "failed to update access key");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_access_key_last_used(
    state: &AppState,
    parameters: &HashMap<String, String>,
) -> Response {
    let Some(access_key) = parameters.get("AccessKeyId") else {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The AccessKeyId parameter is required.",
        );
    };
    let Some(record) = access_key_for_request(state, access_key, parameters).await else {
        return no_such_access_key();
    };
    let owner = match state
        .metadata_store
        .namespace_owner(&record.principal_id)
        .await
    {
        Ok(owner) => owner,
        Err(error) => {
            tracing::error!(%error, "failed to look up access key owner");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    templates::xml_response(
        StatusCode::OK,
        templates::GetAccessKeyLastUsedTemplate {
            user_name: &owner.display_name,
            access_key: &record.id,
            last_used_at: record.last_used_at.as_deref(),
        },
    )
}

async fn access_key_for_request(
    state: &AppState,
    access_key: &str,
    parameters: &HashMap<String, String>,
) -> Option<crate::metadata::AccessKey> {
    if !valid_access_key_id(access_key) {
        return None;
    }
    let record = match state.metadata_store.access_key(access_key).await {
        Ok(record) => record?,
        Err(error) => {
            tracing::error!(%error, "failed to look up access key");
            return None;
        }
    };
    if let Some(user_name) = parameters.get("UserName") {
        if !valid_user_name(user_name) {
            return None;
        }
        let owner = state
            .metadata_store
            .namespace_owner(&record.principal_id)
            .await
            .ok()?;
        if owner.display_name != *user_name {
            return None;
        }
    }
    Some(record)
}

fn no_such_access_key() -> Response {
    s3_error_response(
        StatusCode::NOT_FOUND,
        "NoSuchEntity",
        "The access key does not exist.",
    )
}

async fn create_access_key_response(
    state: &AppState,
    caller: &str,
    parameters: &HashMap<String, String>,
) -> Response {
    let user_name = parameters
        .get("UserName")
        .map(String::as_str)
        .unwrap_or(caller);
    if !valid_user_name(user_name) {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The UserName parameter is invalid.",
        );
    }

    let (access_key, secret_key) = match create_unique_access_key(state, user_name).await {
        Ok(credentials) => credentials,
        Err(error) => {
            tracing::error!(%error, "failed to create access key");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let create_date = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting is infallible");

    templates::xml_response(
        StatusCode::OK,
        templates::CreateAccessKeyTemplate {
            access_key: &access_key,
            secret_key: &secret_key,
            user_name,
            create_date: &create_date,
        },
    )
}

async fn delete_access_key(state: &AppState, parameters: &HashMap<String, String>) -> Response {
    let Some(access_key) = parameters.get("AccessKeyId") else {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The AccessKeyId parameter is required.",
        );
    };
    if !valid_access_key_id(access_key) {
        return s3_error_response(
            StatusCode::BAD_REQUEST,
            "ValidationError",
            "The AccessKeyId parameter is invalid.",
        );
    }
    let record = match state.metadata_store.access_key(access_key).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchEntity",
                "The access key does not exist.",
            )
        }
        Err(error) => {
            tracing::error!(%error, "failed to look up access key");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if let Some(user_name) = parameters.get("UserName") {
        if !valid_user_name(user_name) {
            return s3_error_response(
                StatusCode::BAD_REQUEST,
                "ValidationError",
                "The UserName parameter is invalid.",
            );
        }
        let owner = match state
            .metadata_store
            .namespace_owner(&record.principal_id)
            .await
        {
            Ok(owner) => owner,
            Err(error) => {
                tracing::error!(%error, "failed to look up access key owner");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        if owner.display_name != *user_name {
            return s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchEntity",
                "The access key does not belong to the specified user.",
            );
        }
    }
    match state.metadata_store.delete_access_key(access_key).await {
        Ok(true) => templates::xml_response(StatusCode::OK, templates::DeleteAccessKeyTemplate),
        Ok(false) => s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchEntity",
            "The access key does not exist.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to delete access key");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn create_unique_access_key(
    state: &AppState,
    user_name: &str,
) -> anyhow::Result<(String, String)> {
    for _ in 0..5 {
        let access_key = format!("AKIA{}", hex::encode(random::<[u8; 8]>()));
        let secret_key =
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(random::<[u8; 30]>());
        if state
            .metadata_store
            .create_access_key(&access_key, &secret_key, user_name)
            .await?
        {
            state
                .metadata_store
                .set_namespace_owner(&access_key, user_name, user_name)
                .await?;
            return Ok((access_key, secret_key));
        }
    }
    anyhow::bail!("could not generate a unique access key")
}

fn valid_user_name(user_name: &str) -> bool {
    !user_name.is_empty()
        && user_name.len() <= 128
        && user_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+=,.@-".contains(&byte))
}

fn valid_access_key_id(access_key: &str) -> bool {
    (16..=128).contains(&access_key.len())
        && access_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_form(bytes: &[u8]) -> std::collections::HashMap<String, String> {
    String::from_utf8_lossy(bytes)
        .split('&')
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((
                urlencoding::decode(key).ok()?.into_owned(),
                urlencoding::decode(value.replace('+', " ").as_str())
                    .ok()?
                    .into_owned(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_form, valid_access_key_id, valid_user_name};

    #[test]
    fn validates_iam_user_names() {
        assert!(valid_user_name("user.name+test@example"));
        assert!(!valid_user_name(""));
        assert!(!valid_user_name("has spaces"));
        assert!(!valid_user_name(&"a".repeat(129)));
    }

    #[test]
    fn decodes_query_api_form_parameters() {
        let parameters = parse_form(b"Action=CreateAccessKey&UserName=Jane+Doe%40example");

        assert_eq!(
            parameters.get("Action").map(String::as_str),
            Some("CreateAccessKey")
        );
        assert_eq!(
            parameters.get("UserName").map(String::as_str),
            Some("Jane Doe@example")
        );
    }

    #[test]
    fn validates_iam_access_key_ids() {
        assert!(valid_access_key_id("AKIAIOSFODNN7EXAMPLE"));
        assert!(!valid_access_key_id("too-short"));
        assert!(!valid_access_key_id("AKIA-with-dashes"));
    }
}
