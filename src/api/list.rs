use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::signature::{s3_error_response, VerifiedRequest};
use crate::{storage, templates, AppState};
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Query, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_route_error::RouteError;
use tokio_stream::StreamExt;

pub async fn get_bucket(
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
    let is_versions = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|part| part == "versions" || part.starts_with("versions="));
    let authenticated = request
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION);
    let query = match Query::<HashMap<String, String>>::try_from_uri(request.uri()) {
        Ok(Query(query)) => query,
        Err(error) => return error.into_response(),
    };
    let mut signature = if request
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION)
        || is_policy
        || is_versioning
        || is_versions
    {
        match VerifiedRequest::from_request(request, &state).await {
            Ok(signature) => signature,
            Err(error) => return error.into_response(),
        }
    } else {
        let resource = format!("arn:aws:s3:::{bucket_name}");
        let mut namespace = if crate::policy::allows_public_read_acl("s3:ListBucket") {
            state
                .metadata_store
                .public_bucket_namespace(&bucket_name)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        if namespace.is_none() {
            namespace = match state.metadata_store.bucket_policies(&bucket_name).await {
                Ok(policies) => policies.into_iter().find_map(|(namespace, policy)| {
                    crate::policy::allows_anonymous(&policy, "s3:ListBucket", &resource)
                        .ok()
                        .filter(|allowed| *allowed)
                        .map(|_| namespace)
                }),
                Err(error) => {
                    tracing::error!(%error, "failed to resolve bucket policy");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
        }
        let Some(namespace) = namespace else {
            return s3_error_response(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        };
        VerifiedRequest {
            access_key: namespace.clone(),
            namespace,
            bytes: Bytes::new(),
        }
    };

    if authenticated {
        let resource = format!("arn:aws:s3:::{bucket_name}");
        let policies = match state.metadata_store.bucket_policies(&bucket_name).await {
            Ok(policies) => policies,
            Err(error) => {
                tracing::error!(%error, "failed to resolve bucket policy");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        for (namespace, policy) in policies {
            let principal = signature.namespace.as_str();
            match crate::policy::decision(&policy, Some(principal), "s3:ListBucket", &resource) {
                Ok(crate::policy::Decision::Deny) => {
                    return s3_error_response(
                        StatusCode::FORBIDDEN,
                        "AccessDenied",
                        "Access Denied",
                    );
                }
                Ok(crate::policy::Decision::Allow) => {
                    signature.namespace = namespace;
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "invalid bucket policy");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
    }

    if is_policy {
        return match state
            .metadata_store
            .bucket_policy(&signature.namespace, &bucket_name)
            .await
        {
            Ok(Some(policy)) => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(policy))
                .expect("static policy response headers are valid"),
            Ok(None) => s3_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchBucketPolicy",
                "The bucket policy does not exist.",
            ),
            Err(error) => {
                tracing::error!(%error, "failed to read bucket policy");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    if is_versioning {
        let status = match crate::versioning::bucket_versioning(
            &state.opendal_operator,
            &state.config,
            &signature.namespace,
            &bucket_name,
        )
        .await
        {
            Ok(status) => status,
            Err(error) => {
                tracing::error!(%error, "failed to read bucket versioning");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        return templates::xml_response(
            StatusCode::OK,
            templates::GetBucketVersioningTemplate {
                status: status.as_s3_status(),
            },
        );
    }

    if is_versions {
        return list_object_versions(bucket_name, state, signature).await;
    }

    list_objects_inner(bucket_name, query, state, signature)
        .await
        .map_or_else(IntoResponse::into_response, |response| response)
}

async fn list_object_versions(
    bucket_name: String,
    state: AppState,
    signature: VerifiedRequest,
) -> Response {
    let namespace = &signature.namespace;
    if !state
        .opendal_operator
        .exists(&format!("{namespace}/{bucket_name}/"))
        .await
        .unwrap_or(false)
    {
        return s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        );
    }
    let versions = match crate::versioning::list_versions(
        &state.opendal_operator,
        &state.config,
        namespace,
        &bucket_name,
    )
    .await
    {
        Ok(versions) => versions,
        Err(error) => {
            tracing::error!(%error, "failed to list object versions");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut latest = HashSet::new();
    let mut versions_output = Vec::new();
    let mut markers_output = Vec::new();
    for (key, version) in versions {
        let is_latest = latest.insert(key.clone());
        let size = match version.data_path.as_deref() {
            Some(path) => state
                .opendal_operator
                .stat(path)
                .await
                .map(|metadata| metadata.content_length())
                .unwrap_or(0),
            None => 0,
        };
        let item = templates::ListVersionItem {
            key,
            version_id: version.version_id,
            is_latest,
            last_modified: version.created_at,
            size,
        };
        if version.is_delete_marker {
            markers_output.push(item);
        } else {
            versions_output.push(item);
        }
    }
    templates::xml_response(
        StatusCode::OK,
        templates::ListObjectVersionsTemplate {
            bucket_name: &bucket_name,
            versions: &versions_output,
            delete_markers: &markers_output,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::get_bucket;
    use crate::metadata::{MetaDataBackend, MetadataStore, SqliteMetadataStore};
    use crate::{AppState, Config, SqliteConfig};
    use axum::body::Body;
    use axum::extract::{Path, State};
    use axum::http::Request;
    use opendal::services::Memory;
    use opendal::Operator;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn lists_anonymous_public_bucket() {
        let metadata_store = Arc::new(
            SqliteMetadataStore::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        metadata_store
            .set_bucket_public("namespace", "bucket", true)
            .await
            .unwrap();
        let operator = Operator::new(Memory::default()).unwrap();
        operator.create_dir("namespace/bucket/").await.unwrap();
        operator
            .write("namespace/bucket/object.txt", b"object".to_vec())
            .await
            .unwrap();
        let state = AppState {
            metadata_store,
            config: Arc::new(Config {
                server_host: "127.0.0.1:0".to_string(),
                external_server_host: "http://127.0.0.1:0".to_string(),
                max_request_body_bytes: 256 * 1024 * 1024,
                metadata_backend: MetaDataBackend::Sqlite,
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
            }),
            opendal_operator: operator,
        };
        let response = get_bucket(
            Path("bucket".to_string()),
            State(state),
            Request::builder()
                .uri("/bucket?list-type=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }
}

async fn list_objects_inner(
    bucket_name: String,
    query: HashMap<String, String>,
    AppState {
        opendal_operator,
        config,
        ..
    }: AppState,
    signature: VerifiedRequest,
) -> Result<Response, RouteError> {
    let namespace = &signature.namespace;
    let Some(bucket_prefix) = storage::bucket_prefix(&config, namespace, &bucket_name) else {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    };

    if !storage::is_single_bucket(&config) && !opendal_operator.exists(&bucket_prefix).await? {
        return Ok(s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ));
    }

    let mut lister = opendal_operator
        .lister_with(&bucket_prefix)
        .recursive(true)
        .await?;

    let prefix = query
        .get("prefix")
        .map(Cow::from)
        .unwrap_or(Cow::Borrowed(""));
    let delimiter = query.get("delimiter").map(String::as_str);
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
    let mut common_prefixes = HashSet::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(entry) => {
                let metadata = entry.metadata();
                if metadata.is_file() {
                    let key = entry
                        .path()
                        .strip_prefix(&bucket_prefix)
                        .unwrap_or(entry.path());
                    if key == ".s3-proxy" || key.starts_with(".s3-proxy/") {
                        continue;
                    }
                    let etag = metadata.etag().map(|y| Cow::from(y.to_string()));
                    let last_modified =
                        metadata.last_modified().map(|dt| Cow::from(dt.to_string()));
                    let size = metadata.content_length();
                    if key.starts_with(prefix.as_ref()) {
                        if let Some(delimiter) = delimiter.filter(|value| !value.is_empty()) {
                            let remainder = &key[prefix.len()..];
                            if let Some(index) = remainder.find(delimiter) {
                                common_prefixes.insert(format!(
                                    "{}{}",
                                    prefix,
                                    &remainder[..index + delimiter.len()]
                                ));
                                continue;
                            }
                        }
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
    let mut common_prefixes: Vec<_> = common_prefixes
        .into_iter()
        .map(|prefix| templates::ListCommonPrefix {
            prefix: Cow::Owned(prefix),
        })
        .collect();
    common_prefixes.sort_by(|left, right| left.prefix.cmp(&right.prefix));
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
        key_count: (end - start + common_prefixes.len()) as u64,
        bucket_name: Cow::from(bucket_name),
        prefix,
        max_keys,
        common_prefixes: &common_prefixes,
    };

    Ok(template.into_response())
}
