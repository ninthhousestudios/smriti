//! Full-scan parity property test (smriti/21).
//!
//! Verifies that the watcher (startup scan + event processing) and the batch
//! scanner produce identical DB state for the same filesystem tree.
//!
//! Marked #[ignore] — run explicitly with `cargo test --test scan_parity_test -- --ignored`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use smriti::{config::Config, db, ignore, scanner, watcher};
use tempfile::TempDir;

fn make_config(db_dir: &TempDir, roots: Vec<PathBuf>) -> Config {
    Config {
        db_path: db_dir.path().join("index.db"),
        roots,
        model_path: None,
        listen_addr: "unix:/tmp/smriti-parity-test.sock".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
        scan_batch_size: 500,
        full_scan_interval_sec: 86400,
        shutdown_drain_ms: 10000,
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct PathRecord {
    content_hash: String,
    root: String,
    disappeared: bool,
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct DocRecord {
    body_hash: Option<String>,
    is_binary: bool,
    byte_size: Option<i64>,
}

fn extract_paths(conn: &Connection) -> BTreeMap<String, PathRecord> {
    let mut stmt = conn
        .prepare("SELECT path, content_hash, root, disappeared FROM paths")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathRecord {
                    content_hash: r.get(1)?,
                    root: r.get(2)?,
                    disappeared: r.get::<_, Option<String>>(3)?.is_some(),
                },
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn extract_docs(conn: &Connection) -> BTreeMap<String, DocRecord> {
    let mut stmt = conn
        .prepare("SELECT content_hash, body_hash, is_binary, byte_size FROM documents")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                DocRecord {
                    body_hash: r.get(1)?,
                    is_binary: r.get(2)?,
                    byte_size: r.get(3)?,
                },
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn build_test_tree(root: &std::path::Path) {
    // Plain files
    std::fs::write(root.join("readme.txt"), "Hello, world!\n").unwrap();
    std::fs::write(root.join("data.json"), r#"{"key": "value"}"#).unwrap();
    std::fs::write(root.join("empty.txt"), "").unwrap();

    // Nested directories
    let sub = root.join("src");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(sub.join("lib.rs"), "pub mod foo;\n").unwrap();

    let deep = sub.join("nested");
    std::fs::create_dir(&deep).unwrap();
    std::fs::write(deep.join("deep.txt"), "deep content").unwrap();

    // Symlink (points to a file within the tree)
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("readme.txt"), root.join("link.txt")).unwrap();
    }

    // Binary-ish file
    std::fs::write(root.join("binary.bin"), vec![0u8, 1, 2, 255, 254, 253]).unwrap();
}

#[test]
fn scan_and_watcher_produce_identical_state() {
    // --- Build shared filesystem tree ---
    let tree = TempDir::new().unwrap();
    build_test_tree(tree.path());

    let root = tree.path().to_path_buf();

    // --- Run batch scanner ---
    let batch_db = TempDir::new().unwrap();
    let batch_config = make_config(&batch_db, vec![root.clone()]);
    let rules = ignore::load_user_smritiignore();

    let mut batch_conn = db::open(&batch_config.db_path).unwrap();
    scanner::scan(&mut batch_conn, &batch_config, &rules).unwrap();

    let batch_paths = extract_paths(&batch_conn);
    let batch_docs = extract_docs(&batch_conn);
    drop(batch_conn);

    // --- Run watcher (startup scan reaches same state) ---
    let watcher_db = TempDir::new().unwrap();
    let watcher_config = make_config(&watcher_db, vec![root.clone()]);

    // Pre-create DB so watcher can open it
    let _conn = db::open(&watcher_config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let watcher_config_clone = watcher_config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&watcher_config_clone, &shutdown_clone)
    });

    // Wait for startup scan to complete and settle
    std::thread::sleep(Duration::from_millis(4000));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let watcher_conn = db::open_readonly(&watcher_config.db_path).unwrap();
    let watcher_paths = extract_paths(&watcher_conn);
    let watcher_docs = extract_docs(&watcher_conn);
    drop(watcher_conn);

    // --- Compare paths ---
    assert_eq!(
        batch_paths.len(),
        watcher_paths.len(),
        "path count mismatch: batch={}, watcher={}",
        batch_paths.len(),
        watcher_paths.len()
    );

    for (path, batch_rec) in &batch_paths {
        let watcher_rec = watcher_paths
            .get(path)
            .unwrap_or_else(|| panic!("watcher missing path: {path}"));

        assert_eq!(
            batch_rec.content_hash, watcher_rec.content_hash,
            "content_hash mismatch for {path}"
        );
        assert_eq!(
            batch_rec.disappeared, watcher_rec.disappeared,
            "disappeared mismatch for {path}"
        );
    }

    // --- Compare documents ---
    assert_eq!(
        batch_docs.len(),
        watcher_docs.len(),
        "document count mismatch: batch={}, watcher={}",
        batch_docs.len(),
        watcher_docs.len()
    );

    for (hash, batch_doc) in &batch_docs {
        let watcher_doc = watcher_docs
            .get(hash)
            .unwrap_or_else(|| panic!("watcher missing document: {hash}"));

        assert_eq!(
            batch_doc.body_hash, watcher_doc.body_hash,
            "body_hash mismatch for doc {hash}"
        );
        assert_eq!(
            batch_doc.is_binary, watcher_doc.is_binary,
            "is_binary mismatch for doc {hash}"
        );
        assert_eq!(
            batch_doc.byte_size, watcher_doc.byte_size,
            "byte_size mismatch for doc {hash}"
        );
    }
}

#[test]
fn scan_and_watcher_agree_on_ignored_files() {
    let tree = TempDir::new().unwrap();
    let root = tree.path().to_path_buf();

    // Create files that should be ignored by default hardened rules
    std::fs::write(root.join("keep.txt"), "visible").unwrap();
    let git_dir = root.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main").unwrap();
    let node = root.join("node_modules");
    std::fs::create_dir(&node).unwrap();
    std::fs::write(node.join("pkg.js"), "module.exports = {}").unwrap();

    // Use same rules as watcher (load_user_smritiignore); hardened_defaults
    // are applied internally by walk_roots regardless.
    let rules = ignore::load_user_smritiignore();

    // --- Batch scan ---
    let batch_db = TempDir::new().unwrap();
    let batch_config = make_config(&batch_db, vec![root.clone()]);
    let mut batch_conn = db::open(&batch_config.db_path).unwrap();
    scanner::scan(&mut batch_conn, &batch_config, &rules).unwrap();
    let batch_paths = extract_paths(&batch_conn);
    drop(batch_conn);

    // --- Watcher ---
    let watcher_db = TempDir::new().unwrap();
    let watcher_config = make_config(&watcher_db, vec![root.clone()]);
    let _conn = db::open(&watcher_config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let watcher_config_clone = watcher_config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&watcher_config_clone, &shutdown_clone)
    });

    std::thread::sleep(Duration::from_millis(4000));
    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let watcher_conn = db::open_readonly(&watcher_config.db_path).unwrap();
    let watcher_paths = extract_paths(&watcher_conn);
    drop(watcher_conn);

    // Both should only see keep.txt
    assert_eq!(
        batch_paths.len(),
        watcher_paths.len(),
        "ignored-file path count mismatch: batch={}, watcher={}",
        batch_paths.len(),
        watcher_paths.len()
    );

    for path in batch_paths.keys() {
        assert!(
            !path.contains(".git") && !path.contains("node_modules"),
            "ignored path leaked through: {path}"
        );
    }
}
