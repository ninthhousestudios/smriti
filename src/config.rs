use std::path::PathBuf;

use crate::error::{Result, SmritiError};

#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub roots: Vec<PathBuf>,
    pub model_path: Option<PathBuf>,
    pub listen_addr: String,
    pub stale_threshold_sec: u64,
    pub fts_content_max_bytes: u64,
    pub max_metadata_bytes: u64,
    pub audit_retention_days: u64,
    pub scan_batch_size: usize,
    pub full_scan_interval_sec: u64,
    pub shutdown_drain_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let db_path = std::env::var("SMRITI_DB_PATH")
            .map(|s| expand_tilde(&s))
            .unwrap_or_else(|_| default_smriti_dir().join("index.db"));

        let roots: Vec<PathBuf> = std::env::var("SMRITI_ROOTS")
            .map(|s| {
                s.split(':')
                    .filter(|p| !p.is_empty())
                    .map(expand_tilde)
                    .collect()
            })
            .unwrap_or_default();

        let model_path = std::env::var("SMRITI_MODEL_PATH")
            .ok()
            .map(|s| expand_tilde(&s));

        let listen_addr = std::env::var("SMRITI_LISTEN_ADDR").unwrap_or_else(|_| {
            let sock = default_smriti_dir().join("sock");
            format!("unix:{}", sock.display())
        });

        let stale_threshold_sec = parse_env_or("SMRITI_STALE_THRESHOLD_SEC", 3600u64)?;
        let fts_content_max_bytes = parse_env_or("SMRITI_FTS_CONTENT_MAX_BYTES", 102400u64)?;
        let max_metadata_bytes = parse_env_or("SMRITI_MAX_METADATA_BYTES", 524288000u64)?;
        let audit_retention_days = parse_env_or("SMRITI_AUDIT_RETENTION_DAYS", 30u64)?;
        let scan_batch_size = parse_env_or("SMRITI_SCAN_BATCH_SIZE", 2000usize)?;
        let full_scan_interval_sec = parse_env_or("SMRITI_WATCH_FULL_SCAN_INTERVAL", 86400u64)?;
        let shutdown_drain_ms = parse_env_or("SMRITI_WATCH_SHUTDOWN_DRAIN_MS", 10000u64)?;

        Ok(Self {
            db_path,
            roots,
            model_path,
            listen_addr,
            stale_threshold_sec,
            fts_content_max_bytes,
            max_metadata_bytes,
            audit_retention_days,
            scan_batch_size,
            full_scan_interval_sec,
            shutdown_drain_ms,
        })
    }
}

fn default_smriti_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".smriti")
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(rest)
    } else if path == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
    } else {
        PathBuf::from(path)
    }
}

fn parse_env_or<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr + Copy,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) => v.parse::<T>().map_err(|e| SmritiError::Config {
            var: name.to_string(),
            message: format!("expected a valid value, got {v:?}: {e}"),
        }),
    }
}
