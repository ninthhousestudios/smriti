use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::search;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FreshnessEnvelope {
    pub as_of: DateTime<Utc>,
    pub is_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

impl FreshnessEnvelope {
    pub fn new(last_scan: DateTime<Utc>, stale_threshold_sec: u64) -> Self {
        let age = Utc::now().signed_duration_since(last_scan);
        let is_stale = age.num_seconds() > stale_threshold_sec as i64;
        Self { as_of: last_scan, is_stale, stale_reason: None }
    }

    pub fn from_watcher(conn: &Connection) -> Self {
        let now = Utc::now();
        match search::read_watcher_status(conn) {
            Ok(Some(ws)) if !ws.running => Self {
                as_of: now,
                is_stale: true,
                stale_reason: Some(format!("watcher not running (state: {}, last update: {})", ws.state, ws.updated_at)),
            },
            Ok(None) => Self {
                as_of: now,
                is_stale: true,
                stale_reason: Some("watcher has never run".to_string()),
            },
            _ => Self { as_of: now, is_stale: false, stale_reason: None },
        }
    }
}
