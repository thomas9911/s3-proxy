use crate::metadata::NamespaceOwner;
use crate::{metadata::AccessKeyStatus, metrics, templates, AppState};
use askama::Template;
use aws_sigv4::sign::v4::{calculate_signature, generate_signing_key};
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use rand::random;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use tokio_stream::StreamExt;

pub async fn management_dashboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    let principals = match principals(&state).await {
        Ok(principals) => principals,
        Err(error) => return internal_error(error, "failed to load dashboard principals"),
    };
    let buckets = match buckets(&state, "").await {
        Ok(buckets) => buckets,
        Err(error) => return internal_error(error, "failed to load dashboard buckets"),
    };
    match (templates::ManagementDashboardTemplate {
        principals: &principals,
        buckets: &buckets,
    })
    .render()
    {
        Ok(body) => Html(body).into_response(),
        Err(error) => internal_error(error, "failed to render management dashboard"),
    }
}

pub async fn management_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    Json(status(&state).await).into_response()
}

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

fn default_audit_limit() -> usize {
    50
}

pub async fn management_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    Json(crate::audit::recent(query.limit)).into_response()
}

pub async fn management_metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(axum::body::Body::from(metrics::render()))
        .expect("static metrics response is valid")
}

pub async fn management_status_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    render(templates::ManagementStatusFragmentTemplate {
        status: status(&state).await,
    })
}

pub async fn management_principals(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match principals(&state).await {
        Ok(principals) => Json(principals).into_response(),
        Err(error) => internal_error(error, "failed to list management principals"),
    }
}

pub async fn management_principals_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match principals(&state).await {
        Ok(principals) => render(templates::ManagementPrincipalsFragmentTemplate {
            principals: &principals,
        }),
        Err(error) => internal_error(error, "failed to list management principals"),
    }
}

#[derive(Deserialize)]
pub struct BucketQuery {
    #[serde(default)]
    prefix: String,
}

pub async fn management_buckets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BucketQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match buckets(&state, &query.prefix).await {
        Ok(buckets) => Json(buckets).into_response(),
        Err(error) => internal_error(error, "failed to list management buckets"),
    }
}

pub async fn management_buckets_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BucketQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match buckets(&state, &query.prefix).await {
        Ok(buckets) => render(templates::ManagementBucketsFragmentTemplate { buckets: &buckets }),
        Err(error) => internal_error(error, "failed to list management buckets"),
    }
}

#[derive(Clone, Deserialize)]
pub struct ObjectQuery {
    namespace: String,
    bucket: String,
    #[serde(default)]
    prefix: String,
}

pub async fn management_list_objects(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match list_objects(&state, query).await {
        Ok(objects) => Json(objects).into_response(),
        Err(error) => invalid_request(error),
    }
}

pub async fn management_upload_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    let mut namespace = None;
    let mut bucket = None;
    let mut key = None;
    let mut file = None;
    let mut file_name = None;
    let mut content_type = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return invalid_request(format!("invalid multipart form: {error}")),
        };
        match field.name() {
            Some("namespace") => match field.text().await {
                Ok(value) => namespace = Some(value),
                Err(error) => return invalid_request(format!("invalid namespace field: {error}")),
            },
            Some("bucket") => match field.text().await {
                Ok(value) => bucket = Some(value),
                Err(error) => return invalid_request(format!("invalid bucket field: {error}")),
            },
            Some("key") => match field.text().await {
                Ok(value) => key = Some(value),
                Err(error) => return invalid_request(format!("invalid key field: {error}")),
            },
            Some("file") => {
                content_type = field.content_type().map(str::to_owned);
                file_name = field
                    .file_name()
                    .and_then(|name| name.rsplit(['/', '\\']).next())
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                match field.bytes().await {
                    Ok(value) => file = Some(value),
                    Err(error) => return invalid_request(format!("invalid file field: {error}")),
                }
            }
            _ => {}
        }
    }
    let key = key.filter(|key| !key.is_empty()).or(file_name);
    let (Some(namespace), Some(bucket), Some(key), Some(file)) = (namespace, bucket, key, file)
    else {
        return invalid_request("namespace, bucket, and a named file are required");
    };
    if !valid_bucket_name(&bucket) || !valid_object_key(&key) || key.is_empty() {
        return invalid_request("the bucket or object key is invalid");
    }
    let mut object_headers = HeaderMap::new();
    if let Some(content_type) = content_type.and_then(|value| HeaderValue::from_str(&value).ok()) {
        object_headers.insert(header::CONTENT_TYPE, content_type);
    }
    let request = crate::signature::VerifiedRequest {
        access_key: namespace.clone(),
        namespace,
        bytes: file,
    };
    match super::objects::create_object(Path((bucket, key)), object_headers, State(state), request)
        .await
    {
        Ok(_) => (StatusCode::SEE_OTHER, [(header::LOCATION, "/admin")]).into_response(),
        Err(error) => internal_error(error, "failed to upload management object"),
    }
}

#[derive(Deserialize)]
pub struct PresignObjectInput {
    namespace: String,
    bucket: String,
    key: String,
    access_key: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Serialize)]
struct PresignedUrl {
    url: String,
    expires_in: u64,
}

pub async fn management_presign_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<PresignObjectInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match presign_object(&state, input).await {
        Ok(url) => Json(url).into_response(),
        Err(error) => invalid_request(error),
    }
}

#[derive(Deserialize)]
pub struct DownloadQuery {
    namespace: String,
    bucket: String,
    key: String,
}

pub async fn management_download_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DownloadQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match download_object(&state, query).await {
        Ok(response) => response,
        Err(error) => invalid_request(error),
    }
}

#[derive(Deserialize)]
pub struct PresignPostInput {
    namespace: String,
    bucket: String,
    access_key: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    max_size: Option<u64>,
}

#[derive(Serialize)]
struct PresignedPost {
    url: String,
    fields: HashMap<String, String>,
    expires_in: u64,
}

pub async fn management_presign_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<PresignPostInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match presign_post(&state, input).await {
        Ok(form) => Json(form).into_response(),
        Err(error) => invalid_request(error),
    }
}

#[derive(Deserialize)]
pub struct CreateManagementAccessKeyInput {
    namespace: String,
}

#[derive(Deserialize)]
pub struct UpdateManagementAccessKeyInput {
    namespace: String,
    access_key: String,
    status: String,
}

#[derive(Deserialize)]
pub struct RotateManagementAccessKeyInput {
    namespace: String,
    access_key: String,
}

#[derive(Deserialize)]
pub struct DeleteManagementAccessKeyInput {
    namespace: String,
    access_key: String,
    confirm: bool,
}

#[derive(Serialize)]
struct ManagementAccessKeySecret {
    id: String,
    secret_key: String,
}

pub async fn management_create_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<CreateManagementAccessKeyInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match create_management_access_key(&state, &input.namespace).await {
        Ok(key) => refresh_dashboard((StatusCode::CREATED, Json(key)).into_response()),
        Err(error) => invalid_request(error),
    }
}

pub async fn management_update_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<UpdateManagementAccessKeyInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    let status = match input.status.as_str() {
        "Active" => AccessKeyStatus::Active,
        "Inactive" => AccessKeyStatus::Inactive,
        _ => return invalid_request("status must be Active or Inactive"),
    };
    match access_key_in_namespace(&state, &input.namespace, &input.access_key).await {
        Ok(_) => match state
            .metadata_store
            .set_access_key_status(&input.access_key, status)
            .await
        {
            Ok(true) => StatusCode::NO_CONTENT.into_response(),
            Ok(false) => invalid_request("the access key does not exist"),
            Err(error) => internal_error(error, "failed to update management access key"),
        },
        Err(error) => invalid_request(error),
    }
}

pub async fn management_rotate_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<RotateManagementAccessKeyInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if let Err(error) = access_key_in_namespace(&state, &input.namespace, &input.access_key).await {
        return invalid_request(error);
    }
    let replacement = match create_management_access_key(&state, &input.namespace).await {
        Ok(key) => key,
        Err(error) => return invalid_request(error),
    };
    match state
        .metadata_store
        .set_access_key_status(&input.access_key, AccessKeyStatus::Inactive)
        .await
    {
        Ok(true) => refresh_dashboard((StatusCode::CREATED, Json(replacement)).into_response()),
        Ok(false) | Err(_) => {
            if let Err(error) = state
                .metadata_store
                .delete_access_key(&replacement.id)
                .await
            {
                tracing::error!(%error, "failed to clean up replacement access key after rotation failure");
            }
            invalid_request("failed to deactivate the access key being rotated")
        }
    }
}

pub async fn management_delete_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<DeleteManagementAccessKeyInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if !input.confirm {
        return invalid_request("access-key deletion requires confirm=true");
    }
    if let Err(error) = access_key_in_namespace(&state, &input.namespace, &input.access_key).await {
        return invalid_request(error);
    }
    match state
        .metadata_store
        .delete_access_key(&input.access_key)
        .await
    {
        Ok(true) => refresh_dashboard(StatusCode::NO_CONTENT.into_response()),
        Ok(false) => invalid_request("the access key does not exist"),
        Err(error) => internal_error(error, "failed to delete management access key"),
    }
}

#[derive(Deserialize)]
pub struct MultipartQuery {
    namespace: String,
    #[serde(default)]
    bucket: Option<String>,
}

#[derive(Deserialize)]
pub struct AbortMultipartInput {
    namespace: String,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    upload_id: Option<String>,
    #[serde(default)]
    older_than_seconds: Option<u64>,
    confirm: bool,
}

pub async fn management_list_multipart_uploads(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MultipartQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match multipart_uploads(&state, query).await {
        Ok(uploads) => Json(uploads).into_response(),
        Err(error) => invalid_request(error),
    }
}

pub async fn management_abort_multipart_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<AbortMultipartInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if !input.confirm {
        return invalid_request("multipart abort requires confirm=true");
    }
    let AbortMultipartInput {
        namespace,
        bucket,
        upload_id,
        older_than_seconds,
        ..
    } = input;
    let bucket = bucket.filter(|bucket| !bucket.is_empty());
    if let Some(bucket) = &bucket {
        if !valid_bucket_name(bucket) {
            return invalid_request("the bucket is invalid");
        }
    }
    match (upload_id, older_than_seconds) {
        (Some(upload_id), None) => {
            let Some(bucket) = bucket else {
                return invalid_request("bucket is required when aborting a specific upload");
            };
            abort_management_upload(&state, namespace, bucket, upload_id).await
        }
        (None, Some(older_than_seconds)) if older_than_seconds > 0 => {
            let uploads = match multipart_uploads(
                &state,
                MultipartQuery {
                    namespace: namespace.clone(),
                    bucket,
                },
            )
            .await
            {
                Ok(uploads) => uploads,
                Err(error) => return invalid_request(error),
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            for upload in uploads {
                let created_at = upload.created_at.parse::<u64>().unwrap_or(now);
                if now.saturating_sub(created_at) >= older_than_seconds {
                    let response = abort_management_upload(
                        &state,
                        namespace.clone(),
                        upload.bucket,
                        upload.upload_id,
                    )
                    .await;
                    if response.status() != StatusCode::NO_CONTENT {
                        return response;
                    }
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        _ => invalid_request("provide either upload_id or older_than_seconds"),
    }
}

#[derive(Deserialize)]
pub struct QuotaQuery {
    namespace: String,
}

#[derive(Deserialize)]
pub struct UpdateQuotaInput {
    namespace: String,
    #[serde(default)]
    max_storage_bytes: String,
    #[serde(default)]
    max_requests_per_minute: String,
}

#[derive(Serialize)]
struct ManagementQuota {
    namespace: String,
    storage_bytes: u64,
    request_count: u64,
    request_window_remaining_seconds: u64,
    max_storage_bytes: Option<u64>,
    max_requests_per_minute: Option<u64>,
}

pub async fn management_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<QuotaQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match quota(&state, query.namespace).await {
        Ok(quota) => Json(quota).into_response(),
        Err(error) => invalid_request(error),
    }
}

pub async fn management_update_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<UpdateQuotaInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if let Err(error) = namespace_exists(&state, &input.namespace).await {
        return invalid_request(error);
    }
    let max_storage_bytes = match parse_optional_limit(&input.max_storage_bytes) {
        Ok(limit) => limit,
        Err(error) => return invalid_request(error),
    };
    let max_requests_per_minute = match parse_optional_limit(&input.max_requests_per_minute) {
        Ok(limit) => limit,
        Err(error) => return invalid_request(error),
    };
    crate::quota::set_runtime_quota(
        input.namespace,
        crate::quota::PrincipalQuotaConfig {
            max_storage_bytes,
            max_requests_per_minute,
        },
    );
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
pub struct UpdateBucketConfigurationInput {
    namespace: String,
    bucket: String,
    #[serde(default)]
    versioning: String,
    #[serde(default)]
    policy: String,
    #[serde(default)]
    clear_policy: bool,
}

pub async fn management_update_bucket_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<UpdateBucketConfigurationInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if !valid_bucket_name(&input.bucket) {
        return invalid_request("the bucket is invalid");
    }
    if !state
        .opendal_operator
        .exists(&format!("{}/{}/", input.namespace, input.bucket))
        .await
        .unwrap_or(false)
    {
        return invalid_request("the specified bucket does not exist");
    }
    if !input.versioning.is_empty() {
        let status = match input.versioning.as_str() {
            "Off" => crate::versioning::BucketVersioning::Off,
            "Enabled" => crate::versioning::BucketVersioning::Enabled,
            "Suspended" => crate::versioning::BucketVersioning::Suspended,
            _ => return invalid_request("versioning must be Off, Enabled, or Suspended"),
        };
        if let Err(error) = crate::versioning::set_bucket_versioning(
            &state.opendal_operator,
            &input.namespace,
            &input.bucket,
            status,
        )
        .await
        {
            return internal_error(error, "failed to update bucket versioning");
        }
    }
    if input.clear_policy {
        if let Err(error) = state
            .metadata_store
            .delete_bucket_policy(&input.namespace, &input.bucket)
            .await
        {
            return internal_error(error, "failed to delete bucket policy");
        }
    } else if !input.policy.trim().is_empty() {
        if serde_json::from_str::<serde_json::Value>(&input.policy).is_err() {
            return invalid_request("the bucket policy is not valid JSON");
        }
        if let Err(error) = state
            .metadata_store
            .set_bucket_policy(&input.namespace, &input.bucket, &input.policy)
            .await
        {
            return internal_error(error, "failed to update bucket policy");
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn management_list_objects_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ObjectQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match list_objects(&state, query.clone()).await {
        Ok(objects) => render(templates::ManagementObjectsFragmentTemplate {
            namespace: &query.namespace,
            bucket: &query.bucket,
            objects: &objects,
        }),
        Err(error) => invalid_request(error),
    }
}

#[derive(Deserialize)]
pub struct InspectionQuery {
    namespace: String,
    bucket: String,
    #[serde(default)]
    key: Option<String>,
}

pub async fn management_inspect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InspectionQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match inspect(&state, query).await {
        Ok(inspection) => Json(inspection).into_response(),
        Err(error) => invalid_request(error),
    }
}

pub async fn management_inspect_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InspectionQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match inspect(&state, query).await {
        Ok(inspection) => render(templates::ManagementInspectionFragmentTemplate {
            inspection: &inspection,
        }),
        Err(error) => invalid_request(error),
    }
}

#[derive(Deserialize)]
pub struct CreateBucketInput {
    namespace: String,
    name: String,
}

pub async fn management_create_bucket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<CreateBucketInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    match create_bucket(&state, input).await {
        Ok(bucket) => (StatusCode::CREATED, Json(bucket)).into_response(),
        Err(error) => invalid_request(error),
    }
}

pub async fn management_create_bucket_fragment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<CreateBucketInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if let Err(error) = create_bucket(&state, input).await {
        return invalid_request(error);
    }
    let mut response = match buckets(&state, "").await {
        Ok(buckets) => render(templates::ManagementBucketsFragmentTemplate { buckets: &buckets }),
        Err(error) => return internal_error(error, "failed to list management buckets"),
    };
    response
        .headers_mut()
        .insert("HX-Refresh", HeaderValue::from_static("true"));
    response
}

#[derive(Deserialize)]
pub struct DeleteBucketInput {
    namespace: String,
    name: String,
    confirm: bool,
}

pub async fn management_delete_bucket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<DeleteBucketInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if !input.confirm {
        return invalid_request("bucket deletion requires confirm=true");
    }
    if !valid_bucket_name(&input.name) {
        return invalid_request("the bucket is invalid");
    }
    let request = crate::signature::VerifiedRequest {
        access_key: input.namespace.clone(),
        namespace: input.namespace,
        bytes: Default::default(),
    };
    match super::buckets::delete_bucket_inner(input.name, state, request).await {
        Ok(response) => response,
        Err(error) => internal_error(error, "failed to delete management bucket"),
    }
}

#[derive(Deserialize)]
pub struct DeleteObjectInput {
    namespace: String,
    bucket: String,
    key: String,
    confirm: bool,
}

pub async fn management_delete_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<DeleteObjectInput>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if !input.confirm {
        return invalid_request("object deletion requires confirm=true");
    }
    if !valid_bucket_name(&input.bucket) || input.key.is_empty() || !valid_object_key(&input.key) {
        return invalid_request("the bucket or object key is invalid");
    }
    let request = crate::signature::VerifiedRequest {
        access_key: input.namespace.clone(),
        namespace: input.namespace,
        bytes: Default::default(),
    };
    match super::objects::delete_object(Path((input.bucket, input.key)), State(state), request)
        .await
    {
        Ok(response) => response,
        Err(error) => internal_error(error, "failed to delete management object"),
    }
}

async fn status(state: &AppState) -> templates::ManagementStatusTemplate {
    let capability = state.opendal_operator.info().capability();
    let storage_capabilities = [
        ("stat", capability.stat),
        ("read", capability.read),
        ("write", capability.write),
        ("create_dir", capability.create_dir),
        ("delete", capability.delete),
        ("list", capability.list),
        ("recursive_list", capability.list_with_recursive),
    ]
    .into_iter()
    .filter_map(|(name, supported)| supported.then(|| name.to_string()))
    .collect();
    templates::ManagementStatusTemplate {
        metadata_ready: state.metadata_store.debug_keys("__readyz__").await.is_ok(),
        storage_ready: state.opendal_operator.exists(".s3-proxy/").await.is_ok(),
        metadata_backend: state.config.metadata_backend.as_str().to_string(),
        opendal_provider: state.config.opendal_provider.clone(),
        storage_capabilities,
        metrics: metrics::render(),
    }
}

async fn principals(state: &AppState) -> anyhow::Result<Vec<templates::ManagementPrincipal>> {
    let owners = state.metadata_store.list_namespace_owners().await?;
    let mut principals = Vec::with_capacity(owners.len());
    for (namespace, owner) in owners {
        principals.push(principal(state, namespace, owner).await?);
    }
    Ok(principals)
}

async fn principal(
    state: &AppState,
    namespace: String,
    owner: NamespaceOwner,
) -> anyhow::Result<templates::ManagementPrincipal> {
    let access_keys = state
        .metadata_store
        .list_access_keys(&namespace)
        .await?
        .into_iter()
        .map(|access_key| templates::ManagementAccessKey {
            id: access_key.id,
            status: access_key.status.as_str().to_string(),
            created_at: access_key.created_at,
            last_used_at: access_key.last_used_at,
        })
        .collect();
    Ok(templates::ManagementPrincipal {
        namespace,
        display_name: owner.display_name,
        id: owner.id,
        access_keys,
    })
}

async fn buckets(
    state: &AppState,
    prefix: &str,
) -> anyhow::Result<Vec<templates::ManagementBucket>> {
    let owners = state.metadata_store.list_namespace_owners().await?;
    let mut buckets = Vec::new();
    for (namespace, _) in owners {
        let namespace_prefix = format!("{namespace}/");
        let mut lister = state
            .opendal_operator
            .lister_with(&namespace_prefix)
            .await?;
        while let Some(entry) = lister.next().await {
            let entry = entry?;
            if entry.metadata().is_dir() {
                let name = entry
                    .path()
                    .strip_prefix(&namespace_prefix)
                    .unwrap_or_default()
                    .trim_end_matches('/');
                if !name.is_empty()
                    && !name.contains('/')
                    && name != ".s3-proxy"
                    && name.starts_with(prefix)
                {
                    buckets.push(templates::ManagementBucket {
                        namespace: namespace.clone(),
                        name: name.to_string(),
                    });
                }
            }
        }
    }
    buckets.sort_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.name.cmp(&right.name))
    });
    Ok(buckets)
}

async fn create_bucket(
    state: &AppState,
    input: CreateBucketInput,
) -> anyhow::Result<templates::ManagementBucket> {
    if !valid_bucket_name(&input.name) {
        anyhow::bail!("bucket names must be DNS-compatible and 3-63 characters long")
    }
    let owners = state.metadata_store.list_namespace_owners().await?;
    if !owners
        .iter()
        .any(|(namespace, _)| namespace == &input.namespace)
    {
        anyhow::bail!("the requested principal namespace does not exist")
    }
    let path = format!("{}/{}/", input.namespace, input.name);
    if state.opendal_operator.exists(&path).await? {
        anyhow::bail!("the bucket already exists")
    }
    state
        .opendal_operator
        .create_dir(&format!("{}/", input.namespace))
        .await?;
    state.opendal_operator.create_dir(&path).await?;
    Ok(templates::ManagementBucket {
        namespace: input.namespace,
        name: input.name,
    })
}

async fn list_objects(
    state: &AppState,
    query: ObjectQuery,
) -> anyhow::Result<Vec<templates::ManagementObject>> {
    if !valid_bucket_name(&query.bucket) || !valid_object_key(&query.prefix) {
        anyhow::bail!("the bucket or object prefix is invalid")
    }
    let bucket_path = format!("{}/{}/", query.namespace, query.bucket);
    if !state.opendal_operator.exists(&bucket_path).await? {
        anyhow::bail!("the specified bucket does not exist")
    }
    let mut lister = state
        .opendal_operator
        .lister_with(&bucket_path)
        .recursive(true)
        .await?;
    let mut objects = Vec::new();
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if !entry.metadata().is_file() {
            continue;
        }
        let key = entry
            .path()
            .strip_prefix(&bucket_path)
            .unwrap_or(entry.path())
            .to_string();
        if key.starts_with(&query.prefix) {
            objects.push(templates::ManagementObject {
                key,
                size: entry.metadata().content_length(),
            });
        }
    }
    objects.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(objects)
}

async fn presign_object(
    state: &AppState,
    input: PresignObjectInput,
) -> anyhow::Result<PresignedUrl> {
    if !valid_bucket_name(&input.bucket) || input.key.is_empty() || !valid_object_key(&input.key) {
        anyhow::bail!("the bucket or object key is invalid")
    }
    let expires_in = input.expires_in.unwrap_or(3600);
    if !(1..=604_800).contains(&expires_in) {
        anyhow::bail!("expiry must be between 1 and 604800 seconds")
    }
    let access_key = state
        .metadata_store
        .access_key(&input.access_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the access key does not exist"))?;
    if access_key.principal_id != input.namespace {
        anyhow::bail!("the access key does not belong to the selected namespace")
    }
    if access_key.status.as_str() != "Active" {
        anyhow::bail!("the access key is inactive")
    }
    let path = format!("{}/{}/{}", input.namespace, input.bucket, input.key);
    if !state.opendal_operator.exists(&path).await? {
        anyhow::bail!("the specified object does not exist")
    }
    Ok(PresignedUrl {
        url: crate::signature::presign_get_url(
            &state.config.external_server_host,
            &input.bucket,
            &input.key,
            &access_key.id,
            &access_key.secret_key,
            "us-east-1",
            expires_in,
        )?,
        expires_in,
    })
}

async fn download_object(state: &AppState, query: DownloadQuery) -> anyhow::Result<Response> {
    if !valid_bucket_name(&query.bucket) || query.key.is_empty() || !valid_object_key(&query.key) {
        anyhow::bail!("the bucket or object key is invalid")
    }
    let path = format!("{}/{}/{}", query.namespace, query.bucket, query.key);
    let metadata = state.opendal_operator.stat(&path).await?;
    if !metadata.is_file() {
        anyhow::bail!("the specified object does not exist")
    }
    let stored = state
        .metadata_store
        .object_metadata(&query.namespace, &query.bucket, &query.key)
        .await?;
    let filename = query.key.rsplit('/').next().unwrap_or("download");
    let mut response =
        axum::body::Body::from(state.opendal_operator.read(&path).await?.to_vec()).into_response();
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{}",
            urlencoding::encode(filename)
        ))?,
    );
    if let Some(content_type) = metadata.content_type().or(stored.content_type.as_deref()) {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }
    Ok(response)
}

async fn create_management_access_key(
    state: &AppState,
    namespace: &str,
) -> anyhow::Result<ManagementAccessKeySecret> {
    namespace_exists(state, namespace).await?;
    for _ in 0..5 {
        let id = format!("AKIA{}", hex::encode(random::<[u8; 8]>()));
        let secret_key =
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(random::<[u8; 30]>());
        if state
            .metadata_store
            .create_access_key(&id, &secret_key, namespace)
            .await?
        {
            return Ok(ManagementAccessKeySecret { id, secret_key });
        }
    }
    anyhow::bail!("could not generate a unique access key")
}

async fn namespace_exists(state: &AppState, namespace: &str) -> anyhow::Result<()> {
    if state
        .metadata_store
        .list_namespace_owners()
        .await?
        .iter()
        .any(|(candidate, _)| candidate == namespace)
    {
        Ok(())
    } else {
        anyhow::bail!("the requested principal namespace does not exist")
    }
}

async fn quota(state: &AppState, namespace: String) -> anyhow::Result<ManagementQuota> {
    namespace_exists(state, &namespace).await?;
    let quota = crate::quota::quota_for(&state.config.quotas, &namespace);
    let storage_bytes = crate::quota::namespace_size(&state.opendal_operator, &namespace).await?;
    let (request_count, request_window_remaining_seconds) = crate::quota::request_usage(&namespace);
    Ok(ManagementQuota {
        namespace,
        storage_bytes,
        request_count,
        request_window_remaining_seconds,
        max_storage_bytes: quota.max_storage_bytes,
        max_requests_per_minute: quota.max_requests_per_minute,
    })
}

fn parse_optional_limit(value: &str) -> anyhow::Result<Option<u64>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let value = value
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("quota limits must be non-negative integers"))?;
    if value == 0 {
        anyhow::bail!("quota limits must be greater than zero or left empty")
    }
    Ok(Some(value))
}

async fn access_key_in_namespace(
    state: &AppState,
    namespace: &str,
    access_key: &str,
) -> anyhow::Result<()> {
    let record = state
        .metadata_store
        .access_key(access_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the access key does not exist"))?;
    if record.principal_id != namespace {
        anyhow::bail!("the access key does not belong to the selected namespace")
    }
    Ok(())
}

#[derive(Deserialize)]
struct StoredMultipartManifest {
    bucket: String,
    object: String,
    #[serde(default)]
    created_at: u64,
}

async fn multipart_uploads(
    state: &AppState,
    query: MultipartQuery,
) -> anyhow::Result<Vec<templates::ManagementMultipartUpload>> {
    if let Some(bucket) = &query.bucket {
        if !valid_bucket_name(bucket) {
            anyhow::bail!("the bucket is invalid")
        }
    }
    let prefix = format!("{}/.multipart/", query.namespace);
    if !state.opendal_operator.exists(&prefix).await? {
        return Ok(Vec::new());
    }
    let mut lister = state
        .opendal_operator
        .lister_with(&prefix)
        .recursive(true)
        .await?;
    let mut uploads = Vec::new();
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if !entry.metadata().is_file() || entry.name() != "manifest.json" {
            continue;
        }
        let manifest: StoredMultipartManifest =
            serde_json::from_slice(&state.opendal_operator.read(entry.path()).await?.to_vec())?;
        if query
            .bucket
            .as_deref()
            .is_some_and(|bucket| bucket != manifest.bucket)
        {
            continue;
        }
        let upload_id = entry
            .path()
            .strip_prefix(&prefix)
            .and_then(|path| path.strip_suffix("/manifest.json"))
            .ok_or_else(|| anyhow::anyhow!("invalid multipart manifest path"))?
            .to_string();
        let created_at = if manifest.created_at > 0 {
            manifest.created_at.to_string()
        } else {
            entry
                .metadata()
                .last_modified()
                .map(SystemTime::from)
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_secs().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        };
        uploads.push(templates::ManagementMultipartUpload {
            namespace: query.namespace.clone(),
            bucket: manifest.bucket,
            key: manifest.object,
            upload_id,
            created_at,
        });
    }
    uploads.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    Ok(uploads)
}

async fn abort_management_upload(
    state: &AppState,
    namespace: String,
    bucket: String,
    upload_id: String,
) -> Response {
    let request = crate::signature::VerifiedRequest {
        access_key: namespace.clone(),
        namespace,
        bytes: Default::default(),
    };
    match super::post::abort_multipart(bucket, state.clone(), request, upload_id).await {
        Ok(response) => response,
        Err(error) => internal_error(error, "failed to abort management multipart upload"),
    }
}

async fn presign_post(state: &AppState, input: PresignPostInput) -> anyhow::Result<PresignedPost> {
    if !valid_bucket_name(&input.bucket) || !valid_object_key(&input.prefix) {
        anyhow::bail!("the bucket or object prefix is invalid")
    }
    let expires_in = input.expires_in.unwrap_or(3600);
    if !(1..=604_800).contains(&expires_in) {
        anyhow::bail!("expiry must be between 1 and 604800 seconds")
    }
    let max_size = input.max_size.unwrap_or(100 * 1024 * 1024);
    if max_size == 0 {
        anyhow::bail!("maximum upload size must be greater than zero")
    }
    let access_key = state
        .metadata_store
        .access_key(&input.access_key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the access key does not exist"))?;
    if access_key.principal_id != input.namespace {
        anyhow::bail!("the access key does not belong to the selected namespace")
    }
    if access_key.status.as_str() != "Active" {
        anyhow::bail!("the access key is inactive")
    }
    if !state
        .opendal_operator
        .exists(&format!("{}/{}/", input.namespace, input.bucket))
        .await?
    {
        anyhow::bail!("the specified bucket does not exist")
    }
    let now = OffsetDateTime::now_utc();
    let date = now.format(time::macros::format_description!("[year][month][day]"))?;
    let timestamp = now.format(time::macros::format_description!(
        "[year][month][day]T[hour][minute][second]Z"
    ))?;
    let credential = format!("{}/{date}/us-east-1/s3/aws4_request", access_key.id);
    let key = format!("{}${{filename}}", input.prefix);
    let policy = serde_json::json!({
        "expiration": (now + Duration::seconds(expires_in as i64)).format(&Rfc3339)?,
        "conditions": [
            { "bucket": input.bucket },
            ["starts-with", "$key", input.prefix],
            ["content-length-range", 1, max_size],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": timestamp }
        ]
    });
    let policy = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&policy)?);
    let signing_time = crate::signature::parse_date_time(&timestamp)?;
    let signature = calculate_signature(
        generate_signing_key(&access_key.secret_key, signing_time, "us-east-1", "s3"),
        policy.as_bytes(),
    );
    let fields = HashMap::from([
        ("key".to_string(), key),
        (
            "x-amz-algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        ),
        ("x-amz-credential".to_string(), credential),
        ("x-amz-date".to_string(), timestamp),
        ("policy".to_string(), policy),
        ("x-amz-signature".to_string(), signature),
    ]);
    Ok(PresignedPost {
        url: format!(
            "{}/{}",
            state.config.external_server_host.trim_end_matches('/'),
            urlencoding::encode(&input.bucket)
        ),
        fields,
        expires_in,
    })
}

async fn inspect(
    state: &AppState,
    query: InspectionQuery,
) -> anyhow::Result<templates::ManagementInspection> {
    if !valid_bucket_name(&query.bucket) {
        anyhow::bail!("the bucket is invalid")
    }
    if let Some(key) = &query.key {
        if key.is_empty() || !valid_object_key(key) {
            anyhow::bail!("the object key is invalid")
        }
    }
    let bucket_path = format!("{}/{}/", query.namespace, query.bucket);
    if !state.opendal_operator.exists(&bucket_path).await? {
        anyhow::bail!("the specified bucket does not exist")
    }
    let bucket_public = state
        .metadata_store
        .public_bucket_namespace(&query.bucket)
        .await?
        .as_deref()
        == Some(query.namespace.as_str());
    let bucket_policy = state
        .metadata_store
        .bucket_policy(&query.namespace, &query.bucket)
        .await?;
    let versioning = crate::versioning::bucket_versioning(
        &state.opendal_operator,
        &query.namespace,
        &query.bucket,
    )
    .await?
    .as_s3_status()
    .unwrap_or("Off")
    .to_string();
    let (object_public, metadata, versions) = if let Some(key) = &query.key {
        let object_path = format!("{bucket_path}{key}");
        if !state.opendal_operator.exists(&object_path).await? {
            anyhow::bail!("the specified object does not exist")
        }
        let object_public = state
            .metadata_store
            .public_object_namespace(&query.bucket, key)
            .await?
            .as_deref()
            == Some(query.namespace.as_str());
        let stored = state
            .metadata_store
            .object_metadata(&query.namespace, &query.bucket, key)
            .await?;
        let mut metadata = stored
            .user_metadata
            .into_iter()
            .map(|(key, value)| templates::ManagementMetadata { key, value })
            .collect::<Vec<_>>();
        if let Some(value) = stored.content_type {
            metadata.push(templates::ManagementMetadata {
                key: "Content-Type".to_string(),
                value,
            });
        }
        if let Some(value) = stored.content_length {
            metadata.push(templates::ManagementMetadata {
                key: "Content-Length".to_string(),
                value: value.to_string(),
            });
        }
        if let Some(value) = stored.etag {
            metadata.push(templates::ManagementMetadata {
                key: "ETag".to_string(),
                value,
            });
        }
        metadata.sort_by(|left, right| left.key.cmp(&right.key));
        let versions = crate::versioning::list_versions(
            &state.opendal_operator,
            &query.namespace,
            &query.bucket,
        )
        .await?
        .into_iter()
        .filter(|(object, _)| object == key)
        .map(|(_, version)| templates::ManagementVersion {
            id: version.version_id,
            created_at: version.created_at,
            delete_marker: version.is_delete_marker,
        })
        .collect();
        (Some(object_public), metadata, versions)
    } else {
        (None, Vec::new(), Vec::new())
    };
    Ok(templates::ManagementInspection {
        namespace: query.namespace,
        bucket: query.bucket,
        key: query.key,
        bucket_public,
        object_public,
        bucket_policy,
        versioning,
        metadata,
        versions,
    })
}

fn valid_bucket_name(name: &str) -> bool {
    (3..=63).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && name
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
}

fn valid_object_key(key: &str) -> bool {
    !key.contains('\\') && !key.contains('\0') && key.split('/').all(|segment| segment != "..")
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => internal_error(error, "failed to render management fragment"),
    }
}

fn refresh_dashboard(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("HX-Refresh", HeaderValue::from_static("true"));
    response
}

fn internal_error(error: impl std::fmt::Display, message: &str) -> Response {
    tracing::error!(%error, "{message}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn invalid_request(error: impl std::fmt::Display) -> Response {
    tracing::warn!(%error, "invalid management request");
    (StatusCode::BAD_REQUEST, error.to_string()).into_response()
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(config) = state.config.management.as_ref() else {
        return Err(StatusCode::NOT_FOUND.into_response());
    };
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(unauthorized());
    };
    let Some(encoded) = value.strip_prefix("Basic ") else {
        return Err(unauthorized());
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return Err(unauthorized());
    };
    let Ok(credentials) = std::str::from_utf8(&decoded) else {
        return Err(unauthorized());
    };
    let Some((username, password)) = credentials.split_once(':') else {
        return Err(unauthorized());
    };
    if username == config.username && password == config.password {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"s3-proxy management\"",
        )],
    )
        .into_response()
}
