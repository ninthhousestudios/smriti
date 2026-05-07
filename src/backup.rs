use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::expand_tilde;
use crate::error::{Result, SmritiError};

pub struct RedundantFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub also_at: Vec<PathBuf>,
}

pub struct UniqueFile {
    pub path: PathBuf,
    pub size_bytes: u64,
}

pub struct StaleFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub newer_path: PathBuf,
    pub target_mtime: i64,
    pub other_mtime: i64,
}

pub struct BackupAuditReport {
    pub target_root: PathBuf,
    pub redundant: Vec<RedundantFile>,
    pub unique: Vec<UniqueFile>,
    pub stale: Vec<StaleFile>,
    pub total_files: u64,
    pub total_bytes: u64,
    pub redundant_bytes: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub enum AuditAction {
    Redundant,
    Keep,
}

impl AuditAction {
    fn as_str(&self) -> &'static str {
        match self {
            AuditAction::Redundant => "redundant",
            AuditAction::Keep => "keep",
        }
    }
}

impl std::str::FromStr for AuditAction {
    type Err = SmritiError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "redundant" => Ok(AuditAction::Redundant),
            "keep" => Ok(AuditAction::Keep),
            other => Err(SmritiError::Other(format!("unknown action: {other}"))),
        }
    }
}

pub fn analyze(conn: &Connection, target_root: &Path) -> Result<BackupAuditReport> {
    let target_root_str = target_root.to_string_lossy();

    let (total_files, total_bytes) = {
        let mut stmt = conn.prepare(
            "SELECT COUNT(*), COALESCE(SUM(d.byte_size), 0) \
             FROM paths p \
             JOIN documents d ON d.content_hash = p.content_hash \
             WHERE p.disappeared IS NULL \
               AND p.root = ?1",
        )?;
        stmt.query_row([target_root_str.as_ref()], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?))
        })?
    };

    struct TargetFile {
        path: String,
        content_hash: String,
        size_bytes: u64,
        mtime: i64,
    }

    let target_files: Vec<TargetFile> = {
        let mut stmt = conn.prepare(
            "SELECT p.path, p.content_hash, d.byte_size, p.mtime \
             FROM paths p \
             JOIN documents d ON d.content_hash = p.content_hash \
             WHERE p.disappeared IS NULL \
               AND p.root = ?1",
        )?;
        let mut rows = stmt.query([target_root_str.as_ref()])?;
        let mut files = Vec::new();
        while let Some(row) = rows.next()? {
            files.push(TargetFile {
                path: row.get(0)?,
                content_hash: row.get(1)?,
                size_bytes: row.get(2)?,
                mtime: row.get(3)?,
            });
        }
        files
    };

    let mut redundant: Vec<RedundantFile> = Vec::new();
    let mut unique: Vec<UniqueFile> = Vec::new();
    let mut stale: Vec<StaleFile> = Vec::new();

    let mut other_copies_stmt = conn.prepare(
        "SELECT p.path \
         FROM paths p \
         WHERE p.content_hash = ?1 \
           AND p.root != ?2 \
           AND p.disappeared IS NULL",
    )?;

    // Match by relative path suffix on other roots: looks for paths ending with
    // '/<relative-path>' where relative-path is the file's path with target_root stripped.
    // This is a heuristic — same relative path ≠ same file, but it's a useful signal.
    let mut stale_stmt = conn.prepare(
        "SELECT p.path, p.mtime \
         FROM paths p \
         WHERE p.path LIKE ?1 \
           AND p.root != ?2 \
           AND p.content_hash != ?3 \
           AND p.disappeared IS NULL \
         ORDER BY p.mtime DESC \
         LIMIT 1",
    )?;

    for tf in &target_files {
        let other_copies: Vec<PathBuf> = {
            let mut rows = other_copies_stmt.query(
                rusqlite::params![tf.content_hash, target_root_str.as_ref()],
            )?;
            let mut paths = Vec::new();
            while let Some(row) = rows.next()? {
                let p: String = row.get(0)?;
                paths.push(PathBuf::from(p));
            }
            paths
        };

        if !other_copies.is_empty() {
            redundant.push(RedundantFile {
                path: PathBuf::from(&tf.path),
                size_bytes: tf.size_bytes,
                also_at: other_copies,
            });
            continue;
        }

        // Stale check: find the relative portion of this path (strip root prefix),
        // then look for that same suffix under other roots with a different hash and
        // a newer mtime.
        let stale_found = if let Ok(rel) = Path::new(&tf.path).strip_prefix(target_root) {
            if let Some(rel_str) = rel.to_str() {
                // LIKE pattern: any path that ends with /<rel_str>
                let like_pattern = format!("%/{rel_str}");
                let mut rows = stale_stmt.query(rusqlite::params![
                    like_pattern,
                    target_root_str.as_ref(),
                    tf.content_hash,
                ])?;
                if let Some(row) = rows.next()? {
                    let other_path: String = row.get(0)?;
                    let other_mtime: i64 = row.get(1)?;
                    if other_mtime > tf.mtime {
                        stale.push(StaleFile {
                            path: PathBuf::from(&tf.path),
                            size_bytes: tf.size_bytes,
                            newer_path: PathBuf::from(other_path),
                            target_mtime: tf.mtime,
                            other_mtime,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if !stale_found {
            unique.push(UniqueFile {
                path: PathBuf::from(&tf.path),
                size_bytes: tf.size_bytes,
            });
        }
    }

    let redundant_bytes: u64 = redundant.iter().map(|f| f.size_bytes).sum();

    redundant.sort_by_key(|f| std::cmp::Reverse(f.size_bytes));
    unique.sort_by_key(|f| std::cmp::Reverse(f.size_bytes));
    stale.sort_by_key(|f| std::cmp::Reverse(f.size_bytes));

    Ok(BackupAuditReport {
        target_root: target_root.to_path_buf(),
        redundant,
        unique,
        stale,
        total_files,
        total_bytes,
        redundant_bytes,
    })
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn path_display(path: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::new());
    let s = path.to_string_lossy();
    if !home.is_empty() && s.starts_with(&home) {
        format!("~{}", &s[home.len()..])
    } else {
        s.into_owned()
    }
}

pub fn format_audit_file(report: &BackupAuditReport) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    let root_display = path_display(&report.target_root);
    let mut out = String::new();

    let _ = writeln!(out, "# smriti backup-audit — {root_display} — {date}");
    let _ = writeln!(out, "# Edit the ACTION column. Save and close to see summary.");
    let _ = writeln!(out, "#");
    let _ = writeln!(out, "# Actions:  redundant = mark for cleanup  |  keep = no change");
    let _ = writeln!(out, "#");
    let _ = writeln!(
        out,
        "# Total: {} files, {} ({} redundant — {})",
        report.total_files,
        format_bytes(report.total_bytes),
        report.redundant.len(),
        format_bytes(report.redundant_bytes),
    );

    if !report.redundant.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "# REDUNDANT — same content exists on live roots (safe to delete from backup)"
        );
        let _ = writeln!(
            out,
            "# {:<10}  {:<50}  {:<10}  ALSO AT",
            "ACTION", "PATH", "SIZE"
        );
        let _ = writeln!(out);

        for f in &report.redundant {
            let path_str = path_display(&f.path);
            let size_str = format_bytes(f.size_bytes);
            let also_at = f
                .also_at
                .iter()
                .map(|p| path_display(p))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "{:<10}  {:<50}  {:<10}  {}",
                AuditAction::Redundant.as_str(),
                path_str,
                size_str,
                also_at,
            );
        }
    }

    if !report.unique.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "# UNIQUE — exists only on this root");
        let _ = writeln!(out, "# (no action needed — listed for awareness)");
        let _ = writeln!(out, "# {:<10}  {:<50}  SIZE", "ACTION", "PATH");
        let _ = writeln!(out);

        for f in &report.unique {
            let path_str = path_display(&f.path);
            let size_str = format_bytes(f.size_bytes);
            let _ = writeln!(out, "{:<10}  {:<50}  {}", "", path_str, size_str);
        }
    }

    if !report.stale.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "# STALE — same relative path exists elsewhere with newer content"
        );
        let _ = writeln!(
            out,
            "# {:<10}  {:<50}  {:<10}  {:<50}  {:<12}  LIVE DATE",
            "ACTION", "PATH", "SIZE", "NEWER AT", "TARGET DATE"
        );
        let _ = writeln!(out);

        for f in &report.stale {
            let path_str = path_display(&f.path);
            let size_str = format_bytes(f.size_bytes);
            let newer_str = path_display(&f.newer_path);
            let target_date = format_mtime(f.target_mtime);
            let other_date = format_mtime(f.other_mtime);
            let _ = writeln!(
                out,
                "{:<10}  {:<50}  {:<10}  {:<50}  {:<12}  {}",
                AuditAction::Keep.as_str(),
                path_str,
                size_str,
                newer_str,
                target_date,
                other_date,
            );
        }
    }

    out
}

fn format_mtime(mtime: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let secs = if mtime < 0 { 0u64 } else { mtime as u64 };
    let system_time = UNIX_EPOCH + Duration::from_secs(secs);
    let dt: chrono::DateTime<chrono::Local> = system_time.into();
    dt.format("%Y-%m-%d").to_string()
}

pub fn parse_audit_file(content: &str) -> Result<Vec<(AuditAction, PathBuf)>> {
    let mut decisions = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let action_str = match tokens.next() {
            Some(s) => s,
            None => continue,
        };
        let action: AuditAction = match action_str.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let path_str = match tokens.next() {
            Some(s) => s,
            None => continue,
        };
        if path_str.is_empty() {
            continue;
        }
        let path = expand_tilde(path_str);
        decisions.push((action, path));
    }
    Ok(decisions)
}

pub fn apply_audit(decisions: &[(AuditAction, PathBuf)]) -> ApplyResult {
    let redundant: Vec<&PathBuf> = decisions
        .iter()
        .filter(|(a, _)| *a == AuditAction::Redundant)
        .map(|(_, p)| p)
        .collect();

    let kept: Vec<&PathBuf> = decisions
        .iter()
        .filter(|(a, _)| *a == AuditAction::Keep)
        .map(|(_, p)| p)
        .collect();

    let mut messages = Vec::new();

    if redundant.is_empty() {
        messages.push("No files marked redundant.".to_string());
    } else {
        messages.push(format!(
            "{} file(s) marked redundant (not deleted — v1 is report-only):",
            redundant.len()
        ));
        for p in &redundant {
            messages.push(format!("  would delete: {}", path_display(p)));
        }
    }

    if !kept.is_empty() {
        messages.push(format!("{} file(s) kept.", kept.len()));
    }

    ApplyResult {
        redundant_count: redundant.len(),
        kept_count: kept.len(),
        messages,
    }
}

pub struct ApplyResult {
    pub redundant_count: usize,
    pub kept_count: usize,
    pub messages: Vec<String>,
}
