use chrono::{DateTime, Utc};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FreshnessEnvelope {
    pub as_of: DateTime<Utc>,
    pub is_stale: bool,
}

impl FreshnessEnvelope {
    pub fn new(last_scan: DateTime<Utc>, stale_threshold_sec: u64) -> Self {
        let age = Utc::now().signed_duration_since(last_scan);
        let is_stale = age.num_seconds() > stale_threshold_sec as i64;
        Self { as_of: last_scan, is_stale }
    }
}
