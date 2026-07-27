use std::future::Future;
use std::time::Duration;

const RETRY_ATTEMPTS: u64 = 3;

pub async fn retry<T, E, F, Fut>(operation: &'static str, mut action: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempts = 0;
    loop {
        attempts += 1;
        match action().await {
            Ok(value) => return Ok(value),
            Err(error) if attempts < RETRY_ATTEMPTS => {
                tracing::warn!(operation, attempts, "operation failed; retrying");
                tokio::time::sleep(Duration::from_millis(25 * attempts)).await;
                let _ = &error;
            }
            Err(error) => return Err(error),
        }
    }
}
