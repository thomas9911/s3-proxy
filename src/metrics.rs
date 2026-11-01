use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

pub struct Metrics {
    http_requests: AtomicU64,
    http_errors: AtomicU64,
    metadata_operations: AtomicU64,
    metadata_duration_us: AtomicU64,
    storage_operations: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            http_requests: AtomicU64::new(0),
            http_errors: AtomicU64::new(0),
            metadata_operations: AtomicU64::new(0),
            metadata_duration_us: AtomicU64::new(0),
            storage_operations: AtomicU64::new(0),
        }
    }
}

fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::new)
}

pub fn record_http(status: u16) {
    let metrics = metrics();
    metrics.http_requests.fetch_add(1, Ordering::Relaxed);
    if status >= 400 {
        metrics.http_errors.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_metadata(duration_us: u64) {
    let metrics = metrics();
    metrics.metadata_operations.fetch_add(1, Ordering::Relaxed);
    metrics
        .metadata_duration_us
        .fetch_add(duration_us, Ordering::Relaxed);
}

pub fn record_storage() {
    metrics().storage_operations.fetch_add(1, Ordering::Relaxed);
}

pub fn render() -> String {
    let metrics = metrics();
    format!(
        "# TYPE s3_proxy_http_requests_total counter\n\
s3_proxy_http_requests_total {}\n\
# TYPE s3_proxy_http_errors_total counter\n\
s3_proxy_http_errors_total {}\n\
# TYPE s3_proxy_metadata_operations_total counter\n\
s3_proxy_metadata_operations_total {}\n\
# TYPE s3_proxy_metadata_duration_microseconds_total counter\n\
s3_proxy_metadata_duration_microseconds_total {}\n\
# TYPE s3_proxy_storage_operations_total counter\n\
s3_proxy_storage_operations_total {}\n",
        metrics.http_requests.load(Ordering::Relaxed),
        metrics.http_errors.load(Ordering::Relaxed),
        metrics.metadata_operations.load(Ordering::Relaxed),
        metrics.metadata_duration_us.load(Ordering::Relaxed),
        metrics.storage_operations.load(Ordering::Relaxed),
    )
}
