use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{templates, AppState};
use axum::body::Body;
use axum::extract::{FromRequest, Path, State};
use axum::http::StatusCode;
use axum::http::{HeaderMap, Request};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use serde::Deserialize;
use tokio_stream::StreamExt;

pub async fn list_buckets(
    State(AppState {
        metadata_store,
        opendal_operator,
        ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = &signature.namespace;

    let mut lister = opendal_operator
        .lister_with(&format!("{}/", namespace))
        .await?;

    let mut buckets = Vec::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(entry) => {
                if entry.metadata().is_dir()
                    && entry.path().trim_end_matches('/') != namespace
                    && entry.name().trim_end_matches('/') != ".s3-proxy"
                {
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

    let owner = metadata_store.namespace_owner(namespace).await?;
    let template = templates::ListBucketsTemplate {
        owner_name: &owner.display_name,
        owner_id: &owner.id,
        buckets,
    };

    Ok(template.into_response())
}

pub async fn put_bucket(
    Path(bucket_name): Path<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let is_policy = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|part| part == "policy" || part.starts_with("policy="));
    let is_versioning = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|part| part == "versioning" || part.starts_with("versioning="));
    let header_map = request.headers().clone();
    let signature = match VerifiedRequest::from_request(request, &state).await {
        Ok(signature) => signature,
        Err(error) => return error.into_response(),
    };

    if is_policy {
        let policy = match std::str::from_utf8(&signature.bytes) {
            Ok(policy) => policy,
            Err(_) => {
                return s3_error_response(
                    StatusCode::BAD_REQUEST,
                    "MalformedPolicy",
                    "The bucket policy is not valid UTF-8.",
                )
            }
        };
        if serde_json::from_str::<serde_json::Value>(policy).is_err() {
            return s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedPolicy",
                "The bucket policy is not valid JSON.",
            );
        }
        if let Err(error) = state
            .metadata_store
            .set_bucket_policy(&signature.namespace, &bucket_name, policy)
            .await
        {
            tracing::error!(%error, "failed to store bucket policy");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return StatusCode::NO_CONTENT.into_response();
    }

    if is_versioning {
        if !state
            .opendal_operator
            .exists(&format!("{}/{}/", signature.namespace, bucket_name))
            .await
            .unwrap_or(false)
        {
            return s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchBucket",
                "The specified bucket does not exist.",
            );
        }
        let configuration: VersioningConfiguration =
            match quick_xml::de::from_reader(signature.bytes.as_ref()) {
                Ok(configuration) => configuration,
                Err(_) => {
                    return s3_error_response(
                        StatusCode::BAD_REQUEST,
                        "MalformedXML",
                        "The XML you provided was not well-formed.",
                    )
                }
            };
        let Some(status) =
            crate::versioning::BucketVersioning::parse_s3_status(&configuration.status)
        else {
            return s3_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "The versioning status must be Enabled or Suspended.",
            );
        };
        if let Err(error) = crate::versioning::set_bucket_versioning(
            &state.opendal_operator,
            &signature.namespace,
            &bucket_name,
            status,
        )
        .await
        {
            tracing::error!(%error, "failed to set bucket versioning");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return StatusCode::OK.into_response();
    }

    create_bucket_inner(bucket_name, header_map, state, signature)
        .await
        .map_or_else(IntoResponse::into_response, |response| response)
}

#[derive(Debug, Deserialize)]
#[serde(rename = "VersioningConfiguration")]
struct VersioningConfiguration {
    #[serde(rename = "Status")]
    status: String,
}

async fn create_bucket_inner(
    bucket_name: String,
    header_map: HeaderMap,
    AppState {
        metadata_store,
        opendal_operator,
        ..
    }: AppState,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = &signature.namespace;

    let utf8_slice = match std::str::from_utf8(&signature.bytes) {
        Ok(body) => body,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "The request body is not valid UTF-8 XML.",
            ))
        }
    };

    let _body: Option<templates::CreateBucket> = match quick_xml::de::from_str(utf8_slice) {
        Ok(body) => body,
        Err(_) => {
            return Ok(s3_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "The XML you provided was not well-formed.",
            ))
        }
    };

    opendal_operator
        .create_dir(&format!("{}/", namespace))
        .await?;
    opendal_operator
        .create_dir(&format!("{}/{}/", namespace, bucket_name))
        .await?;

    if let Some(public) = super::objects::public_acl(&header_map) {
        metadata_store
            .set_bucket_public(namespace, &bucket_name, public)
            .await?;
    }

    Ok("OK".into_response())
}

pub async fn delete_bucket_route(
    Path(bucket_name): Path<String>,
    State(state): State<AppState>,
    request: Request<Body>,
) -> Response {
    let is_policy = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|part| part == "policy" || part.starts_with("policy="));
    let signature = match VerifiedRequest::from_request(request, &state).await {
        Ok(signature) => signature,
        Err(error) => return error.into_response(),
    };
    if is_policy {
        if let Err(error) = state
            .metadata_store
            .delete_bucket_policy(&signature.namespace, &bucket_name)
            .await
        {
            tracing::error!(%error, "failed to delete bucket policy");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return StatusCode::NO_CONTENT.into_response();
    }

    delete_bucket_inner(bucket_name, state, signature)
        .await
        .map_or_else(IntoResponse::into_response, |response| response)
}

pub(crate) async fn delete_bucket_inner(
    bucket_name: String,
    AppState {
        metadata_store,
        opendal_operator,
        ..
    }: AppState,
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
    opendal_operator
        .delete_with(&bucket_path)
        .recursive(true)
        .await?;
    metadata_store
        .delete_public_bucket(&namespace, &bucket_name)
        .await?;
    metadata_store
        .delete_bucket_policy(&namespace, &bucket_name)
        .await?;
    crate::versioning::delete_bucket_state(&opendal_operator, &namespace, &bucket_name).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
