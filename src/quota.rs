use opendal::Operator;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct QuotaConfig {
    pub max_storage_bytes: Option<u64>,
    pub max_requests_per_minute: Option<u64>,
    #[serde(default)]
    pub principals: HashMap<String, PrincipalQuotaConfig>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct PrincipalQuotaConfig {
    pub max_storage_bytes: Option<u64>,
    pub max_requests_per_minute: Option<u64>,
}

pub fn allow_request(config: &QuotaConfig, principal_id: &str) -> bool {
    let Some(limit) = quota_for(config, principal_id).max_requests_per_minute else {
        return true;
    };
    let mut windows = request_windows()
        .lock()
        .expect("quota request window lock poisoned");
    let window = windows
        .entry(principal_id.to_string())
        .or_insert_with(RequestWindow::new);
    if window.started.elapsed() >= Duration::from_secs(60) {
        *window = RequestWindow::new();
    }
    if window.requests >= limit {
        return false;
    }
    window.requests += 1;
    true
}

pub async fn allows_storage(
    config: &QuotaConfig,
    operator: &Operator,
    principal_id: &str,
    replacing_path: Option<&str>,
    new_size: u64,
) -> anyhow::Result<bool> {
    let Some(limit) = quota_for(config, principal_id).max_storage_bytes else {
        return Ok(true);
    };
    let current_size = namespace_size(operator, principal_id).await?;
    let replaced_size = match replacing_path {
        Some(path) => operator
            .stat(path)
            .await
            .map(|metadata| metadata.content_length())
            .unwrap_or(0),
        None => 0,
    };
    Ok(current_size
        .saturating_sub(replaced_size)
        .saturating_add(new_size)
        <= limit)
}

pub async fn namespace_size(operator: &Operator, principal_id: &str) -> anyhow::Result<u64> {
    let prefix = format!("{principal_id}/");
    if !operator.exists(&prefix).await? {
        return Ok(0);
    }
    let mut lister = operator.lister_with(&prefix).recursive(true).await?;
    let mut size = 0;
    use tokio_stream::StreamExt;
    while let Some(entry) = lister.next().await {
        let entry = entry?;
        if entry.metadata().is_file() {
            size += entry.metadata().content_length();
        }
    }
    Ok(size)
}

struct RequestWindow {
    started: Instant,
    requests: u64,
}

impl RequestWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            requests: 0,
        }
    }
}

fn request_windows() -> &'static Mutex<HashMap<String, RequestWindow>> {
    static WINDOWS: OnceLock<Mutex<HashMap<String, RequestWindow>>> = OnceLock::new();
    WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_overrides() -> &'static Mutex<HashMap<String, PrincipalQuotaConfig>> {
    static OVERRIDES: OnceLock<Mutex<HashMap<String, PrincipalQuotaConfig>>> = OnceLock::new();
    OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn quota_for(config: &QuotaConfig, principal_id: &str) -> PrincipalQuotaConfig {
    if let Some(quota) = runtime_overrides()
        .lock()
        .expect("quota override lock poisoned")
        .get(principal_id)
        .cloned()
    {
        return quota;
    }
    config
        .principals
        .get(principal_id)
        .cloned()
        .unwrap_or(PrincipalQuotaConfig {
            max_storage_bytes: config.max_storage_bytes,
            max_requests_per_minute: config.max_requests_per_minute,
        })
}

pub fn set_runtime_quota(principal_id: String, quota: PrincipalQuotaConfig) {
    runtime_overrides()
        .lock()
        .expect("quota override lock poisoned")
        .insert(principal_id, quota);
}

pub fn request_usage(principal_id: &str) -> (u64, u64) {
    let windows = request_windows()
        .lock()
        .expect("quota request window lock poisoned");
    let Some(window) = windows.get(principal_id) else {
        return (0, 60);
    };
    let elapsed = window.started.elapsed().as_secs().min(60);
    (window.requests, 60 - elapsed)
}

#[cfg(test)]
mod tests {
    use super::{allow_request, allows_storage, QuotaConfig};
    use opendal::services::Memory;
    use opendal::Operator;

    #[test]
    fn limits_each_principal_independently() {
        let config = QuotaConfig {
            max_requests_per_minute: Some(1),
            ..Default::default()
        };
        assert!(allow_request(&config, "principal-a"));
        assert!(!allow_request(&config, "principal-a"));
        assert!(allow_request(&config, "principal-b"));
    }

    #[tokio::test]
    async fn counts_storage_per_principal() {
        let operator = Operator::new(Memory::default()).unwrap();
        operator.create_dir("principal-a/bucket/").await.unwrap();
        operator
            .write("principal-a/bucket/object", b"1234".to_vec())
            .await
            .unwrap();
        let config = QuotaConfig {
            max_storage_bytes: Some(5),
            ..Default::default()
        };
        assert!(!allows_storage(&config, &operator, "principal-a", None, 2,)
            .await
            .unwrap());
        assert!(allows_storage(&config, &operator, "principal-b", None, 5)
            .await
            .unwrap());
    }
}
