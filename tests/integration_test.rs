//! End-to-end integration tests: init → roots → scan → search → audit → manifest → history → health.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use smriti::config::Config;
use smriti::ignore::{SectionRules, hardened_defaults};
use smriti::search;

fn test_config(tmp: &TempDir) -> Config {
    let db_path = tmp.path().join("index.db");
    let root = tmp.path().join("docs");
    fs::create_dir_all(&root).unwrap();
    Config {
        db_path,
        roots: vec![root],
        model_path: None,
        listen_addr: "unix:/dev/null".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
        scan_batch_size: 500,
        full_scan_interval_sec: 86400,
        shutdown_drain_ms: 10000,
    }
}

fn write_file(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn test_full_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let root = &config.roots[0].clone();

    // Create test files
    write_file(root, "readme.md", "# My Project\n\nA description of the project.\n\n## Installation\n\nRun the installer.\n");
    write_file(root, "notes.txt", "some plain text notes here\n");
    write_file(root, "src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");

    // Init + scan
    let mut conn = smriti::db::open(&config.db_path).unwrap();
    let global_rules = SectionRules::empty();
    let result = smriti::scanner::scan(&mut conn, &config, &global_rules).unwrap();

    assert_eq!(result.tier1.created, 3);
    assert_eq!(result.tier1.total, 3);

    // Search via FTS
    let search_result = search::search_fts(&conn, "project", 10, &config).unwrap();
    assert!(!search_result.results.is_empty(), "FTS search for 'project' should find readme.md");
    assert_eq!(search_result.results[0].title, Some("My Project".to_string()));

    // Search for content
    let search_result = search::search_fts(&conn, "installer", 10, &config).unwrap();
    assert!(!search_result.results.is_empty(), "FTS search for 'installer' should find readme.md");

    // Search for text file content
    let search_result = search::search_fts(&conn, "plain text notes", 10, &config).unwrap();
    assert!(!search_result.results.is_empty(), "FTS search should find notes.txt");

    // Get document by hash
    let first_hash = &search_result.results[0].content_hash;
    let doc = search::get_document(&conn, first_hash, &config).unwrap();
    assert!(doc.path.is_some());
    assert_eq!(doc.content_hash, *first_hash);

    // Audit
    let audit = search::audit(&conn, None, None, &config).unwrap();
    assert_eq!(audit.tier1_total_files, 3);
    assert!(audit.tier1_total_bytes > 0);
    assert!(audit.tier1_by_extension.contains_key(".md"));
    assert!(audit.tier1_by_extension.contains_key(".txt"));
    assert!(audit.tier1_by_extension.contains_key(".rs"));
    assert_eq!(audit.backup_target_bytes, audit.tier1_total_bytes);

    // Manifest (paths format)
    let manifest = search::manifest(&conn, "paths", &config).unwrap();
    assert_eq!(manifest.entries.len(), 3);
    assert_eq!(manifest.format, "paths");

    // Manifest (ndjson format)
    let manifest_json = search::manifest(&conn, "ndjson", &config).unwrap();
    assert_eq!(manifest_json.entries.len(), 3);
    for entry in &manifest_json.entries {
        let parsed: serde_json::Value = serde_json::from_str(entry).unwrap();
        assert!(parsed.get("path").is_some());
        assert!(parsed.get("content_hash").is_some());
    }

    // Health
    let health = search::health(&conn, &config).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.total_indexed, 3);
    assert!(health.last_scan.is_some());
    assert!(!health.embedder_ok);
}

#[test]
fn test_history_after_update() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let root = &config.roots[0].clone();

    write_file(root, "doc.md", "# Version 1\n\nOriginal content.\n");

    let mut conn = smriti::db::open(&config.db_path).unwrap();
    let global_rules = SectionRules::empty();
    smriti::scanner::scan(&mut conn, &config, &global_rules).unwrap();

    // Update the file
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write_file(root, "doc.md", "# Version 2\n\nUpdated content.\n");

    let result = smriti::scanner::scan(&mut conn, &config, &global_rules).unwrap();
    assert_eq!(result.tier1.updated, 1);

    // Check history
    let doc_path = root.join("doc.md").to_string_lossy().to_string();
    let history = search::history(&conn, &doc_path, None, None, &config).unwrap();
    assert!(!history.events.is_empty());

    let event_types: Vec<&str> = history.events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(event_types.contains(&"created"), "should have a created event");
    assert!(event_types.contains(&"updated"), "should have an updated event");
}

#[test]
fn test_catalog_dirs_in_audit() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let root = &config.roots[0].clone();

    // Create a regular file and a catalog directory
    write_file(root, "important.md", "# Important\n");

    let node_modules = root.join("node_modules");
    fs::create_dir_all(&node_modules).unwrap();
    fs::write(node_modules.join("pkg.js"), "module.exports = {};").unwrap();
    fs::write(node_modules.join("big.js"), "x".repeat(10000)).unwrap();

    let mut conn = smriti::db::open(&config.db_path).unwrap();
    let global_rules = hardened_defaults(root);
    let _result = smriti::scanner::scan(&mut conn, &config, &global_rules).unwrap();

    let audit = search::audit(&conn, None, None, &config).unwrap();
    assert_eq!(audit.tier1_total_files, 1, "only important.md is tier 1");
    assert!(audit.tier2_total_dirs >= 1, "node_modules should be cataloged");
    assert!(audit.tier2_total_bytes > 0);
}

#[test]
fn test_search_no_results() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let root = &config.roots[0].clone();

    write_file(root, "hello.txt", "hello world\n");

    let mut conn = smriti::db::open(&config.db_path).unwrap();
    let global_rules = SectionRules::empty();
    smriti::scanner::scan(&mut conn, &config, &global_rules).unwrap();

    let result = search::search_fts(&conn, "xyznonexistent", 10, &config).unwrap();
    assert!(result.results.is_empty());
}

#[test]
fn test_get_document_not_found() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let conn = smriti::db::open(&config.db_path).unwrap();
    let result = search::get_document(&conn, "nonexistent_hash", &config);
    assert!(result.is_err());
}

#[test]
fn test_freshness_envelope() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let root = &config.roots[0].clone();

    write_file(root, "file.txt", "content\n");

    let mut conn = smriti::db::open(&config.db_path).unwrap();
    let global_rules = SectionRules::empty();
    smriti::scanner::scan(&mut conn, &config, &global_rules).unwrap();

    let result = search::search_fts(&conn, "content", 10, &config).unwrap();
    assert!(!result.envelope.is_stale, "just scanned — should not be stale");

    let health = search::health(&conn, &config).unwrap();
    assert!(health.last_scan.is_some());
}
