//! Core scan engine.
//!
//! Walks allowlisted roots, classifies paths via [`IgnoreStack`], applies an
//! mtime+size short-circuit, diffs against the previous state in `paths`, emits
//! lifecycle events, and records a snapshot row.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::Utc;
use rusqlite::{Connection, params};
use walkdir::WalkDir;

use crate::config::Config;
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
    fn as_str(&self) -> &'static str {
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
}

// ---------------------------------------------------------------------------
// Internal working types
// ---------------------------------------------------------------------------

/// What we know about a path from the previous scan (from the `paths` table).
#[derive(Debug)]
struct PrevPathEntry {
    content_hash: String,
    mtime: i64,
    size_bytes: i64,
}

/// What we computed for a path in the current scan.
#[derive(Debug)]
struct CurrentEntry {
    path: PathBuf,
    root: PathBuf,
    content_hash: String,
    body_hash: String,
    mtime: i64,
    size_bytes: i64,
    inode: u64,
    is_large: bool,       // exceeds max_metadata_bytes — skip metadata extraction
    short_circuited: bool, // mtime+size matched prev; hash reused, no re-hash needed
}

// ---------------------------------------------------------------------------
// Public scan function
// ---------------------------------------------------------------------------

/// Run a full scan cycle over all configured roots.
///
/// Returns a [`ScanResult`] summarising what was found, changed, or deleted.
///
/// # IgnoreStack / SectionRules ownership
///
/// `SectionRules` wraps `Gitignore` from the `ignore` crate, which is not
/// `Clone`.  Rather than fight that constraint, we rebuild a fresh
/// `hardened_defaults(root)` for each root and layer `global_rules` patterns
/// into it via a fresh IgnoreStack.  The global_rules reference is passed in
/// for classification but we start a fresh stack per root.  This is slightly
/// redundant work (recompiling the hardened patterns) but trivially fast and
/// keeps the code straightforward.
///
/// # Symlinks
///
/// `WalkDir` is configured with `follow_links(false)`.  Symlink entries are
/// skipped with a `tracing::debug!` log and not recorded.  This matches the
/// plan's "v0.1: skip symlinks entirely" note.
pub fn scan(
    conn: &mut Connection,
    config: &Config,
    global_rules: &SectionRules,
) -> Result<ScanResult> {
    let start = Instant::now();
    let now = Utc::now();
    let now_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

    // ------------------------------------------------------------------
    // 1. Load previous state from paths table (disappeared IS NULL = current)
    // ------------------------------------------------------------------
    let prev_paths: HashMap<PathBuf, PrevPathEntry> = {
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
        map
    };

    // Also build a reverse map: content_hash → set of paths (prev), for move/copy detection.
    let mut prev_hash_to_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (path, entry) in &prev_paths {
        prev_hash_to_paths
            .entry(entry.content_hash.clone())
            .or_default()
            .push(path.clone());
    }

    // ------------------------------------------------------------------
    // 2. Walk all roots, classify, hash
    // ------------------------------------------------------------------
    let mut current_entries: Vec<CurrentEntry> = Vec::new();
    // Track which paths we've seen this scan (for "still current" bookkeeping).
    let mut seen_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Catalog directories found this scan: path → (total_bytes, file_count).
    let mut catalog_dirs: HashMap<PathBuf, (u64, u64)> = HashMap::new();

    for root in &config.roots {
        if !root.exists() {
            tracing::warn!("root does not exist, skipping: {}", root.display());
            continue;
        }

        // Fresh IgnoreStack for each root: hardened defaults anchored at root,
        // then we overlay the global_rules logic by using a wrapper that checks
        // global_rules first (see walk loop below).
        let global_layer = hardened_defaults(root);
        let mut ignore_stack = IgnoreStack::new(global_layer);

        // Track the depth stack so we can pop correctly on ascent.
        let mut dir_depth_stack: Vec<usize> = Vec::new();

        let walker = WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name();

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

            // Manage IgnoreStack depth: pop layers for directories we've left.
            while dir_depth_stack.last().copied().unwrap_or(0) >= depth && !dir_depth_stack.is_empty() {
                dir_depth_stack.pop();
                ignore_stack.pop();
            }

            // When entering a directory, push any local .smritiignore.
            if is_dir {
                let pushed = ignore_stack.push_dir(path)?;
                if pushed {
                    dir_depth_stack.push(depth);
                }
            }

            // Also check global_rules (passed in by caller) — gives the caller
            // a chance to add extra rules beyond hardened defaults.
            let classification_global = classify_section_rules(global_rules, path, is_dir);
            let classification_stack = ignore_stack.classify(path, is_dir);

            // Take the most restrictive classification.
            let classification = most_restrictive(classification_global, classification_stack);

            match classification {
                PathClassification::Ignored => {
                    // Skip; if it's a dir, skip the whole subtree.
                    if is_dir {
                        // WalkDir doesn't have a skip method we can call from the iterator
                        // without consuming it. We rely on classify catching children too.
                        // For efficiency we could use into_iter().skip_current_dir() but
                        // that requires changing the walker type. Acceptable for v0.1.
                    }
                    continue;
                }

                PathClassification::Cataloged if is_dir => {
                    // Walk the subtree separately to compute total_bytes + file_count.
                    let (total_bytes, file_count) = catalog_subtree(path);
                    catalog_dirs.insert(path.to_path_buf(), (total_bytes, file_count));
                    // Don't recurse into this dir with the main walk — but WalkDir
                    // will naturally descend unless we skip. We rely on children being
                    // classified Cataloged (or Ignored) too via the directory match.
                    // Tier-2 cataloged dir is recorded; we continue the walk normally
                    // so children are also seen and skipped if needed.
                    continue;
                }

                PathClassification::Cataloged => {
                    // File inside a cataloged dir — skip (cataloged at dir level).
                    continue;
                }

                PathClassification::Indexed | PathClassification::IndexedNoEmbed => {
                    if is_dir {
                        // Normal directory — already pushed to ignore stack above.
                        continue;
                    }

                    // Skip symlinks.
                    if entry.path_is_symlink() {
                        tracing::debug!("skipping symlink: {}", path.display());
                        continue;
                    }

                    // Get filesystem metadata.
                    let fs_meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!("cannot stat {}: {}", path.display(), e);
                            continue;
                        }
                    };

                    let mtime = fs_meta.mtime(); // seconds since epoch
                    let size_bytes = fs_meta.size() as i64;
                    let inode = fs_meta.ino();
                    let is_large = fs_meta.len() > config.max_metadata_bytes;

                    seen_paths.insert(path.to_path_buf());

                    // mtime+size short-circuit.
                    if let Some(prev) = prev_paths.get(path) {
                        if prev.mtime == mtime && prev.size_bytes == size_bytes {
                            // Unchanged — reuse previous hash, mark short-circuited.
                            current_entries.push(CurrentEntry {
                                path: path.to_path_buf(),
                                root: root.clone(),
                                content_hash: prev.content_hash.clone(),
                                body_hash: String::new(), // not needed; no change
                                mtime,
                                size_bytes,
                                inode,
                                is_large,
                                short_circuited: true,
                            });
                            continue;
                        }
                    }

                    // Hash the file.
                    let content_hash = match hasher::hash_file(path) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!("cannot hash {}: {}", path.display(), e);
                            continue;
                        }
                    };

                    // Compute body hash for minor-change detection (only for non-large files).
                    let body_hash = if !is_large {
                        match std::fs::read(path) {
                            Ok(content) => hasher::hash_body(&content),
                            Err(_) => content_hash.clone(),
                        }
                    } else {
                        content_hash.clone()
                    };

                    current_entries.push(CurrentEntry {
                        path: path.to_path_buf(),
                        root: root.clone(),
                        content_hash,
                        body_hash,
                        mtime,
                        size_bytes,
                        inode,
                        is_large,
                        short_circuited: false,
                    });
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // 3. Build current hash → paths map (for move/copy detection)
    // ------------------------------------------------------------------
    let mut current_hash_to_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for entry in &current_entries {
        current_hash_to_paths
            .entry(entry.content_hash.clone())
            .or_default()
            .push(entry.path.clone());
    }

    // Also build a map from current inode to paths (for hardlink detection).
    let mut current_inode_to_paths: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for entry in &current_entries {
        current_inode_to_paths
            .entry(entry.inode)
            .or_default()
            .push(entry.path.clone());
    }

    // ------------------------------------------------------------------
    // 4. Diff: determine events for each current entry
    // ------------------------------------------------------------------
    let mut events: Vec<Event> = Vec::new();
    let mut tier1 = Tier1Summary::default();
    let now_dt = Utc::now();

    // We need a lookup: old_body_hash for a given content_hash, to detect minor changes.
    // Fetch from documents table.
    let old_body_hashes: HashMap<String, String> = {
        let mut stmt = conn.prepare("SELECT content_hash, body_hash FROM documents WHERE body_hash IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (hash, body_hash) = row?;
            map.insert(hash, body_hash);
        }
        map
    };

    for entry in &current_entries {
        let path = &entry.path;
        let event_type = determine_event_type(
            entry,
            &prev_paths,
            &prev_hash_to_paths,
            &current_inode_to_paths,
            &old_body_hashes,
            &seen_paths,
        );

        if let Some(et) = event_type {
            let ext = metadata::file_extension(path);
            let mime = metadata::detect_mime_type(path);

            match &et {
                EventType::Created => tier1.created += 1,
                EventType::Moved => tier1.moved += 1,
                EventType::Updated => tier1.updated += 1,
                EventType::MinorChange => tier1.minor_changed += 1,
                EventType::Copied => tier1.copied += 1,
                EventType::Hardlinked => tier1.hardlinked += 1,
                EventType::Deleted => tier1.deleted += 1,
            }
            tier1.total += 1;

            events.push(Event {
                event_type: et,
                content_hash: entry.content_hash.clone(),
                path: path.to_string_lossy().to_string(),
                timestamp: now_dt,
                file_extension: ext,
                mime_type: mime,
            });
        }
    }

    // ------------------------------------------------------------------
    // 5. Deleted events: prev paths not seen this scan
    // ------------------------------------------------------------------
    for (path, prev) in &prev_paths {
        if !seen_paths.contains(path) {
            let ext = metadata::file_extension(path);
            let mime = metadata::detect_mime_type(path);
            tier1.deleted += 1;
            tier1.total += 1;
            events.push(Event {
                event_type: EventType::Deleted,
                content_hash: prev.content_hash.clone(),
                path: path.to_string_lossy().to_string(),
                timestamp: now_dt,
                file_extension: ext,
                mime_type: mime,
            });
        }
    }

    // ------------------------------------------------------------------
    // 6. Write everything inside a transaction
    // ------------------------------------------------------------------
    let tx = conn.transaction()?;

    // 6a. Upsert documents
    for entry in &current_entries {
        // Check if document already exists.
        let exists: bool = tx.query_row(
            "SELECT COUNT(*) FROM documents WHERE content_hash = ?1",
            params![entry.content_hash],
            |row| row.get::<_, i64>(0),
        )? > 0;

        if !exists {
            // Extract metadata for new documents (unless large).
            let (title, summary, topics_json, structure_json, is_binary_doc) = if entry.is_large {
                (None::<String>, None::<String>, "[]".to_string(), "[]".to_string(), true)
            } else {
                match std::fs::read(&entry.path) {
                    Ok(content) => {
                        let meta = metadata::extract_metadata(&entry.path, &content);
                        let topics_json = serde_json::to_string(&meta.topics).unwrap_or_else(|_| "[]".to_string());
                        let structure_json = serde_json::to_string(
                            &meta.structure.iter().map(|s| {
                                serde_json::json!({
                                    "heading": s.heading,
                                    "level": s.level,
                                    "line": s.line,
                                })
                            }).collect::<Vec<_>>()
                        ).unwrap_or_else(|_| "[]".to_string());
                        (meta.title, meta.summary, topics_json, structure_json, meta.is_binary)
                    }
                    Err(_) => (None, None, "[]".to_string(), "[]".to_string(), false),
                }
            };

            let body_hash_opt = if entry.body_hash.is_empty() || entry.body_hash == entry.content_hash {
                None::<String>
            } else {
                Some(entry.body_hash.clone())
            };

            tx.execute(
                "INSERT OR IGNORE INTO documents
                    (content_hash, body_hash, title, summary, topics, structure, is_binary, byte_size, first_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entry.content_hash,
                    body_hash_opt,
                    title,
                    summary,
                    topics_json,
                    structure_json,
                    is_binary_doc,
                    entry.size_bytes,
                    now_str,
                ],
            )?;
        } else if !entry.body_hash.is_empty() && entry.body_hash != entry.content_hash {
            // Update body_hash for existing documents when we have a new one.
            tx.execute(
                "UPDATE documents SET body_hash = ?1 WHERE content_hash = ?2 AND body_hash IS NULL",
                params![entry.body_hash, entry.content_hash],
            )?;
        }
    }

    // 6b. Mark old paths as disappeared (all currently-active rows).
    tx.execute(
        "UPDATE paths SET disappeared = ?1 WHERE disappeared IS NULL",
        params![now_str],
    )?;

    // 6c. Persist current paths.
    // - Short-circuited paths (mtime+size unchanged): un-disappear the existing row
    //   (reset disappeared to NULL) rather than inserting a new one, to avoid
    //   tripping the UNIQUE(content_hash, path, appeared) constraint when the
    //   scan runs within the same second as the previous one.
    // - Changed/new paths: insert a fresh row with appeared=now.
    for entry in &current_entries {
        let is_hardlink = current_inode_to_paths
            .get(&entry.inode)
            .map(|paths| paths.len() > 1)
            .unwrap_or(false);

        if entry.short_circuited {
            // Un-disappear the most-recent matching row.
            tx.execute(
                "UPDATE paths SET disappeared = NULL, is_hardlink = ?1
                 WHERE path = ?2 AND content_hash = ?3 AND disappeared = ?4",
                params![
                    is_hardlink,
                    entry.path.to_string_lossy().as_ref(),
                    entry.content_hash,
                    now_str,
                ],
            )?;
        } else {
            tx.execute(
                "INSERT INTO paths (content_hash, path, root, is_hardlink, mtime, size_bytes, appeared)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    entry.content_hash,
                    entry.path.to_string_lossy().as_ref(),
                    entry.root.to_string_lossy().as_ref(),
                    is_hardlink,
                    entry.mtime,
                    entry.size_bytes,
                    now_str,
                ],
            )?;
        }
    }

    // 6d. Insert events
    for event in &events {
        tx.execute(
            "INSERT INTO events (event_type, content_hash, path, timestamp, file_extension, mime_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.event_type.as_str(),
                event.content_hash,
                event.path,
                event.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
                event.file_extension,
                event.mime_type,
            ],
        )?;
    }

    // 6e. Upsert catalog entries
    let tier2_cataloged = catalog_dirs.len() as u32;
    for (dir_path, (total_bytes, file_count)) in &catalog_dirs {
        let path_str = dir_path.to_string_lossy();
        // Check if already exists.
        let existing: Option<(i64, i64)> = tx.query_row(
            "SELECT total_bytes, file_count FROM catalog WHERE path = ?1",
            params![path_str.as_ref()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).ok();

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
                    now_str,
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
                    now_str,
                ],
            )?;
        }
    }

    // 6f. Record snapshot
    let duration_ms = start.elapsed().as_millis() as u64;
    tx.execute(
        "INSERT INTO snapshots (timestamp, tier1_files_scanned, tier2_dirs_cataloged, events_emitted, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            now_str,
            tier1.total as i64,
            tier2_cataloged as i64,
            events.len() as i64,
            duration_ms as i64,
        ],
    )?;

    tx.commit()?;

    // ------------------------------------------------------------------
    // 7. Prune read_audit (after commit, outside transaction)
    // ------------------------------------------------------------------
    crate::db::prune_audit_log(conn, config.audit_retention_days)?;

    let tier2 = Tier2Summary {
        cataloged: tier2_cataloged,
        total: tier2_cataloged,
    };

    Ok(ScanResult {
        tier1,
        tier2,
        events,
        duration_ms,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk a subtree counting total bytes and file count (for catalog entries).
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

/// Determine the most restrictive of two classifications.
fn most_restrictive(a: PathClassification, b: PathClassification) -> PathClassification {
    // Priority: Ignored > Cataloged > IndexedNoEmbed > Indexed
    let rank = |c: &PathClassification| match c {
        PathClassification::Ignored => 3,
        PathClassification::Cataloged => 2,
        PathClassification::IndexedNoEmbed => 1,
        PathClassification::Indexed => 0,
    };
    if rank(&a) >= rank(&b) { a } else { b }
}

/// Determine what event (if any) to emit for a current path entry.
///
/// Returns `None` if the path is unchanged (short-circuit hit and hash same).
fn determine_event_type(
    entry: &CurrentEntry,
    prev_paths: &HashMap<PathBuf, PrevPathEntry>,
    prev_hash_to_paths: &HashMap<String, Vec<PathBuf>>,
    current_inode_to_paths: &HashMap<u64, Vec<PathBuf>>,
    old_body_hashes: &HashMap<String, String>,
    seen_paths: &std::collections::HashSet<PathBuf>,
) -> Option<EventType> {
    let path = &entry.path;
    let new_hash = &entry.content_hash;

    if let Some(prev) = prev_paths.get(path) {
        // Path existed before.
        if prev.content_hash == *new_hash {
            // Hash unchanged — no event.
            return None;
        }
        // Hash changed — updated or minor_change.
        let old_body = old_body_hashes.get(&prev.content_hash).map(|s| s.as_str()).unwrap_or("");
        let new_body = &entry.body_hash;
        if !old_body.is_empty()
            && !new_body.is_empty()
            && old_body != prev.content_hash.as_str() // body_hash differs from content_hash
            && hasher::detect_minor_change(&prev.content_hash, new_hash, old_body, new_body)
        {
            Some(EventType::MinorChange)
        } else {
            Some(EventType::Updated)
        }
    } else {
        // New path — check if this hash existed at a different path (move/copy/hardlink).
        if let Some(prev_path_list) = prev_hash_to_paths.get(new_hash) {
            // Find a previous path that is now gone.
            let gone_path = prev_path_list
                .iter()
                .find(|p| !seen_paths.contains(*p));

            if let Some(_old_path) = gone_path {
                // Old path gone → Moved.
                return Some(EventType::Moved);
            }

            // All old paths still exist → Copy or Hardlink.
            // Hardlink if same inode shared with another current path.
            let shared_inode = current_inode_to_paths
                .get(&entry.inode)
                .map(|paths| paths.len() > 1)
                .unwrap_or(false);

            if shared_inode {
                Some(EventType::Hardlinked)
            } else {
                Some(EventType::Copied)
            }
        } else {
            // Genuinely new hash at new path.
            Some(EventType::Created)
        }
    }
}

/// Classify a path against a bare `SectionRules` (without a full IgnoreStack).
/// Used to apply the caller-supplied `global_rules` as a pre-filter.
fn classify_section_rules(rules: &SectionRules, path: &Path, is_dir: bool) -> PathClassification {
    if matches!(rules.ignored.matched(path, is_dir), ignore::Match::Ignore(_)) {
        return PathClassification::Ignored;
    }
    if matches!(rules.no_embed.matched(path, is_dir), ignore::Match::Ignore(_)) {
        return PathClassification::IndexedNoEmbed;
    }
    if matches!(rules.cataloged.matched(path, is_dir), ignore::Match::Ignore(_)) {
        return PathClassification::Cataloged;
    }
    PathClassification::Indexed
}
