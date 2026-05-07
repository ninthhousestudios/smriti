//! Privacy gate — allowlist enforcement, path traversal prevention, and read audit logging.
//!
//! [`PrivacyGate`] is the single choke-point through which all file reads pass.
//! It guarantees:
//!
//! 1. The requested path resolves (via `std::fs::canonicalize`) to an absolute
//!    path under one of the configured allowlisted roots.
//! 2. The resolved path is not classified as Ignored or Cataloged by the global
//!    smritiignore rules.
//! 3. Every successful read is recorded in the `read_audit` table.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::Connection;
use tracing::warn;

use ignore::Match;

use crate::error::{Result, SmritiError};
use crate::hasher::hash_content;
use crate::ignore::SectionRules;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A file read that passed all privacy checks.
#[derive(Debug)]
pub struct ReadResult {
    pub content: Vec<u8>,
    pub content_hash: String,
}

/// Privacy gate: enforces the allowlist and ignore rules for every file read.
///
/// Roots are canonicalized once at construction time.  Per-call canonicalization
/// is done inside [`PrivacyGate::can_read`] to resolve symlinks and `..`
/// segments before the allowlist check.
pub struct PrivacyGate {
    /// Pre-canonicalized allowlisted roots.
    roots: Vec<PathBuf>,
    /// Global smritiignore rules (hardened defaults + optional user layer).
    global_rules: SectionRules,
}

impl PrivacyGate {
    /// Create a new gate.
    ///
    /// Roots that do not currently exist on disk are skipped with a `warn!`
    /// log (same policy as the scanner — an unmounted drive should not block
    /// boot).
    pub fn new(roots: Vec<PathBuf>, global_rules: SectionRules) -> Result<Self> {
        let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(roots.len());

        for root in roots {
            match std::fs::canonicalize(&root) {
                Ok(canonical) => canonical_roots.push(canonical),
                Err(e) => {
                    warn!(
                        root = %root.display(),
                        error = %e,
                        "privacy gate: root does not exist or cannot be canonicalized — skipping"
                    );
                }
            }
        }

        Ok(Self {
            roots: canonical_roots,
            global_rules,
        })
    }

    /// Validate that `path` may be read.
    ///
    /// On success returns the canonicalized path; on failure returns the
    /// appropriate `SmritiError`.
    ///
    /// Error ordering (matches plan spec):
    /// 1. `canonicalize` fails → `SmritiError::Io`
    /// 2. Not under any root → `SmritiError::OutsideAllowlist`
    /// 3. Ignored or Cataloged → `SmritiError::IgnoredPath`
    pub fn can_read(&self, path: &Path) -> Result<PathBuf> {
        // Step 1: Resolve symlinks and `..` segments.  This prevents traversal
        // attacks such as `<root>/../.ssh/id_rsa`.
        let canonical = std::fs::canonicalize(path).map_err(SmritiError::Io)?;

        // Step 2: Allowlist check — must be under at least one canonical root.
        let under_root = self.roots.iter().any(|root| canonical.starts_with(root));
        if !under_root {
            return Err(SmritiError::OutsideAllowlist {
                path: canonical.display().to_string(),
            });
        }

        // Step 3: Classification — Ignored and Cataloged paths are not readable
        // through the gate.  Classify directly against the global SectionRules
        // fields (which are public) to avoid needing IgnoreStack ownership.
        //
        // Priority: Ignored > Cataloged > Indexed/IndexedNoEmbed (tier 1).
        // "File is tier 1 (not cataloged) → else error" per the plan.
        let is_dir = canonical.is_dir();
        if matches!(
            self.global_rules.ignored.matched(&canonical, is_dir),
            Match::Ignore(_)
        ) {
            return Err(SmritiError::IgnoredPath {
                path: canonical.display().to_string(),
            });
        }
        if matches!(
            self.global_rules.cataloged.matched(&canonical, is_dir),
            Match::Ignore(_)
        ) {
            return Err(SmritiError::IgnoredPath {
                path: canonical.display().to_string(),
            });
        }

        Ok(canonical)
    }

    /// Record a successful read in the `read_audit` table in `audit.db`.
    pub fn log_read(
        &self,
        audit_conn: &Connection,
        path: &Path,
        content_hash: &str,
        caller: Option<&str>,
    ) -> Result<()> {
        let timestamp = Utc::now().to_rfc3339();
        let path_str = path.display().to_string();

        audit_conn.execute(
            "INSERT INTO read_audit (path, content_hash, timestamp, caller) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![path_str, content_hash, timestamp, caller],
        )?;

        Ok(())
    }

    /// Validate, read, hash, log, and return a file's contents.
    ///
    /// The `audit_conn` is the writable connection to `audit.db` for logging.
    /// The index `conn` is not used here but callers may need it for lookups.
    pub fn read_file(
        &self,
        audit_conn: &Connection,
        path: &Path,
        caller: Option<&str>,
    ) -> Result<ReadResult> {
        let canonical = self.can_read(path)?;

        let content = std::fs::read(&canonical).map_err(SmritiError::Io)?;
        let content_hash = hash_content(&content);

        self.log_read(audit_conn, &canonical, &content_hash, caller)?;

        Ok(ReadResult {
            content,
            content_hash,
        })
    }
}
