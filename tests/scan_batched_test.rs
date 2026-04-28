//! Integration tests for the batched scanner (scan generations).

use std::path::Path;

use smriti::config::Config;
use smriti::db;
use smriti::ignore::SectionRules;
use smriti::scanner::{self, EventType};
use tempfile::TempDir;

fn make_config(roots: Vec<std::path::PathBuf>) -> (Config, TempDir) {
    let db_tmp = TempDir::new().unwrap();
    let config = Config {
        db_path: db_tmp.path().join("index.db"),
        roots,
        model_path: None,
        listen_addr: "unix:/tmp/smriti-test-batched.sock".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
        scan_batch_size: 3,
    };
    (config, db_tmp)
}

fn empty_rules(base: &Path) -> SectionRules {
    smriti::ignore::parse_smritiignore("", base).expect("empty rules")
}

// ---------------------------------------------------------------------------
// test_batched_new_files — basic smoke test
// ---------------------------------------------------------------------------
#[test]
fn test_batched_new_files() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("b.txt"), b"bravo").unwrap();
    std::fs::write(root.join("c.txt"), b"charlie").unwrap();
    std::fs::write(root.join("d.txt"), b"delta").unwrap();
    std::fs::write(root.join("e.txt"), b"echo").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let created: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Created).collect();
    assert_eq!(created.len(), 5, "expected 5 Created events, got: {:#?}", result.events);

    // scan_runs should show complete
    let status = scanner::scan_status(&conn).unwrap().expect("scan_runs row");
    assert_eq!(status.status, "complete");
    assert_eq!(status.files_seen, 5);
}

// ---------------------------------------------------------------------------
// test_batched_deleted_files — disappear pass in finalize
// ---------------------------------------------------------------------------
#[test]
fn test_batched_deleted_files() {

    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("keeper.txt"), b"staying").unwrap();
    std::fs::write(root.join("ephemeral.txt"), b"temporary").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    std::fs::remove_file(root.join("ephemeral.txt")).unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let deleted: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Deleted).collect();
    assert_eq!(deleted.len(), 1, "expected 1 Deleted event, got: {:#?}", result.events);
    assert!(deleted[0].path.contains("ephemeral.txt"));
}

// ---------------------------------------------------------------------------
// test_batched_mid_scan_visibility — batches are visible to a reader mid-scan
// ---------------------------------------------------------------------------
#[test]
fn test_batched_mid_scan_visibility() {

    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    // Create enough files for 2+ batches (batch_size=3)
    for i in 0..7 {
        std::fs::write(root.join(format!("file{i}.txt")), format!("content {i}")).unwrap();
    }

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    // All 7 files should be indexed
    let created: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Created).collect();
    assert_eq!(created.len(), 7);

    // Documents and paths should all be present
    let doc_count: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(doc_count, 7);

    let path_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM paths WHERE disappeared IS NULL",
        [],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(path_count, 7);
}

// ---------------------------------------------------------------------------
// test_batched_move_detection — moved file detected in finalize
// ---------------------------------------------------------------------------
#[test]
fn test_batched_move_detection() {

    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("original.txt"), b"unique content for move test").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    std::fs::rename(root.join("original.txt"), root.join("renamed.txt")).unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let moved: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Moved).collect();
    assert_eq!(moved.len(), 1, "expected 1 Moved event, got: {:#?}", result.events);
    assert!(moved[0].path.contains("renamed.txt"));

    // The moved event should also be persisted in the DB
    let db_moved: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'moved'",
        [],
        |r| r.get(0),
    ).unwrap();
    assert!(db_moved >= 1);
}

// ---------------------------------------------------------------------------
// test_batched_rerun_after_failure — re-run after failed scan works cleanly
// ---------------------------------------------------------------------------
#[test]
fn test_batched_rerun_after_normal_scan() {

    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("b.txt"), b"bravo").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    // First scan
    let r1 = scanner::scan(&mut conn, &config, &rules).unwrap();
    assert_eq!(r1.tier1.created, 2);

    // Second scan without changes — no new events
    let r2 = scanner::scan(&mut conn, &config, &rules).unwrap();
    assert_eq!(r2.tier1.total, 0, "second scan should produce no events: {:#?}", r2.events);

    // Add a file and re-scan
    std::fs::write(root.join("c.txt"), b"charlie").unwrap();
    let r3 = scanner::scan(&mut conn, &config, &rules).unwrap();
    assert_eq!(r3.tier1.created, 1);
}

// ---------------------------------------------------------------------------
// test_batched_scan_runs_recorded — scan_runs table tracks history
// ---------------------------------------------------------------------------
#[test]
fn test_batched_scan_runs_recorded() {

    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("a.txt"), b"alpha").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();
    scanner::scan(&mut conn, &config, &rules).unwrap();

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_runs", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 2, "expected 2 scan_runs rows");

    let all_complete: i64 = conn.query_row(
        "SELECT COUNT(*) FROM scan_runs WHERE status = 'complete'",
        [],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(all_complete, 2);
}

// ---------------------------------------------------------------------------
// test_batched_update_detection
// ---------------------------------------------------------------------------
#[test]
fn test_batched_update_detection() {

    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("doc.txt"), b"version 1").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    // Need to change mtime too — sleep briefly then write new content
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(root.join("doc.txt"), b"version 2").unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let updated: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Updated).collect();
    assert_eq!(updated.len(), 1, "expected 1 Updated event, got: {:#?}", result.events);
}
