//! Integration tests for the scanner (Issue 5).

use std::path::Path;

use smriti::config::Config;
use smriti::db;
use smriti::ignore::{hardened_defaults, SectionRules};
use smriti::scanner::{self, EventType};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper: build a minimal Config pointing at a separate temp-dir DB + roots.
// DB lives outside the scan root so it's never indexed.
// Returns (Config, db_tmp) — caller must hold db_tmp alive.
// ---------------------------------------------------------------------------
fn make_config(roots: Vec<std::path::PathBuf>) -> (Config, TempDir) {
    let db_tmp = TempDir::new().unwrap();
    let config = Config {
        db_path: db_tmp.path().join("index.db"),
        roots,
        model_path: None,
        listen_addr: "unix:/tmp/smriti-test.sock".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
    };
    (config, db_tmp)
}

fn empty_rules(base: &Path) -> SectionRules {
    smriti::ignore::parse_smritiignore("", base).expect("empty rules")
}

// ---------------------------------------------------------------------------
// test_scan_new_files
// ---------------------------------------------------------------------------
#[test]
fn test_scan_new_files() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("hello.txt"), b"hello world").unwrap();
    std::fs::write(root.join("notes.md"), b"# Notes\n\nSome content.\n").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let created: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Created).collect();
    assert_eq!(created.len(), 2, "expected 2 Created events, got: {:#?}", result.events);
    assert_eq!(result.tier1.created, 2);

    for e in &created {
        assert!(!e.content_hash.is_empty());
        assert!(e.content_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

// ---------------------------------------------------------------------------
// test_scan_deleted_files
// ---------------------------------------------------------------------------
#[test]
fn test_scan_deleted_files() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("ephemeral.txt"), b"temporary").unwrap();
    std::fs::write(root.join("keeper.txt"), b"staying").unwrap();

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
// test_catalog_dir_tracking
// ---------------------------------------------------------------------------
#[test]
fn test_catalog_dir_tracking() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    let nm = root.join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("package.js"), b"module.exports = {};").unwrap();
    std::fs::write(nm.join("index.js"), b"// index").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    // Use hardened defaults so node_modules is [catalog]-matched.
    let rules = hardened_defaults(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    assert!(result.tier2.cataloged >= 1, "expected at least 1 cataloged dir");

    let (total_bytes, file_count): (i64, i64) = conn.query_row(
        "SELECT total_bytes, file_count FROM catalog WHERE path LIKE '%node_modules%'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).expect("catalog row should exist");

    assert_eq!(file_count, 2, "node_modules should have 2 files");
    assert!(total_bytes > 0, "total_bytes should be > 0");
}

// ---------------------------------------------------------------------------
// test_symlink_not_followed
// ---------------------------------------------------------------------------
#[test]
fn test_symlink_not_followed() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"secret content").unwrap();

    let link_path = root.join("link_to_secret.txt");
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link_path).unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let symlink_events: Vec<_> = result.events.iter()
        .filter(|e| e.path.contains("link_to_secret"))
        .collect();
    assert!(symlink_events.is_empty(), "symlink should not produce events: {:#?}", symlink_events);
}

// ---------------------------------------------------------------------------
// test_event_carries_extension
// ---------------------------------------------------------------------------
#[test]
fn test_event_carries_extension() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("readme.md"), b"# Readme\n\nContent.").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let ev = result.events.iter()
        .find(|e| e.path.contains("readme.md"))
        .expect("event for readme.md should exist");

    assert_eq!(ev.file_extension.as_deref(), Some("md"));
    assert_eq!(ev.mime_type, "text/markdown");
}

// ---------------------------------------------------------------------------
// test_nonexistent_root_skipped
// ---------------------------------------------------------------------------
#[test]
fn test_nonexistent_root_skipped() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    let fake_root = std::path::PathBuf::from("/nonexistent/path/that/does/not/exist");

    std::fs::write(root.join("real.txt"), b"real content").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone(), fake_root]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let created: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Created).collect();
    assert_eq!(created.len(), 1);
    assert!(created[0].path.contains("real.txt"));
}

// ---------------------------------------------------------------------------
// test_large_file_skips_metadata
// ---------------------------------------------------------------------------
#[test]
fn test_large_file_skips_metadata() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    let large_content = vec![b'A'; 200];
    std::fs::write(root.join("large.md"), &large_content).unwrap();

    let (mut config, _db_tmp) = make_config(vec![root.clone()]);
    config.max_metadata_bytes = 100; // 200-byte file exceeds this cap

    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();

    let ev = result.events.iter()
        .find(|e| e.path.contains("large.md"))
        .expect("should have event for large.md");
    assert_eq!(ev.event_type, EventType::Created);

    let is_binary: bool = conn.query_row(
        "SELECT is_binary FROM documents WHERE content_hash = ?1",
        [&ev.content_hash],
        |row| row.get(0),
    ).expect("document row should exist");

    assert!(is_binary, "large file should have is_binary=true");
}
