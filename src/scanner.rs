//! Core scan engine.
//!
//! Walks allowlisted roots, classifies paths via [`IgnoreStack`], applies an
//! mtime+size short-circuit, diffs against the previous state in `paths`, emits
//! lifecycle events, and records a snapshot row.
//!
//! The per-path core ([`process_path`]) is the shared indexing primitive used by
//! both the batch scanner and the watcher's debounce flusher.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::Utc;
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension as _};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db;
use crate::error::Result;
use crate::hasher;
use crate::ignore::{hardened_defaults, IgnoreStack, PathClassification, SectionRules};
use crate::metadata;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventType {
    Created,
    Moved,
    Updated,
    MinorChange,
    Deleted,
    Copied,
    Hardlinked,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Moved => "moved",
            Self::Updated => "updated",
            Self::MinorChange => "minor_change",
            Self::Deleted => "deleted",
            Self::Copied => "copied",
            Self::Hardlinked => "hardlinked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub event_type: EventType,
    pub content_hash: String,
    pub path: String,
    pub timestamp: chrono::DateTime<Utc>,
    pub file_extension: Option<String>,
    pub mime_type: String,
}

#[derive(Debug, Default, Clone)]
pub struct Tier1Summary {
    pub created: u32,
    pub moved: u32,
    pub updated: u32,
    pub minor_changed: u32,
    pub deleted: u32,
    pub copied: u32,
    pub hardlinked: u32,
    pub total: u32,
}

#[derive(Debug, Default, Clone)]
pub struct Tier2Summary {
    pub cataloged: u32,
    pub total: u32,
}

#[derive(Debug)]
pub struct ScanResult {
    pub tier1: Tier1Summary,
    pub tier2: Tier2Summary,
    pub events: Vec<Event>,
    pub duration_ms: u64,
    pub scan_run_id: Option<i64>,
}

/// What we know about a path from the previous scan (from the `paths` table).
#[derive(Debug, Clone)]
pub struct PrevPathEntry {
    pub content_hash: String,
    pub mtime: i64,
    pub size_bytes: i64,
}

/// Extracted metadata for a document.
pub struct DocInfo {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub topics_json: String,
    pub structure_json: String,
    pub is_binary: bool,
    pub fts_content: Option<String>,
}

impl std::fmt::Debug for DocInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocInfo")
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct CurrentEntry {
    pub path: PathBuf,
    pub root: PathBuf,
    pub content_hash: String,
    pub body_hash: String,
    pub mtime: i64,
    pub size_bytes: i64,
    pub short_circuited: bool,
    pub doc_info: Option<DocInfo>,
}

pub struct WalkEntry {
    pub path: PathBuf,
    pub root: PathBuf,
    pub mtime: i64,
    pub size_bytes: i64,
    pub inode: u64,
    pub is_large: bool,
    pub needs_hash: bool,
    pub prev_hash: Option<String>,
}

pub struct PathOutcome {
    pub event: Option<Event>,
}

pub struct WalkResult {
    pub entries: Vec<WalkEntry>,
    pub seen_paths: HashSet<PathBuf>,
    pub catalog_dirs: HashMap<PathBuf, (u64, u64)>,
}

#[derive(Debug)]
pub struct ScanRunStatus {
    pub id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub files_seen: i64,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Scan setup
// ---------------------------------------------------------------------------

pub fn prepare_scan(
    config: &Config,
    root_override: Option<Vec<PathBuf>>,
    jobs: Option<usize>,
) -> Result<(Connection, Config, SectionRules)> {
    if let Some(j) = jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .ok();
    }

    let mut scan_config = config.clone();
    if let Some(paths) = root_override {
        scan_config.roots = paths;
    }

    let effective_roots = crate::roots::load_roots(&scan_config)?;
    if effective_roots.is_empty() {
        return Err(crate::error::SmritiError::NoRoots);
    }
    scan_config.roots = effective_roots;

    let conn = db::open(&config.db_path)?;
    db::checkpoint_wal(&conn)?;
    let rules = crate::ignore::load_user_smritiignore();

    Ok((conn, scan_config, rules))
}

// ---------------------------------------------------------------------------
// Per-path core
// ---------------------------------------------------------------------------

/// Process a single path: upsert document + path rows, determine event.
///
/// Idempotent: calling twice with the same inputs produces the same DB state.
/// The caller manages transaction boundaries.
pub fn process_path(
    conn: &Connection,
    entry: &CurrentEntry,
    prev: Option<&PrevPathEntry>,
    old_body_hash: Option<&str>,
    scan_id: Option<i64>,
    now_str: &str,
) -> Result<PathOutcome> {
    let now_dt = Utc::now();
    let now_dt_str = now_dt.format("%Y-%m-%d %H:%M:%S").to_string();

    if let Some(ref info) = entry.doc_info {
        let body_hash_opt = if entry.body_hash.is_empty() || entry.body_hash == entry.content_hash {
            None::<&str>
        } else {
            Some(entry.body_hash.as_str())
        };

        let new_rowid: Option<i64> = conn.prepare_cached(
            "INSERT OR IGNORE INTO documents
                (content_hash, body_hash, title, summary, topics, structure, is_binary, byte_size, first_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             RETURNING rowid",
        )?.query_row(
            params![
                entry.content_hash,
                body_hash_opt,
                info.title,
                info.summary,
                info.topics_json,
                info.structure_json,
                info.is_binary,
                entry.size_bytes,
                now_str,
            ],
            |row| row.get(0),
        ).optional()?;

        if let Some(rowid) = new_rowid {
            if !info.is_binary {
                conn.prepare_cached(
                    "INSERT INTO document_fts (rowid, title, topics, summary, content)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    rowid,
                    info.title.as_deref().unwrap_or(""),
                    info.topics_json,
                    info.summary.as_deref().unwrap_or(""),
                    info.fts_content.as_deref().unwrap_or(""),
                ])?;
            }
        } else if !entry.body_hash.is_empty() && entry.body_hash != entry.content_hash {
            conn.prepare_cached(
                "UPDATE documents SET body_hash = ?1 WHERE content_hash = ?2 AND body_hash IS NULL",
            )?
            .execute(params![entry.body_hash, entry.content_hash])?;
        }
    }

    let path_str = entry.path.to_string_lossy();
    if entry.short_circuited {
        conn.prepare_cached(
            "UPDATE paths SET last_seen_scan = ?1 WHERE path = ?2 AND disappeared IS NULL",
        )?
        .execute(params![scan_id, path_str.as_ref()])?;
    } else {
        conn.prepare_cached(
            "UPDATE paths SET disappeared = ?1 WHERE path = ?2 AND disappeared IS NULL",
        )?
        .execute(params![now_str, path_str.as_ref()])?;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO paths (content_hash, path, root, is_hardlink, mtime, size_bytes, appeared, last_seen_scan)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?.execute(params![
            entry.content_hash,
            path_str.as_ref(),
            entry.root.to_string_lossy().as_ref(),
            false,
            entry.mtime,
            entry.size_bytes,
            now_str,
            scan_id,
        ])?;
    }

    let event_type = determine_event_type_provisional(
        &entry.content_hash,
        &entry.body_hash,
        prev,
        old_body_hash,
    );

    let event = if let Some(et) = event_type {
        let ext = metadata::file_extension(&entry.path);
        let mime = metadata::detect_mime_type(&entry.path);
        conn.prepare_cached(
            "INSERT INTO events (event_type, content_hash, path, timestamp, file_extension, mime_type, scan_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?.execute(params![
            et.as_str(),
            entry.content_hash,
            path_str.as_ref(),
            now_dt_str,
            ext,
            mime,
            scan_id,
        ])?;
        Some(Event {
            event_type: et,
            content_hash: entry.content_hash.clone(),
            path: path_str.to_string(),
            timestamp: now_dt,
            file_extension: ext,
            mime_type: mime,
        })
    } else {
        None
    };

    Ok(PathOutcome { event })
}

fn determine_event_type_provisional(
    content_hash: &str,
    body_hash: &str,
    prev: Option<&PrevPathEntry>,
    old_body_hash: Option<&str>,
) -> Option<EventType> {
    if let Some(prev) = prev {
        if prev.content_hash == content_hash {
            return None;
        }
        let old_body = old_body_hash.unwrap_or("");
        if !old_body.is_empty()
            && !body_hash.is_empty()
            && old_body != prev.content_hash.as_str()
            && hasher::detect_minor_change(&prev.content_hash, content_hash, old_body, body_hash)
        {
            Some(EventType::MinorChange)
        } else {
            Some(EventType::Updated)
        }
    } else {
        Some(EventType::Created)
    }
}

// ---------------------------------------------------------------------------
// Walk phase
// ---------------------------------------------------------------------------

pub fn walk_roots(
    config: &Config,
    global_rules: &SectionRules,
    prev_paths: &HashMap<PathBuf, PrevPathEntry>,
) -> Result<WalkResult> {
    let mut entries: Vec<WalkEntry> = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut catalog_dirs: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    let mut skip_subtrees: Vec<PathBuf> = Vec::new();

    for root in &config.roots {
        if !root.exists() {
            tracing::warn!("root does not exist, skipping: {}", root.display());
            continue;
        }

        let global_layer = hardened_defaults(root);
        let mut ignore_stack = IgnoreStack::new(global_layer);
        let mut dir_depth_stack: Vec<usize> = Vec::new();

        let walker = WalkDir::new(root).follow_links(false).sort_by_file_name();

        for entry_result in walker {
            let entry = match entry_result {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("walkdir error: {}", e);
                    continue;
                }
            };

            let path = entry.path();
            let is_dir = entry.file_type().is_dir();
            let depth = entry.depth();

            if skip_subtrees
                .iter()
                .any(|s| path.starts_with(s) && path != s)
            {
                continue;
            }
            skip_subtrees
                .retain(|s| path.starts_with(s) || !s.starts_with(path.parent().unwrap_or(path)));

            while dir_depth_stack.last().copied().unwrap_or(0) >= depth
                && !dir_depth_stack.is_empty()
            {
                dir_depth_stack.pop();
                ignore_stack.pop();
            }

            if is_dir {
                let pushed = ignore_stack.push_dir(path)?;
                if pushed {
                    dir_depth_stack.push(depth);
                }
            }

            let classification_global = classify_section_rules(global_rules, path, is_dir);
            let classification_stack = ignore_stack.classify(path, is_dir);
            let classification = most_restrictive(classification_global, classification_stack);

            match classification {
                PathClassification::Ignored => {
                    if is_dir {
                        skip_subtrees.push(path.to_path_buf());
                    }
                    continue;
                }

                PathClassification::Cataloged if is_dir => {
                    let (total_bytes, file_count) = catalog_subtree(path);
                    catalog_dirs.insert(path.to_path_buf(), (total_bytes, file_count));
                    skip_subtrees.push(path.to_path_buf());
                    continue;
                }

                PathClassification::Cataloged => {
                    continue;
                }

                PathClassification::Indexed => {
                    if is_dir {
                        continue;
                    }

                    if entry.path_is_symlink() {
                        continue;
                    }
                    if !entry.file_type().is_file() {
                        continue;
                    }

                    let fs_meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!("cannot stat {}: {}", path.display(), e);
                            continue;
                        }
                    };

                    let mtime = fs_meta.mtime();
                    let size_bytes = fs_meta.size() as i64;
                    let inode = fs_meta.ino();
                    let is_large = fs_meta.len() > config.max_metadata_bytes;

                    seen_paths.insert(path.to_path_buf());

                    let (needs_hash, prev_hash) = if let Some(prev) = prev_paths.get(path) {
                        if prev.mtime == mtime && prev.size_bytes == size_bytes {
                            (false, Some(prev.content_hash.clone()))
                        } else {
                            (true, None)
                        }
                    } else {
                        (true, None)
                    };

                    entries.push(WalkEntry {
                        path: path.to_path_buf(),
                        root: root.clone(),
                        mtime,
                        size_bytes,
                        inode,
                        is_large,
                        needs_hash,
                        prev_hash,
                    });
                }
            }
        }
    }

    Ok(WalkResult {
        entries,
        seen_paths,
        catalog_dirs,
    })
}

// ---------------------------------------------------------------------------
// Public scan entry point
// ---------------------------------------------------------------------------

pub fn scan(
    conn: &mut Connection,
    config: &Config,
    global_rules: &SectionRules,
) -> Result<ScanResult> {
    scan_with_heartbeat(conn, config, global_rules, None)
}

/// Scan with an optional heartbeat tick. The watcher passes a closure that
/// bumps `watcher_heartbeat.updated_at`; the closure runs at each batch
/// boundary in the hash and DB-commit phases so a long scan doesn't make
/// the watcher look stale to serve.
pub fn scan_with_heartbeat(
    conn: &mut Connection,
    config: &Config,
    global_rules: &SectionRules,
    heartbeat: Option<&dyn Fn(&Connection)>,
) -> Result<ScanResult> {
    let start = Instant::now();
    let now = Utc::now();
    let now_str = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let batch_size = config.scan_batch_size;

    db::enable_scan_pragmas(conn)?;

    // 1. Register this scan run
    conn.execute(
        "INSERT INTO scan_runs (started_at, status) VALUES (?1, 'running')",
        params![now_str],
    )?;
    let scan_id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |row| row.get(0))?;
    tracing::info!("scan {scan_id} started, batch_size={batch_size}");

    // 2. Load previous state
    let prev_paths = load_prev_paths(conn)?;

    let mut prev_hash_to_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (path, entry) in &prev_paths {
        prev_hash_to_paths
            .entry(entry.content_hash.clone())
            .or_default()
            .push(path.clone());
    }

    let old_body_hashes = load_old_body_hashes(conn)?;

    // 3. Walk phase
    let walk_data = match walk_roots(config, global_rules, &prev_paths) {
        Ok(wd) => wd,
        Err(e) => {
            let _ = conn.execute(
                "UPDATE scan_runs SET finished_at = ?1, status = 'failed', error = ?2 WHERE id = ?3",
                params![
                    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    e.to_string(),
                    scan_id,
                ],
            );
            let _ = db::restore_default_pragmas(conn);
            return Err(e);
        }
    };
    let WalkResult {
        entries: walk_entries,
        seen_paths,
        catalog_dirs,
    } = walk_data;

    let walk_elapsed = start.elapsed();
    let needs_hash_count = walk_entries.iter().filter(|e| e.needs_hash).count();
    tracing::info!(
        "scan {scan_id} walk complete: {} files found, {} need hashing, {:.1}s",
        walk_entries.len(),
        needs_hash_count,
        walk_elapsed.as_secs_f64(),
    );
    if let Some(tick) = heartbeat {
        tick(conn);
    }

    // 4. Hash + metadata phase: parallel via rayon, in chunks so the watcher
    //    heartbeat can tick between chunks (rayon blocks the main thread for
    //    the duration of each par_iter call).
    let fts_max = config.fts_content_max_bytes as usize;
    type HashResult = Option<(usize, String, String, Option<DocInfo>)>;
    let hash_chunk_size = batch_size.max(1);
    let mut hash_results: Vec<HashResult> = Vec::with_capacity(walk_entries.len());
    for (chunk_idx, chunk) in walk_entries.chunks(hash_chunk_size).enumerate() {
        let base = chunk_idx * hash_chunk_size;
        let chunk_results: Vec<HashResult> = chunk
            .par_iter()
            .enumerate()
            .filter_map(|(local_idx, entry)| {
                if !entry.needs_hash {
                    return None;
                }
                let idx = base + local_idx;
                if entry.is_large {
                    Some(match hasher::hash_file(&entry.path) {
                        Ok(content_hash) => {
                            let doc_info = DocInfo {
                                title: None,
                                summary: None,
                                topics_json: "[]".to_string(),
                                structure_json: "[]".to_string(),
                                is_binary: true,
                                fts_content: None,
                            };
                            Some((idx, content_hash.clone(), content_hash, Some(doc_info)))
                        }
                        Err(e) => {
                            tracing::warn!("cannot hash {}: {}", entry.path.display(), e);
                            None
                        }
                    })
                } else {
                    Some(match std::fs::read(&entry.path) {
                        Ok(content) => {
                            let content_hash = hasher::hash_content(&content);
                            let body_hash = hasher::hash_body(&content);
                            let meta = metadata::extract_metadata(&entry.path, &content);
                            let topics_json = serde_json::to_string(&meta.topics)
                                .unwrap_or_else(|_| "[]".to_string());
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
                                std::str::from_utf8(&content)
                                    .ok()
                                    .map(|s| truncate_to_char_boundary(s, fts_max).to_string())
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
                            Some((idx, content_hash, body_hash, Some(doc_info)))
                        }
                        Err(e) => {
                            tracing::warn!("cannot read {}: {}", entry.path.display(), e);
                            None
                        }
                    })
                }
            })
            .collect();
        hash_results.extend(chunk_results);
        if let Some(tick) = heartbeat {
            tick(conn);
        }
    }

    let mut hash_map: HashMap<usize, (String, String, Option<DocInfo>)> = HashMap::new();
    for result in hash_results.into_iter().flatten() {
        let (idx, content_hash, body_hash, doc_info) = result;
        hash_map.insert(idx, (content_hash, body_hash, doc_info));
    }

    let hash_elapsed = start.elapsed() - walk_elapsed;
    tracing::info!(
        "scan {scan_id} hash+metadata complete: {} files processed in {:.1}s",
        hash_map.len(),
        hash_elapsed.as_secs_f64(),
    );

    // Build CurrentEntry list, skipping entries that failed to hash.
    let mut current_entries: Vec<CurrentEntry> = Vec::with_capacity(walk_entries.len());
    let mut current_hash_to_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut current_inode_to_paths: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    let mut current_path_to_inode: HashMap<PathBuf, u64> = HashMap::new();

    for (idx, we) in walk_entries.into_iter().enumerate() {
        let (content_hash, body_hash, short_circuited, doc_info) = if we.needs_hash {
            match hash_map.remove(&idx) {
                Some((ch, bh, di)) => (ch, bh, false, di),
                None => continue,
            }
        } else {
            (we.prev_hash.unwrap(), String::new(), true, None)
        };

        current_hash_to_paths
            .entry(content_hash.clone())
            .or_default()
            .push(we.path.clone());
        current_path_to_inode.insert(we.path.clone(), we.inode);
        current_inode_to_paths
            .entry(we.inode)
            .or_default()
            .push(we.path.clone());

        current_entries.push(CurrentEntry {
            path: we.path,
            root: we.root,
            content_hash,
            body_hash,
            mtime: we.mtime,
            size_bytes: we.size_bytes,
            short_circuited,
            doc_info,
        });
    }

    // 5. DB commit phase: flush in batches via process_path
    let mut total_files_seen: u64 = 0;
    let mut total_batches: u64 = 0;
    let mut all_events: Vec<Event> = Vec::new();

    for chunk in current_entries.chunks(batch_size) {
        match flush_batch(
            conn,
            chunk,
            &prev_paths,
            &old_body_hashes,
            scan_id,
            &now_str,
        ) {
            Ok(batch_events) => {
                all_events.extend(batch_events);
                total_files_seen += chunk.len() as u64;
                total_batches += 1;
                conn.execute(
                    "UPDATE scan_runs SET files_seen = ?1 WHERE id = ?2",
                    params![total_files_seen as i64, scan_id],
                )?;
                if let Some(tick) = heartbeat {
                    tick(conn);
                }
                if total_batches.is_multiple_of(10) {
                    tracing::info!(
                        "scan {scan_id} batch {total_batches} committed: {total_files_seen} files"
                    );
                }
            }
            Err(e) => {
                let _ = conn.execute(
                    "UPDATE scan_runs SET finished_at = ?1, status = 'failed', error = ?2 WHERE id = ?3",
                    params![
                        Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                        e.to_string(),
                        scan_id,
                    ],
                );
                let _ = db::restore_default_pragmas(conn);
                return Err(e);
            }
        }
    }

    tracing::info!(
        "scan {scan_id} batches complete: {total_files_seen} files in {total_batches} batches, beginning finalize"
    );

    // 6. Finalize
    let finalize_ctx = FinalizeContext {
        scan_id,
        now,
        now_str: &now_str,
        prev_hash_to_paths: &prev_hash_to_paths,
        seen_paths: &seen_paths,
        current_path_to_inode: &current_path_to_inode,
        current_inode_to_paths: &current_inode_to_paths,
        catalog_dirs: &catalog_dirs,
        total_files_seen,
        start,
    };
    let (tier1, tier2, duration_ms) = finalize_scan(conn, &mut all_events, &finalize_ctx)?;

    db::restore_default_pragmas(conn)?;
    let audit_dir = config.db_path.parent().unwrap_or(std::path::Path::new("."));
    if audit_dir.join("audit.db").exists() {
        if let Ok(audit_conn) = crate::db::open_audit(audit_dir) {
            let _ = crate::db::prune_audit_log(&audit_conn, config.audit_retention_days);
        }
    }

    Ok(ScanResult {
        tier1,
        tier2,
        events: all_events,
        duration_ms,
        scan_run_id: Some(scan_id),
    })
}

/// Query the status of a running (or most recent) scan.
pub fn scan_status(conn: &Connection) -> Result<Option<ScanRunStatus>> {
    let mut stmt = conn.prepare(
        "SELECT id, started_at, finished_at, status, files_seen, error
         FROM scan_runs ORDER BY id DESC LIMIT 1",
    )?;
    let result = stmt.query_row([], |row| {
        Ok(ScanRunStatus {
            id: row.get(0)?,
            started_at: row.get(1)?,
            finished_at: row.get(2)?,
            status: row.get(3)?,
            files_seen: row.get(4)?,
            error: row.get(5)?,
        })
    });
    match result {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Private: batch flush (calls process_path in a transaction)
// ---------------------------------------------------------------------------

fn flush_batch(
    conn: &mut Connection,
    batch: &[CurrentEntry],
    prev_paths: &HashMap<PathBuf, PrevPathEntry>,
    old_body_hashes: &HashMap<String, String>,
    scan_id: i64,
    now_str: &str,
) -> Result<Vec<Event>> {
    let tx = conn.transaction()?;
    let mut batch_events = Vec::new();

    for entry in batch {
        let prev = prev_paths.get(&entry.path);
        let old_body = prev
            .and_then(|p| old_body_hashes.get(&p.content_hash))
            .map(|s| s.as_str());
        let outcome = process_path(&tx, entry, prev, old_body, Some(scan_id), now_str)?;
        if let Some(ev) = outcome.event {
            batch_events.push(ev);
        }
    }

    tx.commit()?;
    Ok(batch_events)
}

// ---------------------------------------------------------------------------
// Private: scan finalize phase
// ---------------------------------------------------------------------------

struct FinalizeContext<'a> {
    scan_id: i64,
    now: chrono::DateTime<Utc>,
    now_str: &'a str,
    prev_hash_to_paths: &'a HashMap<String, Vec<PathBuf>>,
    seen_paths: &'a HashSet<PathBuf>,
    current_path_to_inode: &'a HashMap<PathBuf, u64>,
    current_inode_to_paths: &'a HashMap<u64, Vec<PathBuf>>,
    catalog_dirs: &'a HashMap<PathBuf, (u64, u64)>,
    total_files_seen: u64,
    start: Instant,
}

fn finalize_scan(
    conn: &mut Connection,
    all_events: &mut Vec<Event>,
    ctx: &FinalizeContext,
) -> Result<(Tier1Summary, Tier2Summary, u64)> {
    let tx = conn.transaction()?;

    let disappeared_count = tx.execute(
        "UPDATE paths SET disappeared = ?1
         WHERE disappeared IS NULL
           AND (last_seen_scan IS NULL OR last_seen_scan < ?2)",
        params![ctx.now_str, ctx.scan_id],
    )?;

    {
        let mut stmt = tx.prepare(
            "SELECT path, content_hash FROM paths
             WHERE disappeared = ?1
             AND (last_seen_scan IS NULL OR last_seen_scan < ?2)
             AND NOT EXISTS (
                 SELECT 1 FROM paths p2
                 WHERE p2.path = paths.path AND p2.disappeared IS NULL
             )",
        )?;
        let rows = stmt.query_map(params![ctx.now_str, ctx.scan_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (path_str, content_hash) = row?;
            let p = Path::new(&path_str);
            let ext = metadata::file_extension(p);
            let mime = metadata::detect_mime_type(p);
            tx.execute(
                "INSERT INTO events (event_type, content_hash, path, timestamp, file_extension, mime_type, scan_id)
                 VALUES ('deleted', ?1, ?2, ?3, ?4, ?5, ?6)",
                params![content_hash, path_str, ctx.now_str, ext, mime, ctx.scan_id],
            )?;
            all_events.push(Event {
                event_type: EventType::Deleted,
                content_hash,
                path: path_str,
                timestamp: ctx.now,
                file_extension: ext,
                mime_type: mime,
            });
        }
    }

    {
        let mut stmt = tx.prepare(
            "SELECT id, content_hash, path FROM events
             WHERE scan_id = ?1 AND event_type = 'created'",
        )?;
        let provisional: Vec<(i64, String, String)> = stmt
            .query_map(params![ctx.scan_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        for (event_id, hash, path_str) in &provisional {
            if let Some(prev_path_list) = ctx.prev_hash_to_paths.get(hash.as_str()) {
                let gone_path = prev_path_list.iter().find(|p| !ctx.seen_paths.contains(*p));

                let upgrade = if gone_path.is_some() {
                    Some("moved")
                } else {
                    let entry_path = PathBuf::from(path_str);
                    let shared_inode = ctx
                        .current_path_to_inode
                        .get(&entry_path)
                        .and_then(|ino| ctx.current_inode_to_paths.get(ino))
                        .map(|paths| paths.len() > 1)
                        .unwrap_or(false);

                    if shared_inode {
                        Some("hardlinked")
                    } else {
                        Some("copied")
                    }
                };

                if let Some(new_type) = upgrade {
                    tx.execute(
                        "UPDATE events SET event_type = ?1 WHERE id = ?2",
                        params![new_type, event_id],
                    )?;
                    for ev in all_events.iter_mut() {
                        if ev.path == *path_str && ev.event_type == EventType::Created {
                            ev.event_type = match new_type {
                                "moved" => EventType::Moved,
                                "copied" => EventType::Copied,
                                "hardlinked" => EventType::Hardlinked,
                                _ => unreachable!(),
                            };
                            break;
                        }
                    }
                }
            }
        }
    }

    let mut tier1 = Tier1Summary::default();
    for ev in all_events.iter() {
        match ev.event_type {
            EventType::Created => tier1.created += 1,
            EventType::Moved => tier1.moved += 1,
            EventType::Updated => tier1.updated += 1,
            EventType::MinorChange => tier1.minor_changed += 1,
            EventType::Deleted => tier1.deleted += 1,
            EventType::Copied => tier1.copied += 1,
            EventType::Hardlinked => tier1.hardlinked += 1,
        }
        tier1.total += 1;
    }

    let tier2_cataloged = ctx.catalog_dirs.len() as u32;
    for (dir_path, (total_bytes, file_count)) in ctx.catalog_dirs {
        let path_str = dir_path.to_string_lossy();
        let existing: Option<(i64, i64)> = tx
            .query_row(
                "SELECT total_bytes, file_count FROM catalog WHERE path = ?1",
                params![path_str.as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        if let Some((prev_bytes, prev_count)) = existing {
            tx.execute(
                "UPDATE catalog SET previous_total_bytes = ?1, previous_file_count = ?2,
                    total_bytes = ?3, file_count = ?4, last_scanned = ?5
                 WHERE path = ?6",
                params![
                    prev_bytes,
                    prev_count,
                    *total_bytes as i64,
                    *file_count as i64,
                    ctx.now_str,
                    path_str.as_ref(),
                ],
            )?;
        } else {
            tx.execute(
                "INSERT INTO catalog (path, total_bytes, file_count, last_scanned)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    path_str.as_ref(),
                    *total_bytes as i64,
                    *file_count as i64,
                    ctx.now_str,
                ],
            )?;
        }
    }

    let duration_ms = ctx.start.elapsed().as_millis() as u64;
    tx.execute(
        "INSERT INTO snapshots (timestamp, tier1_files_scanned, tier2_dirs_cataloged, events_emitted, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            ctx.now_str,
            tier1.total as i64,
            tier2_cataloged as i64,
            all_events.len() as i64,
            duration_ms as i64,
        ],
    )?;

    tx.execute(
        "UPDATE scan_runs SET finished_at = ?1, status = 'complete', files_seen = ?2 WHERE id = ?3",
        params![
            Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ctx.total_files_seen as i64,
            ctx.scan_id,
        ],
    )?;

    tx.commit()?;

    tracing::info!(
        "scan {} complete: {} files, {} events, {duration_ms}ms ({disappeared_count} disappeared)",
        ctx.scan_id,
        ctx.total_files_seen,
        all_events.len(),
    );

    let tier2 = Tier2Summary {
        cataloged: tier2_cataloged,
        total: tier2_cataloged,
    };

    Ok((tier1, tier2, duration_ms))
}

// ---------------------------------------------------------------------------
// DB state loaders (used by scan, available for watcher)
// ---------------------------------------------------------------------------

pub fn load_prev_paths(conn: &Connection) -> Result<HashMap<PathBuf, PrevPathEntry>> {
    let mut stmt = conn.prepare(
        "SELECT path, content_hash, mtime, size_bytes FROM paths WHERE disappeared IS NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            PrevPathEntry {
                content_hash: row.get(1)?,
                mtime: row.get(2)?,
                size_bytes: row.get(3)?,
            },
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, entry) = row?;
        map.insert(path, entry);
    }
    Ok(map)
}

pub fn load_old_body_hashes(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT content_hash, body_hash FROM documents WHERE body_hash IS NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (hash, body_hash) = row?;
        map.insert(hash, body_hash);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn catalog_subtree(dir: &Path) -> (u64, u64) {
    let mut total_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        if entry.file_type().is_file() {
            file_count += 1;
            if let Ok(meta) = entry.metadata() {
                total_bytes += meta.len();
            }
        }
    }
    (total_bytes, file_count)
}

fn most_restrictive(a: PathClassification, b: PathClassification) -> PathClassification {
    let rank = |c: &PathClassification| match c {
        PathClassification::Ignored => 2,
        PathClassification::Cataloged => 1,
        PathClassification::Indexed => 0,
    };
    if rank(&a) >= rank(&b) {
        a
    } else {
        b
    }
}

fn classify_section_rules(rules: &SectionRules, path: &Path, is_dir: bool) -> PathClassification {
    if matches!(
        rules.ignored.matched(path, is_dir),
        ignore::Match::Ignore(_)
    ) {
        return PathClassification::Ignored;
    }
    if matches!(
        rules.cataloged.matched(path, is_dir),
        ignore::Match::Ignore(_)
    ) {
        return PathClassification::Cataloged;
    }
    PathClassification::Indexed
}

pub fn truncate_to_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_at_char_boundary_handles_ascii() {
        assert_eq!(truncate_to_char_boundary("hello world", 5), "hello");
        assert_eq!(truncate_to_char_boundary("hi", 100), "hi");
    }

    #[test]
    fn truncate_at_char_boundary_walks_back_inside_multibyte() {
        let mut s = String::from("a");
        s.push('ᥴ');
        assert_eq!(truncate_to_char_boundary(&s, 2), "a");
        assert_eq!(truncate_to_char_boundary(&s, 1), "a");
        assert_eq!(truncate_to_char_boundary(&s, 4), s.as_str());
    }

    #[test]
    fn truncate_at_char_boundary_regression_byte_102400() {
        let mut s = String::with_capacity(102_500);
        for _ in 0..102_399 {
            s.push('a');
        }
        s.push('ᥴ');
        for _ in 0..50 {
            s.push('z');
        }
        let truncated = truncate_to_char_boundary(&s, 102_400);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert_eq!(truncated.len(), 102_399);
    }
}
