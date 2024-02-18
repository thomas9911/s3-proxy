use crate::signature::VerifiedRequest;
use crate::{templates, AppState};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum_route_error::RouteError;
use tokio_stream::StreamExt;

pub async fn list_buckets(
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = &signature.namespace;

    // let bucket = "testing";

    // opendal_operator
    //     .write(
    //         &format!("{}/{}/testing.bin", namespace, bucket),
    //         vec![0; 4096],
    //     )
    //     .await?;

    let mut lister = opendal_operator
        .lister_with(&format!("{}/", namespace))
        .await?;

    let mut buckets = Vec::new();
    while let Some(entry) = lister.next().await {
        match entry {
            Ok(x) => {
                if x.metadata().is_dir() {
                    buckets.push(templates::ListBucketItem {
                        name: x.name().trim_end_matches('/').to_string().into(),
                        timestamp: None,
                    })
                }
            }
            Err(e) => {
                tracing::error!("{}", e.to_string());
                return Err(RouteError::new_internal_server());
            }
        }
    }

    // let datetime = OffsetDateTime::from_unix_timestamp(1706911595)?;
    // let tmp_timestamp = datetime.format(&Rfc3339).unwrap();

    let template = templates::ListBucketsTemplate {
        owner_name: "Testing",
        owner_id: "1",
        // buckets: vec![
        //     templates::ListBucketItem {
        //         name: "testing1".into(),
        //         timestamp: Some(tmp_timestamp.into()),
        //     },
        //     templates::ListBucketItem {
        //         name: "testing2".into(),
        //         timestamp: None,
        //     },
        // ],
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
) -> Result<impl IntoResponse, RouteError> {
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

pub async fn create_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    header_map: HeaderMap,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;

    if opendal_operator
        .is_exist(&format!("{}/{}", namespace, bucket_name))
        .await?
    {
        return Ok((StatusCode::NOT_FOUND, "NOT FOUND").into_response());
    }

    let mut writer = opendal_operator.write_with(
        &format!("{}/{}/{}", namespace, bucket_name, object_name),
        signature.bytes,
    );

    writer = if let Some(content_type) = header_map.get(CONTENT_TYPE) {
        if let Ok(content_type) = content_type.to_str() {
            writer.content_type(content_type)
        } else {
            writer
        }
    } else {
        writer
    };

    writer.await?;

    Ok("OK".into_response())
}

pub async fn get_object(
    Path((bucket_name, object_name)): Path<(String, String)>,
    State(AppState {
        opendal_operator, ..
    }): State<AppState>,
    signature: VerifiedRequest,
) -> Result<impl IntoResponse, RouteError> {
    let namespace = signature.namespace;

    if opendal_operator
        .is_exist(&format!("{}/{}", namespace, bucket_name))
        .await?
    {
        return Ok((StatusCode::NOT_FOUND, "NOT FOUND").into_response());
    }

    let filepath = format!("{}/{}/{}", namespace, bucket_name, object_name);
    let metadata = if let Ok(metadata) = opendal_operator.stat(&filepath).await {
        metadata
    } else {
        // maybe actually check if the error is not found :D
        return Ok((StatusCode::NOT_FOUND, "NOT FOUND").into_response());
    };

    let reader = opendal_operator.reader(&filepath).await?;

    let mut response_headers = HeaderMap::new();

    if let Some(content_type) = metadata.content_type() {
        response_headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type)?);
    }

    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.content_length().to_string())?,
    );

    Ok((response_headers, Body::from_stream(reader)).into_response())
}
