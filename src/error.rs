use thiserror::Error;

#[derive(Debug, Error)]
pub enum SmritiError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

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
    pub fn next_action(&self) -> &'static str {
        match self {
            Self::Db(_) => "Check the database file at SMRITI_DB_PATH; run `smriti health` for diagnostics.",
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

pub type Result<T, E = SmritiError> = std::result::Result<T, E>;
