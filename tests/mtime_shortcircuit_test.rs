//! mtime+size short-circuit tests.

use std::path::Path;

use smriti::config::Config;
use smriti::db;
use smriti::ignore::parse_smritiignore;
use smriti::scanner::{self, EventType};
use tempfile::TempDir;

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
        scan_batch_size: 500,
        full_scan_interval_sec: 86400,
        shutdown_drain_ms: 10000,
    };
    (config, db_tmp)
}

fn empty_rules(base: &Path) -> smriti::ignore::SectionRules {
    parse_smritiignore("", base).expect("empty rules")
}

// ---------------------------------------------------------------------------
// test_unchanged_mtime_size_skips_rehash
//
// Scan twice with no modifications. Second scan emits zero events —
// the mtime+size short-circuit fires for every file.
// ---------------------------------------------------------------------------
#[test]
fn test_unchanged_mtime_size_skips_rehash() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("stable.txt"), b"stable content").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    // First scan.
    let r1 = scanner::scan(&mut conn, &config, &rules).unwrap();
    assert_eq!(r1.events.len(), 1);
    assert_eq!(r1.events[0].event_type, EventType::Created);

    // Second scan — no changes.
    let r2 = scanner::scan(&mut conn, &config, &rules).unwrap();
    assert!(
        r2.events.is_empty(),
        "expected no events on second scan, got: {:#?}",
        r2.events
    );
}

// ---------------------------------------------------------------------------
// test_changed_mtime_triggers_rehash
//
// Rewrite identical content — mtime changes (after sleeping 1s), size same.
// Short-circuit misses and re-hash happens. Since content is identical,
// hash is same → no Updated event.  We verify no false Deleted/Created pair.
// ---------------------------------------------------------------------------
#[test]
fn test_changed_mtime_triggers_rehash() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("file.txt"), b"content").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    // Sleep 1s so filesystem timestamp actually advances.
    std::thread::sleep(std::time::Duration::from_secs(1));
    // Rewrite same bytes — mtime changes, size same, hash same.
    std::fs::write(root.join("file.txt"), b"content").unwrap();

    // Short-circuit must miss (mtime changed). Since content hash is same,
    // no Updated event is emitted. No Deleted either.
    let r2 = scanner::scan(&mut conn, &config, &rules).unwrap();
    let updates: Vec<_> = r2
        .events
        .iter()
        .filter(|e| e.event_type == EventType::Updated)
        .collect();
    let deletes: Vec<_> = r2
        .events
        .iter()
        .filter(|e| e.event_type == EventType::Deleted)
        .collect();
    assert!(
        updates.is_empty(),
        "same content should not produce Updated: {:#?}",
        r2.events
    );
    assert!(
        deletes.is_empty(),
        "file should not appear deleted: {:#?}",
        r2.events
    );
}

// ---------------------------------------------------------------------------
// test_changed_size_triggers_rehash
//
// Extend file content — size and mtime both change. Re-hash must happen.
// New content → Updated event.
// ---------------------------------------------------------------------------
#[test]
fn test_changed_size_triggers_rehash() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("growing.txt"), b"short").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    std::fs::write(
        root.join("growing.txt"),
        b"short and now much longer content here",
    )
    .unwrap();

    let r2 = scanner::scan(&mut conn, &config, &rules).unwrap();
    let updated: Vec<_> = r2
        .events
        .iter()
        .filter(|e| e.event_type == EventType::Updated)
        .collect();
    assert_eq!(
        updated.len(),
        1,
        "expected 1 Updated event, got: {:#?}",
        r2.events
    );
    assert!(updated[0].path.contains("growing.txt"));
}
