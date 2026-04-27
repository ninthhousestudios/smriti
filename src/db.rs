use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, ffi::sqlite3_auto_extension};

use crate::error::{Result, SmritiError};

pub fn open(path: &Path) -> Result<Connection> {
    // sqlite-vec must be registered before the connection is opened so it
    // applies to this and all subsequent connections in the process.
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

    // PRAGMA journal_mode returns a row, so we must use pragma_update_and_check.
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;

    run_migrations(&conn)?;

    Ok(conn)
}

pub fn run_migrations(conn: &Connection) -> Result<()> {
    let sql = include_str!("../migrations/0001_initial.sql");
    conn.execute_batch(sql).map_err(|e| SmritiError::Migration {
        message: e.to_string(),
    })
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
}
