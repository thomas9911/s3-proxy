use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_EVENTS: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct AuditEvent {
    pub timestamp: u64,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub elapsed_us: u64,
}

pub fn record(method: String, path: String, status: u16, elapsed_us: u64) {
    let mut events = events().lock().expect("audit event lock poisoned");
    events.push_back(AuditEvent {
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        method,
        path,
        status,
        elapsed_us,
    });
    if events.len() > MAX_EVENTS {
        events.pop_front();
    }
}

pub fn recent(limit: usize) -> Vec<AuditEvent> {
    events()
        .lock()
        .expect("audit event lock poisoned")
        .iter()
        .rev()
        .take(limit.min(MAX_EVENTS))
        .cloned()
        .collect()
}

fn events() -> &'static Mutex<VecDeque<AuditEvent>> {
    static EVENTS: OnceLock<Mutex<VecDeque<AuditEvent>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_EVENTS)))
}
