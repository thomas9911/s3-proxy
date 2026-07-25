use std::borrow::Cow;
use std::collections::HashMap;

use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{templates, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum_route_error::RouteError;
use opendal::Metakey;
use tokio_stream::StreamExt;

pub async fn list_objects(
    Path(bucket_name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = &signature.namespace;

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

    let mut lister = opendal_operator
        .lister_with(&format!("{}/{}/", namespace, bucket_name))
        .recursive(true)
        .metakey(Metakey::ContentLength)
        .await?;

    let prefix = query
        .get("prefix")
        .map(Cow::from)
        .unwrap_or(Cow::Borrowed(""));
    let max_keys = query
        .get("max-keys")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1000)
        .clamp(1, 1000);
    let offset = query
        .get("continuation-token")
        .map(String::as_str)
        .and_then(|token| token.parse::<usize>().ok())
        .unwrap_or(0);
    let mut all_objects = Vec::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(entry) => {
                let metadata = entry.metadata();
                if metadata.is_file() {
                    let key = entry
                        .path()
                        .strip_prefix(&format!("{}/{}/", namespace, bucket_name))
                        .unwrap_or(entry.path());
                    let etag = metadata.etag().map(|y| Cow::from(y.to_string()));
                    let last_modified = metadata
                        .last_modified()
                        .map(|dt| Cow::from(dt.to_rfc3339()));
                    let size = metadata.content_length();
                    if key.starts_with(prefix.as_ref()) {
                        all_objects.push(templates::ListObjectItem {
                            key: Cow::Owned(key.to_string()),
                            etag,
                            last_modified,
                            size,
                        });
                    }
                }
            }
            Err(error) => {
                tracing::error!("{}", error);
                return Err(RouteError::new_internal_server());
            }
        }
    }

    all_objects.sort_by(|left, right| left.key.cmp(&right.key));
    let start = offset.min(all_objects.len());
    let end = (start + max_keys as usize).min(all_objects.len());
    let is_truncated = end < all_objects.len();
    let next_continuation_token = if is_truncated {
        Cow::Owned(end.to_string())
    } else {
        Cow::Borrowed("")
    };
    let objects = &all_objects[start..end];

    let template = templates::ListObjectsTemplate {
        objects,
        is_truncated,
        continuation_token: query
            .get("continuation-token")
            .map(Cow::from)
            .unwrap_or(Cow::Borrowed("")),
        next_continuation_token,
        key_count: (end - start) as u64,
        bucket_name: Cow::from(bucket_name),
        prefix,
        max_keys,
    };

    Ok(askama_axum::into_response(&template))
}
