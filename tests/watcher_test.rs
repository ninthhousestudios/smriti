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
        full_scan_interval_sec: 86400,
        shutdown_drain_ms: 10000,
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
        assert_ne!(
            initial_hash, new_hash,
            "content_hash should change after modify"
        );
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
    assert!(
        count >= 2,
        "should have indexed at least 2 files in new directory, got {count}"
    );
}

#[test]
fn watcher_rename_within_root() {
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

    let old_path = root_path.join("before.txt");
    std::fs::write(&old_path, "rename me").unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    let hash_before: String = {
        let conn = db::open_readonly(&config.db_path).unwrap();
        conn.query_row(
            "SELECT content_hash FROM paths WHERE path LIKE '%before.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    let new_path = root_path.join("after.txt");
    std::fs::rename(&old_path, &new_path).unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();

    let old_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE path LIKE '%before.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        old_active, 0,
        "old path should be marked disappeared after rename"
    );

    let new_hash: String = conn
        .query_row(
            "SELECT content_hash FROM paths WHERE path LIKE '%after.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        hash_before, new_hash,
        "content_hash should be preserved across rename"
    );
}

#[test]
fn watcher_move_out_of_root_is_delete() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
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

    let file_path = root_path.join("going-away.txt");
    std::fs::write(&file_path, "bye").unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    {
        let conn = db::open_readonly(&config.db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM paths WHERE path LIKE '%going-away.txt' AND disappeared IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "file should be indexed before move");
    }

    std::fs::rename(&file_path, outside.path().join("going-away.txt")).unwrap();
    // Cookie TTL (1s) + debounce idle (1s) + margin
    std::thread::sleep(Duration::from_millis(3500));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE path LIKE '%going-away.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        active, 0,
        "file moved out of root should be marked disappeared"
    );
}

#[test]
fn watcher_atomic_write_via_rename() {
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

    let target = root_path.join("config.toml");
    std::fs::write(&target, "version = 1").unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    let hash_v1: String = {
        let conn = db::open_readonly(&config.db_path).unwrap();
        conn.query_row(
            "SELECT content_hash FROM paths WHERE path LIKE '%config.toml' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    // Atomic write: write tmp then rename into place
    let tmp = root_path.join("config.toml.tmp");
    std::fs::write(&tmp, "version = 2").unwrap();
    std::fs::rename(&tmp, &target).unwrap();
    std::thread::sleep(Duration::from_millis(2500));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let hash_v2: String = conn
        .query_row(
            "SELECT content_hash FROM paths WHERE path LIKE '%config.toml' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(hash_v1, hash_v2, "atomic write should update content_hash");

    let tmp_active: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE path LIKE '%config.toml.tmp' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tmp_active, 0, "tmp file should not remain as active path");
}

#[test]
fn watcher_startup_scan_indexes_preexisting_files() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);

    // Create files BEFORE starting the watcher
    std::fs::write(root.path().join("alpha.txt"), "aaa").unwrap();
    std::fs::write(root.path().join("beta.txt"), "bbb").unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("gamma.txt"), "ccc").unwrap();

    let _conn = db::open(&config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    // Give watcher time for startup scan + settling
    std::thread::sleep(Duration::from_millis(3000));

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 3,
        "startup scan should index all 3 pre-existing files"
    );

    let state: String = conn
        .query_row(
            "SELECT state FROM watcher_heartbeat WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        state, "stopped",
        "heartbeat should be 'stopped' after shutdown"
    );
}

#[test]
fn watcher_crash_recovery_marks_running_scans_failed() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);

    // Set up DB and insert a fake "running" scan_run (simulating a crash)
    {
        let conn = db::open(&config.db_path).unwrap();
        conn.execute(
            "INSERT INTO scan_runs (started_at, status) VALUES ('2026-01-01 00:00:00', 'running')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_runs (started_at, status) VALUES ('2026-01-01 01:00:00', 'running')",
            [],
        )
        .unwrap();
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    std::thread::sleep(Duration::from_millis(2000));
    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let crashed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM scan_runs WHERE error = 'watcher restarted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        crashed, 2,
        "both stale running scans should be marked as failed"
    );
}

#[test]
fn watcher_heartbeat_reflects_scanning_then_watching() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);

    // Add some files so the scan takes a moment
    for i in 0..10 {
        std::fs::write(
            root.path().join(format!("file{i}.txt")),
            format!("content {i}"),
        )
        .unwrap();
    }

    let _conn = db::open(&config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    // Wait for startup scan to complete and enter watching
    std::thread::sleep(Duration::from_millis(3000));

    {
        let conn = db::open_readonly(&config.db_path).unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM watcher_heartbeat WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            state, "watching",
            "should be in watching state after startup scan"
        );

        let last_scan: Option<String> = conn
            .query_row(
                "SELECT last_full_scan_at FROM watcher_heartbeat WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            last_scan.is_some(),
            "last_full_scan_at should be set after startup scan"
        );
    }

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn watcher_drains_scan_request() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);

    std::fs::write(root.path().join("a.txt"), "alpha").unwrap();

    let _conn = db::open(&config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    // Wait for startup scan + enter watching
    std::thread::sleep(Duration::from_millis(3000));

    // Enqueue a scan request from the "serve" side
    let conn = db::open(&config.db_path).unwrap();
    let req_id = db::enqueue_scan(&conn, "full", None).unwrap();

    // Poll until complete (timeout 10s)
    let start = std::time::Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        if start.elapsed() > Duration::from_secs(10) {
            panic!("scan request {req_id} did not complete within 10s");
        }
        let status = db::poll_scan_request(&conn, req_id).unwrap();
        if let Some(s) = status {
            if s.status == "complete" {
                assert!(s.scan_run_id.is_some(), "should link to a scan_run");
                break;
            }
            if s.status == "failed" {
                panic!("scan request failed: {:?}", s.error);
            }
        }
    }

    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn watcher_graceful_shutdown_drains_pending_events() {
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

    // Create a file and immediately signal shutdown, before debounce can flush
    std::fs::write(root_path.join("drain-me.txt"), "drain test").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    shutdown.store(true, Ordering::SeqCst);

    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM paths WHERE path LIKE '%drain-me.txt' AND disappeared IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "graceful shutdown should drain pending events");

    let state: String = conn
        .query_row(
            "SELECT state FROM watcher_heartbeat WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        state, "stopped",
        "heartbeat should be 'stopped' after graceful shutdown"
    );
}

#[test]
fn watcher_shutdown_aborts_running_scan_runs() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);

    let _conn = db::open(&config.db_path).unwrap();
    drop(_conn);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    std::thread::sleep(Duration::from_millis(3000));
    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let running_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM scan_runs WHERE status = 'running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        running_count, 0,
        "no scan_runs should remain running after shutdown"
    );

    let complete_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM scan_runs WHERE status = 'complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(complete_count >= 1, "startup scan should have completed");
}

#[test]
fn watcher_crash_recovery_marks_running_scan_requests_failed() {
    let db_dir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let config = make_config(&db_dir, &root);

    // Insert two stuck-running scan_requests as if a previous watcher crashed
    // mid-scan. The new watcher must reset them on startup.
    {
        let conn = db::open(&config.db_path).unwrap();
        conn.execute(
            "INSERT INTO scan_requests (requested_at, kind, status, started_at)
             VALUES ('2026-01-01 00:00:00', 'full', 'running', '2026-01-01 00:00:01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_requests (requested_at, kind, status, started_at)
             VALUES ('2026-01-01 01:00:00', 'path', 'running', '2026-01-01 01:00:01')",
            [],
        )
        .unwrap();
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let config_clone = config.clone();

    let handle = std::thread::spawn(move || {
        watcher::run_watch_with_shutdown(&config_clone, &shutdown_clone)
    });

    std::thread::sleep(Duration::from_millis(2000));
    shutdown.store(true, Ordering::SeqCst);
    let _ = handle.join();

    let conn = db::open_readonly(&config.db_path).unwrap();
    let recovered: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM scan_requests WHERE status = 'failed' AND error = 'watcher restarted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        recovered, 2,
        "both stale running scan_requests should be marked as failed on startup"
    );
    let still_running: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM scan_requests WHERE status = 'running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(still_running, 0, "no scan_requests should remain running");
}
