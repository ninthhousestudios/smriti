use std::path::{Path, PathBuf};

use smriti::config::Config;
use smriti::db;
use smriti::scanner::{
    self, CurrentEntry, DocInfo, EventType, PathOutcome, PrevPathEntry,
};
use tempfile::TempDir;

fn make_config() -> (Config, TempDir) {
    let db_tmp = TempDir::new().unwrap();
    let config = Config {
        db_path: db_tmp.path().join("index.db"),
        roots: vec![],
        model_path: None,
        listen_addr: "unix:/tmp/smriti-test.sock".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
        scan_batch_size: 500,
    };
    (config, db_tmp)
}

fn setup_db() -> (rusqlite::Connection, TempDir) {
    let (config, db_tmp) = make_config();
    let conn = db::open(&config.db_path).unwrap();
    register_scan_run(&conn);
    (conn, db_tmp)
}

fn register_scan_run(conn: &rusqlite::Connection) {
    conn.execute(
        "INSERT INTO scan_runs (started_at, status) VALUES ('2026-01-01 00:00:00', 'running')",
        [],
    )
    .unwrap();
}

fn make_entry(path: &str, content_hash: &str, body_hash: &str) -> CurrentEntry {
    CurrentEntry {
        path: PathBuf::from(path),
        root: PathBuf::from("/tmp/root"),
        content_hash: content_hash.to_string(),
        body_hash: body_hash.to_string(),
        mtime: 1000,
        size_bytes: 42,
        short_circuited: false,
        embed_excluded: false,
        doc_info: Some(DocInfo {
            title: Some("Test".to_string()),
            summary: None,
            topics_json: "[]".to_string(),
            structure_json: "[]".to_string(),
            is_binary: false,
            fts_content: Some("hello world".to_string()),
        }),
    }
}

// ---------------------------------------------------------------------------
// Create event
// ---------------------------------------------------------------------------

#[test]
fn process_path_create_emits_created_event() {
    let (conn, _tmp) = setup_db();
    let entry = make_entry("/tmp/root/a.txt", "hash_a", "body_a");

    let outcome = scanner::process_path(&conn, &entry, None, None, 1, "2026-01-01 00:00:00")
        .unwrap();

    let ev = outcome.event.expect("should emit Created event");
    assert_eq!(ev.event_type, EventType::Created);
    assert_eq!(ev.content_hash, "hash_a");
}

// ---------------------------------------------------------------------------
// Modify (update) event
// ---------------------------------------------------------------------------

#[test]
fn process_path_modify_emits_updated_event() {
    let (conn, _tmp) = setup_db();

    // First: create the path
    let entry1 = make_entry("/tmp/root/a.txt", "hash_v1", "body_v1");
    scanner::process_path(&conn, &entry1, None, None, 1, "2026-01-01 00:00:00").unwrap();

    // Second: update with new content, passing prev state
    let entry2 = make_entry("/tmp/root/a.txt", "hash_v2", "body_v2");
    let prev = PrevPathEntry {
        content_hash: "hash_v1".to_string(),
        mtime: 1000,
        size_bytes: 42,
    };

    let outcome =
        scanner::process_path(&conn, &entry2, Some(&prev), None, 1, "2026-01-01 00:00:00")
            .unwrap();

    let ev = outcome.event.expect("should emit Updated event");
    assert_eq!(ev.event_type, EventType::Updated);
}

// ---------------------------------------------------------------------------
// No-change (same hash) returns no event
// ---------------------------------------------------------------------------

#[test]
fn process_path_unchanged_returns_none() {
    let (conn, _tmp) = setup_db();

    let entry = make_entry("/tmp/root/a.txt", "hash_a", "body_a");
    scanner::process_path(&conn, &entry, None, None, 1, "2026-01-01 00:00:00").unwrap();

    // Call again with same hash — prev has matching content_hash
    let prev = PrevPathEntry {
        content_hash: "hash_a".to_string(),
        mtime: 1000,
        size_bytes: 42,
    };

    let outcome =
        scanner::process_path(&conn, &entry, Some(&prev), None, 1, "2026-01-01 00:00:00")
            .unwrap();

    assert!(outcome.event.is_none(), "unchanged path should not emit event");
}

// ---------------------------------------------------------------------------
// Idempotency: process_path(p) then process_path(p) yields same DB state
// ---------------------------------------------------------------------------

#[test]
fn process_path_idempotent() {
    let (conn, _tmp) = setup_db();

    let entry = make_entry("/tmp/root/a.txt", "hash_a", "body_a");

    // First call
    scanner::process_path(&conn, &entry, None, None, 1, "2026-01-01 00:00:00").unwrap();

    // Snapshot DB state after first call
    let docs_after_first: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    let paths_after_first: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Second call with same inputs
    scanner::process_path(&conn, &entry, None, None, 1, "2026-01-01 00:00:00").unwrap();

    let docs_after_second: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    let paths_after_second: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(docs_after_first, docs_after_second, "document count should be stable");
    // paths_after_second may have one more row (the disappear+reinsert cycle),
    // but the active (non-disappeared) count should be 1
    assert_eq!(paths_after_second, 1, "should have exactly one active path row");
}

// ---------------------------------------------------------------------------
// Short-circuited path just updates last_seen_scan
// ---------------------------------------------------------------------------

#[test]
fn process_path_short_circuited_updates_scan_id() {
    let (conn, _tmp) = setup_db();

    // First: create the path
    let entry = make_entry("/tmp/root/a.txt", "hash_a", "body_a");
    scanner::process_path(&conn, &entry, None, None, 1, "2026-01-01 00:00:00").unwrap();

    // Second: short-circuited call (mtime+size unchanged)
    let mut entry2 = make_entry("/tmp/root/a.txt", "hash_a", "");
    entry2.short_circuited = true;
    entry2.doc_info = None;

    let prev = PrevPathEntry {
        content_hash: "hash_a".to_string(),
        mtime: 1000,
        size_bytes: 42,
    };

    // Register scan run 2
    conn.execute(
        "INSERT INTO scan_runs (started_at, status) VALUES ('2026-01-01 00:01:00', 'running')",
        [],
    )
    .unwrap();

    let outcome =
        scanner::process_path(&conn, &entry2, Some(&prev), None, 2, "2026-01-01 00:01:00")
            .unwrap();

    assert!(outcome.event.is_none(), "short-circuited path should not emit event");

    let last_seen: i64 = conn
        .query_row(
            "SELECT last_seen_scan FROM paths WHERE path = '/tmp/root/a.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last_seen, 2, "last_seen_scan should be updated to scan 2");
}
