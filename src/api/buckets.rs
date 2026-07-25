use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{templates, AppState};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use tokio_stream::StreamExt;

pub async fn list_buckets(
    State(AppState {
        opendal_operator, ..
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
) -> Result<Response, RouteError> {
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
    opendal_operator.delete(&bucket_path).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
