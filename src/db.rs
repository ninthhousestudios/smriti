use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ffi::sqlite3_auto_extension};

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

fn open_connection(path: &Path) -> Result<Connection> {
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

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
    conn.execute_batch(sql).map_err(|e| SmritiError::Migration {
        message: e.to_string(),
    })?;

    // 0002: scan generations — ALTER TABLE ADD COLUMN is not idempotent,
    // so check whether the column exists before running the migration.
    let has_last_seen: bool = conn
        .prepare("SELECT last_seen_scan FROM paths LIMIT 0")
        .is_ok();
    if !has_last_seen {
        let sql = include_str!("../migrations/0002_scan_generations.sql");
        conn.execute_batch(sql).map_err(|e| SmritiError::Migration {
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
    conn.execute_batch(sql).map_err(|e| SmritiError::Migration {
        message: format!("0003_watcher_tables: {e}"),
    })?;

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

pub fn prune_events(conn: &Connection, older_than: Duration) -> Result<u64> {
    let threshold_secs = older_than.as_secs() as i64;
    let deleted = conn.execute(
        "DELETE FROM events WHERE timestamp < datetime('now', '-' || ? || ' seconds')",
        rusqlite::params![threshold_secs],
    )?;
    Ok(deleted as u64)
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
    fn test_open_in_memory_and_vec_version() {
        let conn = open(Path::new(":memory:")).expect("open in-memory db");
        let version: String = conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("vec_version() should return a row");
        assert!(version.starts_with('v'), "vec_version returned: {version}");
    }

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
        let conn = open(Path::new(":memory:")).unwrap();
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
}
