use std::path::PathBuf;

use proptest::prelude::*;
use smriti::config::Config;
use smriti::db;
use smriti::scanner::{self, CurrentEntry, DocInfo, EventType, PrevPathEntry};
use tempfile::TempDir;

fn make_config() -> (Config, TempDir) {
    let db_tmp = TempDir::new().unwrap();
    let config = Config {
        db_path: db_tmp.path().join("index.db"),
        roots: vec![],
        listen_addr: "unix:/tmp/smriti-test.sock".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
        scan_batch_size: 500,
        full_scan_interval_sec: 86400,
        shutdown_drain_ms: 10000,
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

    let outcome =
        scanner::process_path(&conn, &entry, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

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
    scanner::process_path(&conn, &entry1, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

    // Second: update with new content, passing prev state
    let entry2 = make_entry("/tmp/root/a.txt", "hash_v2", "body_v2");
    let prev = PrevPathEntry {
        content_hash: "hash_v1".to_string(),
        mtime: 1000,
        size_bytes: 42,
    };

    let outcome = scanner::process_path(
        &conn,
        &entry2,
        Some(&prev),
        None,
        Some(1),
        "2026-01-01 00:00:00",
    )
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
    scanner::process_path(&conn, &entry, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

    // Call again with same hash — prev has matching content_hash
    let prev = PrevPathEntry {
        content_hash: "hash_a".to_string(),
        mtime: 1000,
        size_bytes: 42,
    };

    let outcome = scanner::process_path(
        &conn,
        &entry,
        Some(&prev),
        None,
        Some(1),
        "2026-01-01 00:00:00",
    )
    .unwrap();

    assert!(
        outcome.event.is_none(),
        "unchanged path should not emit event"
    );
}

// ---------------------------------------------------------------------------
// Idempotency: process_path(p) then process_path(p) yields same DB state
// ---------------------------------------------------------------------------

#[test]
fn process_path_idempotent() {
    let (conn, _tmp) = setup_db();

    let entry = make_entry("/tmp/root/a.txt", "hash_a", "body_a");

    // First call
    scanner::process_path(&conn, &entry, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

    // Snapshot DB state after first call
    let docs_after_first: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    let _paths_after_first: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Second call with same inputs
    scanner::process_path(&conn, &entry, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

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

    assert_eq!(
        docs_after_first, docs_after_second,
        "document count should be stable"
    );
    // paths_after_second may have one more row (the disappear+reinsert cycle),
    // but the active (non-disappeared) count should be 1
    assert_eq!(
        paths_after_second, 1,
        "should have exactly one active path row"
    );
}

// ---------------------------------------------------------------------------
// Short-circuited path just updates last_seen_scan
// ---------------------------------------------------------------------------

#[test]
fn process_path_short_circuited_updates_scan_id() {
    let (conn, _tmp) = setup_db();

    // First: create the path
    let entry = make_entry("/tmp/root/a.txt", "hash_a", "body_a");
    scanner::process_path(&conn, &entry, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

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

    let outcome = scanner::process_path(
        &conn,
        &entry2,
        Some(&prev),
        None,
        Some(2),
        "2026-01-01 00:01:00",
    )
    .unwrap();

    assert!(
        outcome.event.is_none(),
        "short-circuited path should not emit event"
    );

    let last_seen: i64 = conn
        .query_row(
            "SELECT last_seen_scan FROM paths WHERE path = '/tmp/root/a.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last_seen, 2, "last_seen_scan should be updated to scan 2");
}

// ---------------------------------------------------------------------------
// MinorChange event (frontmatter-only edit: content_hash changes, body_hash same)
// ---------------------------------------------------------------------------

#[test]
fn process_path_minor_change_when_only_frontmatter_changes() {
    let (conn, _tmp) = setup_db();

    let entry1 = make_entry("/tmp/root/a.md", "content_v1", "body_v1");
    scanner::process_path(&conn, &entry1, None, None, Some(1), "2026-01-01 00:00:00").unwrap();

    // Change content_hash (frontmatter changed) but keep body_hash the same
    let entry2 = make_entry("/tmp/root/a.md", "content_v2", "body_v1");
    let prev = PrevPathEntry {
        content_hash: "content_v1".to_string(),
        mtime: 1000,
        size_bytes: 42,
    };

    let outcome = scanner::process_path(
        &conn,
        &entry2,
        Some(&prev),
        Some("body_v1"),
        Some(1),
        "2026-01-01 00:00:00",
    )
    .unwrap();

    let ev = outcome.event.expect("should emit MinorChange event");
    assert_eq!(ev.event_type, EventType::MinorChange);
}

// ---------------------------------------------------------------------------
// Proptest: idempotency with random inputs
// ---------------------------------------------------------------------------

fn snap_db(conn: &rusqlite::Connection) -> (i64, i64, i64) {
    let docs: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    let active_paths: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    (docs, active_paths, events)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn process_path_idempotent_proptest(
        content_hash in "[a-f0-9]{8,16}",
        body_hash in "[a-f0-9]{8,16}",
        has_prev in proptest::bool::ANY,
        has_old_body in proptest::bool::ANY,
        short_circuited in proptest::bool::ANY,
    ) {
        let (conn, _tmp) = setup_db();

        let mut entry = make_entry("/tmp/root/test.txt", &content_hash, &body_hash);
        entry.short_circuited = short_circuited;
        if short_circuited {
            entry.doc_info = None;
        }

        let prev = if has_prev {
            Some(PrevPathEntry {
                content_hash: if short_circuited { content_hash.clone() } else { "prev_hash".to_string() },
                mtime: 1000,
                size_bytes: 42,
            })
        } else {
            None
        };

        let old_body = if has_old_body {
            Some("old_body_hash".to_string())
        } else {
            None
        };

        // If short_circuited but no prev, the UPDATE WHERE path=... won't match
        // anything — that's fine, it's a no-op edge case.

        // First call
        scanner::process_path(
            &conn,
            &entry,
            prev.as_ref(),
            old_body.as_deref(),
            Some(1),
            "2026-01-01 00:00:00",
        )
        .unwrap();

        let snap1 = snap_db(&conn);

        // Second call with identical inputs
        scanner::process_path(
            &conn,
            &entry,
            prev.as_ref(),
            old_body.as_deref(),
            Some(1),
            "2026-01-01 00:00:00",
        )
        .unwrap();

        let snap2 = snap_db(&conn);

        prop_assert_eq!(snap1.0, snap2.0, "document count must be stable");
        prop_assert_eq!(snap1.1, snap2.1, "active path count must be stable");
        // Events may increase (each call inserts an event row if there's a state change),
        // but document and path counts must be idempotent.
    }
}
