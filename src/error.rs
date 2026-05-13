use thiserror::Error;

pub const INDEX_CORRUPT_EXIT_STATUS: i32 = 78;

#[derive(Debug, Error)]
pub enum SmritiError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("index corrupt: {message}")]
    IndexCorrupt { message: String },

    #[error("migration error: {message}")]
    Migration { message: String },

    #[error("config error: {var} — {message}")]
    Config { var: String, message: String },

    #[error("no roots configured — set SMRITI_ROOTS or run `smriti roots add`")]
    NoRoots,

    #[error("path {path} is outside allowlisted roots")]
    OutsideAllowlist { path: String },

    #[error("path {path} matches ignore pattern")]
    IgnoredPath { path: String },

    #[error("not found: {entity} {id}")]
    NotFound { entity: String, id: String },

    #[error("scan in progress")]
    ScanInProgress,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl SmritiError {
    pub fn from_db_context(err: rusqlite::Error, context: &str) -> Self {
        if is_sqlite_corruption(&err) {
            Self::IndexCorrupt {
                message: format!("{context}: {err}"),
            }
        } else {
            Self::Db(err)
        }
    }

    pub fn is_index_corrupt(&self) -> bool {
        match self {
            Self::IndexCorrupt { .. } => true,
            Self::Db(err) => is_sqlite_corruption(err),
            _ => false,
        }
    }

    pub fn repair_hint(&self) -> Option<&'static str> {
        self.is_index_corrupt().then_some(
            "Stop smriti services, move or delete ~/.smriti/index.db*, then restart smriti-watch to rebuild the index.",
        )
    }

    pub fn next_action(&self) -> &'static str {
        match self {
            Self::Db(err) if is_sqlite_corruption(err) => "Stop smriti services, move or delete ~/.smriti/index.db*, then restart smriti-watch to rebuild the index.",
            Self::Db(_) => "Check the database file at SMRITI_DB_PATH; run `smriti health` for diagnostics.",
            Self::IndexCorrupt { .. } => "Stop smriti services, move or delete ~/.smriti/index.db*, then restart smriti-watch to rebuild the index.",
            Self::Migration { .. } => "Inspect the migration file at migrations/0001_initial.sql and ensure the database schema is consistent.",
            Self::Config { .. } => "Set the missing environment variable or check ~/.smriti/ for config files.",
            Self::NoRoots => "Run `smriti roots add <path>` or set SMRITI_ROOTS=<colon-separated paths>.",
            Self::OutsideAllowlist { .. } => "Add the root with `smriti roots add <path>` before accessing files under it.",
            Self::IgnoredPath { .. } => "Remove the matching pattern from ~/.smriti/ignore or add a negation pattern.",
            Self::NotFound { .. } => "Verify the identifier is correct; run `smriti manifest` to list all tracked files.",
            Self::ScanInProgress => "Wait for the current scan to complete; check progress via `smriti health`.",
            Self::Io(_) => "Check file permissions and that the path exists.",
            Self::Other(_) => "Check server logs for additional context.",
        }
    }
}

pub fn is_sqlite_corruption(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(sqlite_err, msg) => {
            sqlite_err.code == rusqlite::ErrorCode::DatabaseCorrupt
                || matches!(sqlite_err.extended_code, 267)
                || msg
                    .as_deref()
                    .map(message_mentions_corruption)
                    .unwrap_or(false)
        }
        _ => message_mentions_corruption(&err.to_string()),
    }
}

fn message_mentions_corruption(message: &str) -> bool {
    message.contains("database disk image is malformed")
        || message.contains("Content in the virtual table is corrupt")
        || message.contains("database corruption")
}

pub type Result<T, E = SmritiError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi;

    #[test]
    fn classifies_sqlite_corrupt_vtab() {
        let err = rusqlite::Error::SqliteFailure(
            ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt,
                extended_code: 267,
            },
            Some("Content in the virtual table is corrupt".to_string()),
        );

        assert!(is_sqlite_corruption(&err));
        let err = SmritiError::from_db_context(err, "fts health probe");
        assert!(err.is_index_corrupt());
        assert!(err.repair_hint().unwrap().contains("index.db*"));
    }
}
