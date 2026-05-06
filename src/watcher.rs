use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use notify::{EventKind, RecursiveMode, Watcher};
use rusqlite::{Connection, params};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db;
use crate::debounce::{DebounceBuffer, FlushedEvent, FlushedKind, FsEventKind};
use crate::error::{Result, SmritiError};
use crate::hasher;
use crate::ignore::{self, PathClassification, SectionRules};
use crate::metadata;
use crate::scanner::{self, CurrentEntry, DocInfo, PrevPathEntry};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn run_watch(config: &Config) -> Result<()> {
    SHUTDOWN.store(false, Ordering::SeqCst);

    unsafe {
        libc::signal(libc::SIGTERM, signal_handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, signal_handler as *const () as libc::sighandler_t);
    }

    run_watch_with_shutdown(config, &SHUTDOWN)
}

pub fn run_watch_with_shutdown(config: &Config, shutdown: &AtomicBool) -> Result<()> {
    let _lock = db::acquire_writer_lock(&config.db_path)?;
    tracing::info!("writer lock acquired");

    let mut conn = db::open(&config.db_path)?;

    recover_crashed_scans(&conn)?;

    let roots = crate::roots::load_roots(config)?;
    if roots.is_empty() {
        return Err(SmritiError::NoRoots);
    }

    let global_rules = ignore::load_user_smritiignore();

    // Register inotify watches BEFORE scan so nothing falls through the gap
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: std::result::Result<notify::Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })
    .map_err(|e| SmritiError::Other(format!("Failed to create watcher: {e}")))?;

    let mut watch_count: i64 = 0;
    for root in &roots {
        if root.is_dir() {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .map_err(|e| SmritiError::Other(format!("Failed to watch {}: {e}", root.display())))?;
            tracing::info!("watching {}", root.display());
            watch_count += 1;
        } else {
            tracing::warn!("skipping non-directory root: {}", root.display());
        }
    }

    upsert_heartbeat(&conn, "scanning", watch_count)?;

    // Startup full scan — config needs resolved roots
    let mut scan_config = config.clone();
    scan_config.roots = roots.clone();
    let mut last_full_scan = Instant::now();
    match scanner::scan(&mut conn, &scan_config, &global_rules) {
        Ok(result) => {
            tracing::info!(
                "startup scan: {} created, {} updated, {} deleted in {}ms",
                result.tier1.created, result.tier1.updated, result.tier1.deleted, result.duration_ms,
            );
            update_heartbeat_scan_done(&conn, result.duration_ms)?;
            last_full_scan = Instant::now();
        }
        Err(e) => {
            tracing::error!("startup scan failed: {e}");
            update_heartbeat_state(&conn, "watching")?;
        }
    }

    let prev_paths = scanner::load_prev_paths(&conn)?;
    let old_body_hashes = scanner::load_old_body_hashes(&conn)?;

    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "INSERT INTO scan_runs (started_at, status) VALUES (?1, 'running')",
        params![now_str],
    )?;
    let watcher_scan_id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))?;

    tracing::info!("watching, {} roots, {} known paths, scan_id={}", roots.len(), prev_paths.len(), watcher_scan_id);

    event_loop(
        &mut conn, config, &scan_config, &rx, &roots, &global_rules,
        prev_paths, old_body_hashes, watcher_scan_id, shutdown, last_full_scan,
    )?;

    update_heartbeat_state(&conn, "stopped")?;
    tracing::info!("watcher shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Crash recovery + heartbeat
// ---------------------------------------------------------------------------

fn recover_crashed_scans(conn: &Connection) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let count = conn.execute(
        "UPDATE scan_runs SET status = 'failed', finished_at = ?1, error = 'watcher restarted' WHERE status = 'running'",
        params![now_str],
    )?;
    if count > 0 {
        tracing::info!("crash recovery: marked {} stale scan_runs as failed", count);
    }
    Ok(())
}

fn upsert_heartbeat(conn: &Connection, state: &str, watch_count: i64) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let pid = std::process::id() as i64;
    conn.execute(
        "INSERT INTO watcher_heartbeat (id, pid, started_at, updated_at, state, watch_count)
         VALUES (1, ?1, ?2, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET pid = ?1, started_at = ?2, updated_at = ?2, state = ?3, watch_count = ?4",
        params![pid, now_str, state, watch_count],
    )?;
    Ok(())
}

fn update_heartbeat_state(conn: &Connection, state: &str) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE watcher_heartbeat SET state = ?1, updated_at = ?2 WHERE id = 1",
        params![state, now_str],
    )?;
    Ok(())
}

fn update_heartbeat_scan_done(conn: &Connection, duration_ms: u64) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE watcher_heartbeat SET state = 'watching', updated_at = ?1, last_full_scan_at = ?1, last_full_scan_duration_ms = ?2 WHERE id = 1",
        params![now_str, duration_ms as i64],
    )?;
    Ok(())
}

fn tick_heartbeat(conn: &Connection, pending_events: i64) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE watcher_heartbeat SET updated_at = ?1, pending_events = ?2 WHERE id = 1",
        params![now_str, pending_events],
    )?;
    Ok(())
}

fn mark_event_processed(conn: &Connection) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE watcher_heartbeat SET last_event_processed_at = ?1, updated_at = ?1 WHERE id = 1",
        params![now_str],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

fn event_loop(
    conn: &mut Connection,
    config: &Config,
    scan_config: &Config,
    rx: &std::sync::mpsc::Receiver<notify::Event>,
    roots: &[PathBuf],
    global_rules: &SectionRules,
    mut prev_paths: HashMap<PathBuf, PrevPathEntry>,
    mut old_body_hashes: HashMap<String, String>,
    mut scan_id: i64,
    shutdown: &AtomicBool,
    mut last_full_scan: Instant,
) -> Result<()> {
    let mut debounce = DebounceBuffer::with_defaults();
    let fts_max = config.fts_content_max_bytes as usize;
    let scan_interval = Duration::from_secs(config.full_scan_interval_sec);
    let heartbeat_interval = Duration::from_secs(5);
    let mut last_heartbeat = Instant::now();
    let scan_req_interval = Duration::from_secs(1);
    let mut last_scan_req_check = Instant::now();

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        if last_heartbeat.elapsed() >= heartbeat_interval {
            let pending = debounce.pending_count() as i64;
            if let Err(e) = tick_heartbeat(conn, pending) {
                tracing::warn!("heartbeat tick failed: {e}");
            }
            last_heartbeat = Instant::now();
        }

        // Drain pending scan_requests
        if last_scan_req_check.elapsed() >= scan_req_interval {
            last_scan_req_check = Instant::now();
            while let Ok(Some(req)) = db::claim_pending_scan(conn) {
                tracing::info!(req_id = req.id, kind = %req.kind, "claimed scan request");
                update_heartbeat_state(conn, "scanning")?;

                let mut req_config = scan_config.clone();
                if let Some(ref root_str) = req.root {
                    if let Ok(paths) = serde_json::from_str::<Vec<PathBuf>>(root_str) {
                        req_config.roots = paths;
                    }
                }

                match scanner::scan(conn, &req_config, global_rules) {
                    Ok(result) => {
                        tracing::info!(
                            req_id = req.id, "scan request complete: {} created, {} updated, {} deleted in {}ms",
                            result.tier1.created, result.tier1.updated, result.tier1.deleted, result.duration_ms,
                        );
                        let scan_run_id = result.scan_run_id;
                        if let Err(e) = db::complete_scan_request(conn, req.id, scan_run_id) {
                            tracing::error!(req_id = req.id, "failed to mark complete: {e}");
                        }
                        update_heartbeat_scan_done(conn, result.duration_ms)?;
                    }
                    Err(e) => {
                        tracing::error!(req_id = req.id, "scan request failed: {e}");
                        if let Err(e2) = db::fail_scan_request(conn, req.id, &e.to_string()) {
                            tracing::error!(req_id = req.id, "failed to mark failed: {e2}");
                        }
                        update_heartbeat_state(conn, "watching")?;
                    }
                }

                prev_paths = scanner::load_prev_paths(conn)?;
                old_body_hashes = scanner::load_old_body_hashes(conn)?;
                if req.kind == "full" {
                    last_full_scan = Instant::now();
                }

                let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
                conn.execute(
                    "INSERT INTO scan_runs (started_at, status) VALUES (?1, 'running')",
                    params![now_str],
                )?;
                scan_id = conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))?;
            }
        }

        // Periodic safety-net scan
        if last_full_scan.elapsed() >= scan_interval {
            tracing::info!("periodic full scan triggered");
            update_heartbeat_state(conn, "scanning")?;

            match scanner::scan(conn, scan_config, global_rules) {
                Ok(result) => {
                    tracing::info!(
                        "periodic scan: {} created, {} updated, {} deleted in {}ms",
                        result.tier1.created, result.tier1.updated, result.tier1.deleted, result.duration_ms,
                    );
                    update_heartbeat_scan_done(conn, result.duration_ms)?;
                }
                Err(e) => {
                    tracing::error!("periodic scan failed: {e}");
                    update_heartbeat_state(conn, "watching")?;
                }
            }

            prev_paths = scanner::load_prev_paths(conn)?;
            old_body_hashes = scanner::load_old_body_hashes(conn)?;
            last_full_scan = Instant::now();

            let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
            conn.execute(
                "INSERT INTO scan_runs (started_at, status) VALUES (?1, 'running')",
                params![now_str],
            )?;
            scan_id = conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))?;
        }

        let timeout = debounce
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .map(|d| d.max(Duration::from_millis(10)))
            .unwrap_or(Duration::from_secs(1));

        match rx.recv_timeout(timeout) {
            Ok(event) => {
                let now = Instant::now();
                let cookie = event.tracker().unwrap_or(0) as u32;
                for path in &event.paths {
                    if let Some(kind) = map_notify_event(&event.kind, cookie) {
                        tracing::debug!("debounce insert: {:?} {:?}", kind, path);
                        debounce.insert(path.clone(), kind, now);
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();
        let flushed = debounce.flush(now);
        if !flushed.is_empty() {
            tracing::debug!("flushing {} events", flushed.len());
            let tx = conn.transaction().map_err(SmritiError::Db)?;
            for fe in &flushed {
                if let Err(e) = process_flushed(
                    &tx, config, fe, roots, global_rules, &prev_paths, &old_body_hashes, fts_max, scan_id,
                ) {
                    tracing::error!("processing {}: {e}", fe.path.display());
                }
            }
            if let Err(e) = tx.commit() {
                tracing::error!("commit failed: {e}");
                return Err(SmritiError::Db(e));
            }

            for fe in &flushed {
                update_prev_paths(&mut prev_paths, fe);
            }

            if let Err(e) = mark_event_processed(conn) {
                tracing::warn!("mark_event_processed failed: {e}");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Event mapping
// ---------------------------------------------------------------------------

fn map_notify_event(kind: &EventKind, cookie: u32) -> Option<FsEventKind> {
    match kind {
        EventKind::Modify(notify::event::ModifyKind::Name(mode)) => match mode {
            notify::event::RenameMode::From => Some(FsEventKind::MovedFrom { cookie }),
            notify::event::RenameMode::To => Some(FsEventKind::MovedTo { cookie }),
            notify::event::RenameMode::Both => None,
            _ => Some(FsEventKind::Modify),
        },
        EventKind::Create(_) => Some(FsEventKind::Create),
        EventKind::Modify(_) => Some(FsEventKind::Modify),
        EventKind::Remove(_) => Some(FsEventKind::Delete),
        EventKind::Access(notify::event::AccessKind::Close(notify::event::AccessMode::Write)) => {
            Some(FsEventKind::CloseWrite)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Per-event processing
// ---------------------------------------------------------------------------

fn find_root_for_path<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    roots.iter().find(|r| path.starts_with(r))
}

fn process_flushed(
    conn: &Connection,
    config: &Config,
    fe: &FlushedEvent,
    roots: &[PathBuf],
    global_rules: &SectionRules,
    prev_paths: &HashMap<PathBuf, PrevPathEntry>,
    old_body_hashes: &HashMap<String, String>,
    fts_max: usize,
    scan_id: i64,
) -> Result<()> {
    let root = match find_root_for_path(&fe.path, roots) {
        Some(r) => r,
        None => return Ok(()),
    };

    match &fe.kind {
        FlushedKind::Delete => {
            handle_delete(conn, &fe.path)?;
        }
        FlushedKind::Create | FlushedKind::Modify => {
            if fe.path.is_dir() {
                handle_new_directory(conn, config, &fe.path, root, global_rules, prev_paths, old_body_hashes, fts_max, scan_id)?;
            } else if fe.path.is_file() {
                handle_file(conn, config, &fe.path, root, global_rules, prev_paths, old_body_hashes, fts_max, scan_id)?;
            }
        }
        FlushedKind::Moved { from } => {
            handle_delete(conn, from)?;
            if fe.path.is_dir() {
                handle_new_directory(conn, config, &fe.path, root, global_rules, prev_paths, old_body_hashes, fts_max, scan_id)?;
            } else if fe.path.is_file() {
                handle_file(conn, config, &fe.path, root, global_rules, prev_paths, old_body_hashes, fts_max, scan_id)?;
            }
        }
    }

    Ok(())
}

fn handle_delete(conn: &Connection, path: &Path) -> Result<()> {
    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let path_str = path.to_string_lossy();

    let content_hash: Option<String> = conn
        .prepare_cached(
            "SELECT content_hash FROM paths WHERE path = ?1 AND disappeared IS NULL",
        )?
        .query_row(params![path_str.as_ref()], |r| r.get(0))
        .ok();

    conn.prepare_cached(
        "UPDATE paths SET disappeared = ?1 WHERE path = ?2 AND disappeared IS NULL",
    )?
    .execute(params![now_str, path_str.as_ref()])?;

    if let Some(hash) = content_hash {
        let ext = metadata::file_extension(path);
        let mime = metadata::detect_mime_type(path);
        conn.prepare_cached(
            "INSERT INTO events (event_type, content_hash, path, timestamp, file_extension, mime_type)
             VALUES ('deleted', ?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![hash, path_str.as_ref(), now_str, ext, mime])?;
    }

    Ok(())
}

fn handle_file(
    conn: &Connection,
    config: &Config,
    path: &Path,
    root: &Path,
    global_rules: &SectionRules,
    prev_paths: &HashMap<PathBuf, PrevPathEntry>,
    old_body_hashes: &HashMap<String, String>,
    fts_max: usize,
    scan_id: i64,
) -> Result<()> {
    let classification = classify_path(path, global_rules);
    if classification == PathClassification::Ignored {
        return Ok(());
    }

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    let mtime = meta.mtime();
    let size_bytes = meta.len() as i64;
    let is_large = size_bytes > config.max_metadata_bytes as i64;
    let embed_excluded = classification == PathClassification::IndexedNoEmbed;

    let (content_hash, body_hash, doc_info) = if is_large {
        match hasher::hash_file(path) {
            Ok(ch) => {
                let di = DocInfo {
                    title: None,
                    summary: None,
                    topics_json: "[]".to_string(),
                    structure_json: "[]".to_string(),
                    is_binary: true,
                    fts_content: None,
                };
                (ch.clone(), ch, Some(di))
            }
            Err(e) => {
                tracing::warn!("cannot hash {}: {e}", path.display());
                return Ok(());
            }
        }
    } else {
        match std::fs::read(path) {
            Ok(content) => build_doc_entry(path, &content, fts_max),
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", path.display());
                return Ok(());
            }
        }
    };

    let prev = prev_paths.get(path);
    let old_body = prev
        .and_then(|p| old_body_hashes.get(&p.content_hash))
        .map(|s| s.as_str());

    let now_str = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let entry = CurrentEntry {
        path: path.to_path_buf(),
        root: root.to_path_buf(),
        content_hash,
        body_hash,
        mtime,
        size_bytes,
        short_circuited: false,
        embed_excluded,
        doc_info,
    };

    scanner::process_path(conn, &entry, prev, old_body, scan_id, &now_str)?;
    Ok(())
}

fn handle_new_directory(
    conn: &Connection,
    config: &Config,
    dir: &Path,
    root: &Path,
    global_rules: &SectionRules,
    prev_paths: &HashMap<PathBuf, PrevPathEntry>,
    old_body_hashes: &HashMap<String, String>,
    fts_max: usize,
    scan_id: i64,
) -> Result<()> {
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            handle_file(conn, config, entry.path(), root, global_rules, prev_paths, old_body_hashes, fts_max, scan_id)?;
        }
    }
    Ok(())
}

fn build_doc_entry(path: &Path, content: &[u8], fts_max: usize) -> (String, String, Option<DocInfo>) {
    let content_hash = hasher::hash_content(content);
    let body_hash = hasher::hash_body(content);
    let meta = metadata::extract_metadata(path, content);
    let topics_json = serde_json::to_string(&meta.topics).unwrap_or_else(|_| "[]".to_string());
    let structure_json = serde_json::to_string(
        &meta
            .structure
            .iter()
            .map(|s| {
                serde_json::json!({
                    "heading": s.heading,
                    "level": s.level,
                    "line": s.line,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());
    let fts_content = if !meta.is_binary {
        std::str::from_utf8(content)
            .ok()
            .map(|s| scanner::truncate_to_char_boundary(s, fts_max).to_string())
    } else {
        None
    };
    let doc_info = DocInfo {
        title: meta.title,
        summary: meta.summary,
        topics_json,
        structure_json,
        is_binary: meta.is_binary,
        fts_content,
    };
    (content_hash, body_hash, Some(doc_info))
}

fn classify_path(path: &Path, global_rules: &SectionRules) -> PathClassification {
    let is_dir = path.is_dir();
    global_rules.classify(path, is_dir)
}

fn update_prev_paths(prev_paths: &mut HashMap<PathBuf, PrevPathEntry>, fe: &FlushedEvent) {
    match &fe.kind {
        FlushedKind::Delete => {
            prev_paths.remove(&fe.path);
        }
        FlushedKind::Moved { from } => {
            prev_paths.remove(from);
        }
        _ => {}
    }
}
