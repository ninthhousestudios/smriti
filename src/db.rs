use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use rusqlite::Connection;

use crate::error::{Result, SmritiError};

pub fn open(path: &Path) -> Result<Connection> {
    let conn = open_connection(path)?;
    run_migrations(&conn)?;
    Ok(conn)
}

pub fn open_readonly(path: &Path) -> Result<Connection> {
    let conn = open_connection(path)?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

pub fn probe_index_health(conn: &Connection) -> Result<()> {
    probe_base_tables(conn)?;
    probe_fts(conn)
}

pub fn probe_fts(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("SELECT rowid FROM document_fts WHERE document_fts MATCH ?1 LIMIT 1")
        .map_err(|e| SmritiError::from_db_context(e, "prepare FTS health probe"))?;
    let mut rows = stmt
        .query(["\"__smriti_health_probe_no_match__\""])
        .map_err(|e| SmritiError::from_db_context(e, "run FTS health probe"))?;
    while rows
        .next()
        .map_err(|e| SmritiError::from_db_context(e, "read FTS health probe"))?
        .is_some()
    {}
    Ok(())
}

fn probe_base_tables(conn: &Connection) -> Result<()> {
    conn.query_row("SELECT 1 FROM documents LIMIT 1", [], |_| Ok(()))
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(()),
            other => Err(SmritiError::from_db_context(other, "probe documents table")),
        })?;
    conn.query_row("SELECT 1 FROM paths LIMIT 1", [], |_| Ok(()))
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(()),
            other => Err(SmritiError::from_db_context(other, "probe paths table")),
        })?;
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = if path.as_os_str() == ":memory:" {
        Connection::open_in_memory()?
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Connection::open(path)?
    };

    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    Ok(conn)
}

/// Checkpoint and truncate the WAL. Call before write-heavy operations (scan)
/// to prevent stale WAL frames from causing SIGBUS. Not needed for read-only
/// commands — the exclusive lock it requires would contend with concurrent scans.
pub fn checkpoint_wal(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
}

/// Passive WAL checkpoint — doesn't block readers or writers.
pub fn checkpoint_wal_passive(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
    Ok(())
}

pub fn enable_scan_pragmas(conn: &Connection) -> Result<()> {
    set_pragma(conn, "synchronous", "NORMAL")?;
    // 256 MB page cache: hot working set (FTS5 index, paths index, documents)
    // exceeds the previous 64 MB once the DB grows past a few hundred MB.
    set_pragma(conn, "cache_size", "-262144")?;
    // mmap disabled on the writer: leave page-cache as the sole hot path.
    // Re-enable cautiously — see the SIGBUS that was traced to two writer
    // connections sharing the wal-index SHM mmap (smriti-serve held an rw
    // connection while `smriti scan` ran). The fix moved smriti-serve to
    // open_readonly + lazy-writer-per-scan; mmap on the writer is now safe
    // in principle but the 256 MB page cache is enough for the working set.
    set_pragma(conn, "mmap_size", "0")?;
    set_pragma(conn, "temp_store", "2")?;
    // Leave wal_autocheckpoint at the default (1000 pages ~= 4 MB). Disabling
    // it caused the WAL to grow to 700+ MB during long scans, and every insert
    // had to walk an ever-larger frame index — the scan got progressively
    // slower per batch. Default autocheckpoint runs PASSIVE in-line on the
    // writer connection, which is what we want.
    Ok(())
}

pub fn restore_default_pragmas(conn: &Connection) -> Result<()> {
    set_pragma(conn, "synchronous", "FULL")?;
    set_pragma(conn, "cache_size", "-2000")?;
    set_pragma(conn, "mmap_size", "0")?;
    set_pragma(conn, "temp_store", "0")?;
    set_pragma(conn, "wal_autocheckpoint", "1000")?;
    Ok(())
}

fn set_pragma(conn: &Connection, name: &str, value: &str) -> Result<()> {
    let sql = format!("PRAGMA {name} = {value}");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

pub fn run_migrations(conn: &Connection) -> Result<()> {
    let sql = include_str!("../migrations/0001_initial.sql");
    conn.execute_batch(sql)
        .map_err(|e| SmritiError::Migration {
            message: e.to_string(),
        })?;

    // 0002: scan generations — ALTER TABLE ADD COLUMN is not idempotent,
    // so check whether the column exists before running the migration.
    let has_last_seen: bool = conn
        .prepare("SELECT last_seen_scan FROM paths LIMIT 0")
        .is_ok();
    if !has_last_seen {
        let sql = include_str!("../migrations/0002_scan_generations.sql");
        conn.execute_batch(sql)
            .map_err(|e| SmritiError::Migration {
                message: format!("0002_scan_generations: {e}"),
            })?;
    } else {
        // Columns exist; still ensure the table and indexes are present
        // (CREATE TABLE/INDEX IF NOT EXISTS are idempotent).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scan_runs (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at  TIMESTAMP NOT NULL,
                finished_at TIMESTAMP,
                status      TEXT NOT NULL CHECK (status IN ('running', 'complete', 'failed')),
                files_seen  INTEGER NOT NULL DEFAULT 0,
                error       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_paths_last_seen ON paths(last_seen_scan)
                WHERE disappeared IS NULL;
            CREATE INDEX IF NOT EXISTS idx_events_scan_id ON events(scan_id);",
        )
        .map_err(|e| SmritiError::Migration {
            message: format!("0002_scan_generations (idempotent): {e}"),
        })?;
    }

    // 0003: watcher tables — all CREATE TABLE/INDEX IF NOT EXISTS, inherently idempotent.
    let sql = include_str!("../migrations/0003_watcher_tables.sql");
    conn.execute_batch(sql)
        .map_err(|e| SmritiError::Migration {
            message: format!("0003_watcher_tables: {e}"),
        })?;

    // 0005: add 'stopping' and 'reconciling' states to watcher_heartbeat.
    let has_new_states: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='watcher_heartbeat'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|sql| sql.contains("stopping"))
        .unwrap_or(false);
    if !has_new_states {
        let sql = include_str!("../migrations/0005_heartbeat_states.sql");
        conn.execute_batch(sql)
            .map_err(|e| SmritiError::Migration {
                message: format!("0005_heartbeat_states: {e}"),
            })?;
    }

    // 0004: drop read_audit from index.db (moved to audit.db).
    let has_read_audit: bool = conn.prepare("SELECT 1 FROM read_audit LIMIT 0").is_ok();
    if has_read_audit {
        let sql = include_str!("../migrations/0004_drop_read_audit.sql");
        conn.execute_batch(sql)
            .map_err(|e| SmritiError::Migration {
                message: format!("0004_drop_read_audit: {e}"),
            })?;
    }

    Ok(())
}

pub fn acquire_writer_lock(db_path: &Path) -> Result<File> {
    use std::os::unix::io::AsRawFd;

    let lock_path = writer_lock_path(db_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(&lock_path)?;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        return Err(SmritiError::Other(format!(
            "Another smriti writer holds the lock ({}). Only one writer process is allowed.",
            lock_path.display()
        )));
    }
    Ok(file)
}

fn writer_lock_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("writer.lock")
}

pub fn open_audit(db_dir: &Path) -> Result<Connection> {
    let path = db_dir.join("audit.db");
    let conn = open_connection(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS read_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            content_hash TEXT,
            timestamp TIMESTAMP NOT NULL,
            caller TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_read_audit_ts ON read_audit(timestamp);",
    )
    .map_err(|e| SmritiError::Migration {
        message: format!("audit.db init: {e}"),
    })?;
    Ok(conn)
}

pub fn enqueue_scan(conn: &Connection, kind: &str, root: Option<&str>) -> Result<i64> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "INSERT INTO scan_requests (requested_at, kind, root) VALUES (?1, ?2, ?3)",
        rusqlite::params![now_str, kind, root],
    )?;
    let id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))?;
    Ok(id)
}

/// Write-capability wrapper that exposes only scan-request INSERTs.
/// Enforces ADR 0001's single-writer invariant at the type level.
pub struct ScanEnqueuer(Connection);

impl ScanEnqueuer {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self(open_connection(path)?))
    }

    pub fn enqueue_scan(&self, kind: &str, root: Option<&str>) -> Result<i64> {
        enqueue_scan(&self.0, kind, root)
    }
}

#[derive(Debug)]
pub struct ScanRequest {
    pub id: i64,
    pub kind: String,
    pub root: Option<String>,
}

#[derive(Debug)]
pub struct ScanRequestStatus {
    pub status: String,
    pub scan_run_id: Option<i64>,
    pub error: Option<String>,
    pub files_seen: Option<i64>,
    pub duration_ms: Option<i64>,
}

pub fn claim_pending_scan(conn: &Connection) -> Result<Option<ScanRequest>> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let row = conn.query_row(
        "UPDATE scan_requests SET status = 'running', started_at = ?1
         WHERE id = (SELECT id FROM scan_requests WHERE status = 'pending' ORDER BY requested_at LIMIT 1)
         RETURNING id, kind, root",
        rusqlite::params![now_str],
        |r| {
            Ok(ScanRequest {
                id: r.get(0)?,
                kind: r.get(1)?,
                root: r.get(2)?,
            })
        },
    );
    match row {
        Ok(req) => Ok(Some(req)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(SmritiError::Db(e)),
    }
}

pub fn complete_scan_request(conn: &Connection, id: i64, scan_run_id: Option<i64>) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE scan_requests SET status = 'complete', completed_at = ?1, scan_run_id = ?2 WHERE id = ?3",
        rusqlite::params![now_str, scan_run_id, id],
    )?;
    Ok(())
}

pub fn fail_scan_request(conn: &Connection, id: i64, error: &str) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE scan_requests SET status = 'failed', completed_at = ?1, error = ?2 WHERE id = ?3",
        rusqlite::params![now_str, error, id],
    )?;
    Ok(())
}

pub fn poll_scan_request(conn: &Connection, id: i64) -> Result<Option<ScanRequestStatus>> {
    let row = conn.query_row(
        "SELECT sr.status, sr.scan_run_id, sr.error,
                s.files_seen,
                CAST((julianday(s.finished_at) - julianday(s.started_at)) * 86400000 AS INTEGER) as duration_ms
         FROM scan_requests sr
         LEFT JOIN scan_runs s ON sr.scan_run_id = s.id
         WHERE sr.id = ?1",
        rusqlite::params![id],
        |r| Ok(ScanRequestStatus {
            status: r.get(0)?,
            scan_run_id: r.get(1)?,
            error: r.get(2)?,
            files_seen: r.get(3)?,
            duration_ms: r.get(4)?,
        }),
    );
    match row {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(SmritiError::Db(e)),
    }
}

pub fn watcher_holds_lock(db_path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let lock_path = writer_lock_path(db_path);
    let Ok(file) = File::open(&lock_path) else {
        return false;
    };
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        true
    } else {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        false
    }
}

pub fn prune_events(conn: &Connection, older_than: Duration) -> Result<u64> {
    let threshold_secs = older_than.as_secs() as i64;
    let deleted = conn.execute(
        "DELETE FROM events WHERE timestamp < datetime('now', '-' || ? || ' seconds')",
        rusqlite::params![threshold_secs],
    )?;
    Ok(deleted as u64)
}

#[derive(Debug, serde::Serialize)]
pub struct EventRecord {
    pub id: i64,
    pub event_type: String,
    pub path: String,
    pub content_hash: String,
    pub previous_hash: Option<String>,
    pub previous_path: Option<String>,
    pub timestamp: String,
    pub file_extension: Option<String>,
    pub mime_type: Option<String>,
    pub scan_id: Option<i64>,
}

#[derive(Debug, serde::Serialize)]
pub struct EventPage {
    pub cursor_valid: bool,
    pub events: Vec<EventRecord>,
    pub next_cursor: i64,
    pub has_more: bool,
}

pub fn events_since(conn: &Connection, cursor: i64, limit: u32) -> Result<EventPage> {
    let limit = limit.min(1000);

    if cursor > 0 {
        let min_id: Option<i64> = conn.query_row("SELECT MIN(id) FROM events", [], |r| r.get(0))?;
        match min_id {
            None => {
                return Ok(EventPage {
                    cursor_valid: true,
                    events: vec![],
                    next_cursor: cursor,
                    has_more: false,
                });
            }
            Some(min) if min > cursor + 1 => {
                return Ok(EventPage {
                    cursor_valid: false,
                    events: vec![],
                    next_cursor: 0,
                    has_more: false,
                });
            }
            _ => {}
        }
    }

    let fetch = limit + 1;
    let mut stmt = conn.prepare(
        "SELECT id, event_type, path, content_hash, previous_hash, previous_path,
                timestamp, file_extension, mime_type, scan_id
         FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
    )?;

    let mut rows: Vec<EventRecord> = stmt
        .query_map(rusqlite::params![cursor, fetch], |row| {
            Ok(EventRecord {
                id: row.get(0)?,
                event_type: row.get(1)?,
                path: row.get(2)?,
                content_hash: row.get(3)?,
                previous_hash: row.get(4)?,
                previous_path: row.get(5)?,
                timestamp: row.get(6)?,
                file_extension: row.get(7)?,
                mime_type: row.get(8)?,
                scan_id: row.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let has_more = rows.len() > limit as usize;
    if has_more {
        rows.truncate(limit as usize);
    }

    let next_cursor = rows.last().map(|e| e.id).unwrap_or(cursor);

    Ok(EventPage {
        cursor_valid: true,
        events: rows,
        next_cursor,
        has_more,
    })
}

pub fn prune_audit_log(conn: &Connection, retention_days: u64) -> Result<u64> {
    let deleted = conn.execute(
        "DELETE FROM read_audit WHERE timestamp < datetime('now', '-' || ? || ' days')",
        rusqlite::params![retention_days as i64],
    )?;
    Ok(deleted as u64)
}

pub fn db_file_size(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path)?;
    Ok(meta.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_events_older_than() {
        let conn = open(Path::new(":memory:")).unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, first_seen) VALUES ('abc', datetime('now', '-2 days'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (event_type, content_hash, path, timestamp) VALUES ('created', 'abc', '/tmp/a', datetime('now', '-2 days'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (event_type, content_hash, path, timestamp) VALUES ('updated', 'abc', '/tmp/a', datetime('now'))",
            [],
        )
        .unwrap();
        let deleted = prune_events(&conn, Duration::from_secs(86400)).unwrap();
        assert_eq!(deleted, 1);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn test_prune_keeps_recent() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_audit(dir.path()).unwrap();
        conn.execute(
            "INSERT INTO read_audit (path, timestamp) VALUES ('/tmp/x', datetime('now'))",
            [],
        )
        .unwrap();
        let deleted = prune_audit_log(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM read_audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_watcher_tables_created() {
        let conn = open(Path::new(":memory:")).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('scan_requests','watcher_heartbeat') ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(tables, vec!["scan_requests", "watcher_heartbeat"]);
    }

    #[test]
    fn test_probe_index_health_ok_on_fresh_db() {
        let conn = open(Path::new(":memory:")).unwrap();
        probe_index_health(&conn).unwrap();
    }

    #[test]
    fn test_probe_fts_classifies_corrupt_virtual_table() {
        let conn = open(Path::new(":memory:")).unwrap();
        conn.execute(
            "INSERT INTO documents (content_hash, first_seen) VALUES ('abc', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO document_fts (rowid, title, topics, summary, content)
             VALUES (1, 'hello', '[]', '', 'hello world')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE document_fts_data SET block = x'00' WHERE id = (SELECT max(id) FROM document_fts_data)",
            [],
        )
        .unwrap();

        let err = probe_fts(&conn).unwrap_err();
        assert!(err.is_index_corrupt(), "{err}");
    }

    #[test]
    fn test_writer_lock_exclusive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");

        let _lock1 = acquire_writer_lock(&db_path).expect("first lock should succeed");
        let result = acquire_writer_lock(&db_path);
        assert!(result.is_err(), "second lock should fail");
    }

    #[test]
    fn test_writer_lock_released_on_drop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");

        {
            let _lock = acquire_writer_lock(&db_path).expect("first lock should succeed");
        }
        let _lock2 = acquire_writer_lock(&db_path).expect("lock should succeed after drop");
    }

    #[test]
    fn test_scan_request_lifecycle() {
        let conn = open(Path::new(":memory:")).unwrap();

        let id = enqueue_scan(&conn, "full", None).unwrap();
        assert!(id > 0);

        let status = poll_scan_request(&conn, id).unwrap().unwrap();
        assert_eq!(status.status, "pending");

        let req = claim_pending_scan(&conn).unwrap().unwrap();
        assert_eq!(req.id, id);
        assert_eq!(req.kind, "full");
        assert!(req.root.is_none());

        let status = poll_scan_request(&conn, id).unwrap().unwrap();
        assert_eq!(status.status, "running");

        conn.execute(
            "INSERT INTO scan_runs (id, started_at, status) VALUES (42, datetime('now'), 'running')",
            [],
        ).unwrap();
        complete_scan_request(&conn, id, Some(42)).unwrap();
        let status = poll_scan_request(&conn, id).unwrap().unwrap();
        assert_eq!(status.status, "complete");
        assert_eq!(status.scan_run_id, Some(42));
    }

    #[test]
    fn test_scan_request_fail() {
        let conn = open(Path::new(":memory:")).unwrap();
        let id = enqueue_scan(&conn, "path", Some(r#"["/tmp"]"#)).unwrap();
        let _ = claim_pending_scan(&conn).unwrap().unwrap();
        fail_scan_request(&conn, id, "boom").unwrap();
        let status = poll_scan_request(&conn, id).unwrap().unwrap();
        assert_eq!(status.status, "failed");
        assert_eq!(status.error.as_deref(), Some("boom"));
    }

    #[test]
    fn test_claim_returns_none_when_empty() {
        let conn = open(Path::new(":memory:")).unwrap();
        assert!(claim_pending_scan(&conn).unwrap().is_none());
    }

    #[test]
    fn test_watcher_holds_lock_detection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");

        assert!(!watcher_holds_lock(&db_path), "no lock file yet");

        let _lock = acquire_writer_lock(&db_path).unwrap();
        assert!(watcher_holds_lock(&db_path), "lock is held");

        drop(_lock);
        assert!(!watcher_holds_lock(&db_path), "lock released");
    }

    #[test]
    fn test_serve_readonly_while_writer_lock_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");

        // Watcher creates DB and holds writer.lock
        let _writer_conn = open(&db_path).unwrap();
        let _lock = acquire_writer_lock(&db_path).unwrap();

        // Serve opens read-only — must succeed without migrations
        let ro = open_readonly(&db_path).unwrap();
        let count: i64 = ro
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // ScanEnqueuer opens without running migrations
        let enqueuer = ScanEnqueuer::open(&db_path).unwrap();
        let id = enqueuer.enqueue_scan("full", None).unwrap();
        assert!(id > 0);
    }

    #[test]
    fn test_events_since_empty_table() {
        let conn = open(Path::new(":memory:")).unwrap();
        let page = events_since(&conn, 0, 100).unwrap();
        assert!(page.cursor_valid);
        assert!(page.events.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn test_events_since_pagination() {
        let conn = open(Path::new(":memory:")).unwrap();
        for i in 0..5 {
            conn.execute(
                "INSERT INTO events (event_type, content_hash, path, timestamp)
                 VALUES ('created', ?1, ?2, datetime('now'))",
                rusqlite::params![format!("hash{i}"), format!("/tmp/f{i}")],
            )
            .unwrap();
        }

        let page1 = events_since(&conn, 0, 2).unwrap();
        assert!(page1.cursor_valid);
        assert_eq!(page1.events.len(), 2);
        assert!(page1.has_more);
        assert_eq!(page1.events[0].path, "/tmp/f0");
        assert_eq!(page1.events[1].path, "/tmp/f1");

        let page2 = events_since(&conn, page1.next_cursor, 2).unwrap();
        assert_eq!(page2.events.len(), 2);
        assert!(page2.has_more);

        let page3 = events_since(&conn, page2.next_cursor, 2).unwrap();
        assert_eq!(page3.events.len(), 1);
        assert!(!page3.has_more);
    }

    #[test]
    fn test_events_since_cursor_expiry() {
        let conn = open(Path::new(":memory:")).unwrap();
        for i in 0..3 {
            conn.execute(
                "INSERT INTO events (event_type, content_hash, path, timestamp)
                 VALUES ('created', ?1, ?2, datetime('now'))",
                rusqlite::params![format!("hash{i}"), format!("/tmp/f{i}")],
            )
            .unwrap();
        }
        // Simulate pruning: delete the first event
        conn.execute("DELETE FROM events WHERE id = 1", []).unwrap();

        // Cursor 0 is always valid (start of retained window)
        let page = events_since(&conn, 0, 100).unwrap();
        assert!(page.cursor_valid);
        assert_eq!(page.events.len(), 2);

        // Cursor 1 is still valid (min_id=2, cursor+1=2, not behind)
        let page = events_since(&conn, 1, 100).unwrap();
        assert!(page.cursor_valid);

        // Simulate more pruning: delete event 2 too
        conn.execute("DELETE FROM events WHERE id = 2", []).unwrap();

        // Cursor 1 is now expired (min_id=3 > cursor+1=2)
        let page = events_since(&conn, 1, 100).unwrap();
        assert!(!page.cursor_valid);
        assert!(page.events.is_empty());
        assert_eq!(page.next_cursor, 0);
    }
}
