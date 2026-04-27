//! Move, copy, hardlink, update, and minor-change detection tests.

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
    };
    (config, db_tmp)
}

fn empty_rules(base: &Path) -> smriti::ignore::SectionRules {
    parse_smritiignore("", base).expect("empty rules")
}

// ---------------------------------------------------------------------------
// test_update_detection
// ---------------------------------------------------------------------------
#[test]
fn test_update_detection() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("file.txt"), b"original content").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    std::fs::write(root.join("file.txt"), b"modified content with new body").unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let updated: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Updated).collect();
    assert_eq!(updated.len(), 1, "expected 1 Updated event, got: {:#?}", result.events);
    assert!(updated[0].path.contains("file.txt"));
}

// ---------------------------------------------------------------------------
// test_minor_change_detection
// ---------------------------------------------------------------------------
#[test]
fn test_minor_change_detection() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    let v1 = b"---\ntitle: Old Title\n---\n\n# Body\n\nSome content here.\n";
    std::fs::write(root.join("doc.md"), v1).unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    // Change only the frontmatter — body stays identical.
    let v2 = b"---\ntitle: New Title\ndate: 2026-01-01\n---\n\n# Body\n\nSome content here.\n";
    std::fs::write(root.join("doc.md"), v2).unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let minor: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::MinorChange).collect();
    assert_eq!(minor.len(), 1, "expected 1 MinorChange event, got: {:#?}", result.events);
}

// ---------------------------------------------------------------------------
// test_move_detection
// ---------------------------------------------------------------------------
#[test]
fn test_move_detection() {
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
}

// ---------------------------------------------------------------------------
// test_copy_detection
// ---------------------------------------------------------------------------
#[test]
fn test_copy_detection() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("source.txt"), b"unique content for copy test").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    // Copy — both paths exist afterwards.
    std::fs::copy(root.join("source.txt"), root.join("copy.txt")).unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let copied: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Copied).collect();
    assert_eq!(copied.len(), 1, "expected 1 Copied event, got: {:#?}", result.events);
    assert!(copied[0].path.contains("copy.txt"));
}

// ---------------------------------------------------------------------------
// test_hardlink_detection
// ---------------------------------------------------------------------------
#[test]
fn test_hardlink_detection() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("original.txt"), b"unique content for hardlink test").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    // Hard link — same inode.
    std::fs::hard_link(root.join("original.txt"), root.join("hardlink.txt")).unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let hardlinked: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Hardlinked).collect();
    assert_eq!(hardlinked.len(), 1, "expected 1 Hardlinked event, got: {:#?}", result.events);
    assert!(hardlinked[0].path.contains("hardlink.txt"));
}

// ---------------------------------------------------------------------------
// test_fuzzy_move_plus_edit
// Marked #[ignore] — detecting "move + content change" simultaneously requires
// comparing old filename to new filename heuristically, which is out of scope
// for v0.1.  The scanner emits Deleted+Created for a renamed-and-modified file.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "fuzzy move+edit heuristic not implemented in v0.1; emits Deleted+Created instead"]
fn test_fuzzy_move_plus_edit() {
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().to_path_buf();

    std::fs::write(root.join("before.txt"), b"some content A").unwrap();

    let (config, _db_tmp) = make_config(vec![root.clone()]);
    let mut conn = db::open(&config.db_path).unwrap();
    let rules = empty_rules(&root);

    scanner::scan(&mut conn, &config, &rules).unwrap();

    std::fs::remove_file(root.join("before.txt")).unwrap();
    std::fs::write(root.join("after.txt"), b"some content A modified").unwrap();

    let result = scanner::scan(&mut conn, &config, &rules).unwrap();
    let moved: Vec<_> = result.events.iter().filter(|e| e.event_type == EventType::Moved).collect();
    assert_eq!(moved.len(), 1, "expected Moved+Updated, got: {:#?}", result.events);
}
