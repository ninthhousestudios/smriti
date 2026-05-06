use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smriti::config::Config;
use smriti::db;
use smriti::watcher;
use tempfile::TempDir;

fn make_config(db_dir: &TempDir, root: &TempDir) -> Config {
    Config {
        db_path: db_dir.path().join("index.db"),
        roots: vec![root.path().to_path_buf()],
        model_path: None,
        listen_addr: "unix:/tmp/smriti-watcher-test.sock".to_string(),
        stale_threshold_sec: 3600,
        fts_content_max_bytes: 102400,
        max_metadata_bytes: 524288000,
        audit_retention_days: 30,
        scan_batch_size: 500,
    }
}

#[test]
fn watcher_create_modify_delete_lifecycle() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);
    let root_path = root.path().to_path_buf();

    let _conn = db::open(&config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        let result = watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone);
        if let Err(ref e) = result {
            eprintln!("watcher error: {e}");
        }
        result
    });

    // Give the watcher time to register inotify watches
    std::thread::sleep(Duration::from_millis(500));

    // --- CREATE ---
    let file_path = root_path.join("test.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    // Wait for debounce idle (1s) + processing margin
    std::thread::sleep(Duration::from_millis(2500));

    {
        let conn = db::open_readonly(&config.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM paths WHERE path LIKE '%test.txt' AND disappeared IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "created file should appear in paths table");
    }

    // --- MODIFY ---
    let initial_hash: String = {
        let conn = db::open_readonly(&config.db_path).unwrap();
        conn.query_row(
            "SELECT content_hash FROM paths WHERE path LIKE '%test.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    std::fs::write(&file_path, "modified content that is different").unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    {
        let conn = db::open_readonly(&config.db_path).unwrap();
        let new_hash: String = conn
            .query_row(
                "SELECT content_hash FROM paths WHERE path LIKE '%test.txt' AND disappeared IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(initial_hash, new_hash, "content_hash should change after modify");
    }

    // --- DELETE ---
    std::fs::remove_file(&file_path).unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE path LIKE '%test.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 0, "deleted file should be marked disappeared");
}

#[test]
fn watcher_indexes_new_directory_children() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);
    let root_path = root.path().to_path_buf();

    let _conn = db::open(&config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    std::thread::sleep(Duration::from_millis(500));

    let subdir = root_path.join("newdir");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("a.txt"), "aaa").unwrap();
    std::fs::write(subdir.join("b.txt"), "bbb").unwrap();

    std::thread::sleep(Duration::from_millis(3000));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE path LIKE '%newdir%' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count >= 2, "should have indexed at least 2 files in new directory, got {count}");
}
