//! Search, audit, manifest, health, and history queries against the smriti index.

use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::config::Config;
use crate::envelope::FreshnessEnvelope;
use crate::error::{Result, SmritiError};

// ---------------------------------------------------------------------------
// Search (BM25 via FTS5)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub topics: Vec<String>,
    pub content_hash: String,
    pub byte_size: Option<i64>,
    pub embed_excluded: bool,
    pub rank: f64,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub results: Vec<SearchHit>,
    pub total_indexed: i64,
    #[serde(flatten)]
    pub envelope: FreshnessEnvelope,
}

fn escape_fts5_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn search_fts(conn: &Connection, query: &str, k: u32, config: &Config) -> Result<SearchResult> {
    let total_indexed = count_documents(conn)?;
    let envelope = freshness_envelope(conn, config)?;
    let escaped_query = escape_fts5_query(query);

    // Contentless FTS5: column SELECTs from document_fts return NULL, so we
    // join to documents on rowid (FTS rowid == documents.rowid by construction
    // in flush_batch). The JOIN also filters out orphan FTS rows whose
    // documents row was deleted.
    let mut stmt = conn.prepare(
        "SELECT
            d.content_hash,
            d.title,
            d.summary,
            d.topics,
            d.byte_size,
            d.embed_excluded,
            rank
         FROM document_fts f
         JOIN documents d ON d.rowid = f.rowid
         WHERE document_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![escaped_query, k], |row| {
        let content_hash: String = row.get(0)?;
        let title: Option<String> = row.get(1)?;
        let summary: Option<String> = row.get(2)?;
        let topics_json: Option<String> = row.get(3)?;
        let byte_size: Option<i64> = row.get(4)?;
        let embed_excluded: bool = row.get(5)?;
        let rank: f64 = row.get(6)?;
        Ok((
            content_hash,
            title,
            summary,
            topics_json,
            byte_size,
            embed_excluded,
            rank,
        ))
    })?;

    let mut results = Vec::new();
    for row in rows {
        let (content_hash, title, summary, topics_json, byte_size, embed_excluded, rank) = row?;

        let topics: Vec<String> = topics_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();

        let path =
            current_path(conn, &content_hash)?.unwrap_or_else(|| "(no current path)".to_string());

        results.push(SearchHit {
            path,
            title,
            summary,
            topics,
            content_hash,
            byte_size,
            embed_excluded,
            rank,
        });
    }

    Ok(SearchResult {
        results,
        total_indexed,
        envelope,
    })
}

// ---------------------------------------------------------------------------
// Path search (glob/extension against paths table)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PathHit {
    pub path: String,
    pub byte_size: i64,
    pub content_hash: String,
    pub title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PathSearchResult {
    pub results: Vec<PathHit>,
    pub total_matched: usize,
    #[serde(flatten)]
    pub envelope: FreshnessEnvelope,
}

pub fn search_path(
    conn: &Connection,
    pattern: &str,
    limit: u32,
    config: &Config,
) -> Result<PathSearchResult> {
    let envelope = freshness_envelope(conn, config)?;

    let like_pattern = glob_to_like(pattern);
    let esc = "\\";

    let total_matched: usize = conn.query_row(
        "SELECT COUNT(*) FROM paths p
         JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL AND p.path LIKE ?1 ESCAPE ?2",
        params![like_pattern, esc],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT p.path, COALESCE(d.byte_size, 0), p.content_hash, d.title
         FROM paths p
         JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL AND p.path LIKE ?1 ESCAPE ?2
         ORDER BY d.byte_size DESC
         LIMIT ?3",
    )?;

    let mut results = Vec::new();
    let mut rows = stmt.query(params![like_pattern, esc, limit])?;
    while let Some(row) = rows.next()? {
        results.push(PathHit {
            path: row.get(0)?,
            byte_size: row.get(1)?,
            content_hash: row.get(2)?,
            title: row.get(3)?,
        });
    }

    Ok(PathSearchResult {
        results,
        total_matched,
        envelope,
    })
}

pub fn search_extension(
    conn: &Connection,
    ext: &str,
    limit: u32,
    config: &Config,
) -> Result<PathSearchResult> {
    let envelope = freshness_envelope(conn, config)?;

    let like_pattern = format!("%.{}", ext.trim_start_matches('.').to_lowercase());

    let total_matched: usize = conn.query_row(
        "SELECT COUNT(*) FROM paths p
         JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL AND LOWER(p.path) LIKE ?1",
        params![like_pattern],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT p.path, COALESCE(d.byte_size, 0), p.content_hash, d.title
         FROM paths p
         JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL AND LOWER(p.path) LIKE ?1
         ORDER BY d.byte_size DESC
         LIMIT ?2",
    )?;

    let mut results = Vec::new();
    let mut rows = stmt.query(params![like_pattern, limit])?;
    while let Some(row) = rows.next()? {
        results.push(PathHit {
            path: row.get(0)?,
            byte_size: row.get(1)?,
            content_hash: row.get(2)?,
            title: row.get(3)?,
        });
    }

    Ok(PathSearchResult {
        results,
        total_matched,
        envelope,
    })
}

fn glob_to_like(pattern: &str) -> String {
    let mut out = String::new();
    for ch in pattern.chars() {
        match ch {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            '~' => {
                let home = std::env::var("HOME").unwrap_or_default();
                out.push_str(&home);
            }
            _ => out.push(ch),
        }
    }
    if !out.contains('%') && !out.contains('_') {
        out.insert(0, '%');
        out.push('%');
    }
    out
}

/// Hybrid search: BM25 + dense retrieval with RRF merge.
/// Falls back to BM25-only if the embedding feature is disabled or embedder is None.
#[cfg(feature = "embedding")]
pub fn search_hybrid(
    conn: &Connection,
    query: &str,
    k: u32,
    config: &Config,
    embedder: &mut crate::embedding::Embedder,
) -> Result<SearchResult> {
    let total_indexed = count_documents(conn)?;
    let envelope = freshness_envelope(conn, config)?;

    // BM25 leg
    let bm25_result = search_fts(conn, query, k * 2, config)?;
    let bm25_hashes: Vec<String> = bm25_result
        .results
        .iter()
        .map(|h| h.content_hash.clone())
        .collect();

    // Dense leg
    let query_embedding = embedder.embed_text(query)?;
    let dense_results = crate::embedding::search_dense(conn, &query_embedding, k * 2)?;
    let dense_hashes: Vec<String> = dense_results.iter().map(|(h, _)| h.clone()).collect();

    // RRF merge
    let merged = crate::embedding::rrf_merge(&bm25_hashes, &dense_hashes, 60.0);

    let mut results = Vec::new();
    for (rank, content_hash) in merged.iter().enumerate().take(k as usize) {
        let (title, summary, topics_json, byte_size, embed_excluded) = conn.query_row(
            "SELECT title, summary, topics, byte_size, embed_excluded FROM documents WHERE content_hash = ?1",
            params![content_hash],
            |row| Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, bool>(4)?,
            )),
        )?;

        let topics: Vec<String> = topics_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let path =
            current_path(conn, content_hash)?.unwrap_or_else(|| "(no current path)".to_string());

        results.push(SearchHit {
            path,
            title,
            summary,
            topics,
            content_hash: content_hash.clone(),
            byte_size,
            embed_excluded,
            rank: rank as f64,
        });
    }

    Ok(SearchResult {
        results,
        total_indexed,
        envelope,
    })
}

// ---------------------------------------------------------------------------
// Get by content_hash
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DocumentInfo {
    pub path: Option<String>,
    pub all_current_paths: Vec<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub topics: Vec<String>,
    pub content_hash: String,
    pub byte_size: Option<i64>,
    #[serde(flatten)]
    pub envelope: FreshnessEnvelope,
}

pub fn get_document(
    conn: &Connection,
    content_hash: &str,
    config: &Config,
) -> Result<DocumentInfo> {
    let envelope = freshness_envelope(conn, config)?;

    let (title, summary, topics_json, byte_size) = conn
        .query_row(
            "SELECT title, summary, topics, byte_size FROM documents WHERE content_hash = ?1",
            params![content_hash],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .map_err(|_| SmritiError::NotFound {
            entity: "document".to_string(),
            id: content_hash.to_string(),
        })?;

    let topics: Vec<String> = topics_json
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();

    let all_current_paths = all_current_paths(conn, content_hash)?;
    let path = all_current_paths.first().cloned();

    Ok(DocumentInfo {
        path,
        all_current_paths,
        title,
        summary,
        topics,
        content_hash: content_hash.to_string(),
        byte_size,
        envelope,
    })
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct HistoryEvent {
    pub event_type: String,
    pub timestamp: String,
    pub path: String,
    pub previous_path: Option<String>,
    pub previous_hash: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResult {
    pub current_path: Option<String>,
    pub content_hash: Option<String>,
    pub events: Vec<HistoryEvent>,
    pub versions: i64,
    #[serde(flatten)]
    pub envelope: FreshnessEnvelope,
}

pub fn history(
    conn: &Connection,
    path: &str,
    since: Option<&str>,
    until: Option<&str>,
    config: &Config,
) -> Result<HistoryResult> {
    let envelope = freshness_envelope(conn, config)?;

    // Find the content hash(es) associated with this path.
    let content_hash: Option<String> = conn
        .query_row(
            "SELECT content_hash FROM paths WHERE path = ?1 AND disappeared IS NULL LIMIT 1",
            params![path],
            |row| row.get(0),
        )
        .ok();

    let mut sql = String::from(
        "SELECT event_type, timestamp, path, previous_path, previous_hash
         FROM events WHERE path = ?1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(path.to_string())];

    if let Some(s) = since {
        sql.push_str(" AND timestamp >= ?");
        param_values.push(Box::new(s.to_string()));
    }
    if let Some(u) = until {
        sql.push_str(" AND timestamp <= ?");
        param_values.push(Box::new(u.to_string()));
    }
    sql.push_str(" ORDER BY timestamp ASC");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(HistoryEvent {
            event_type: row.get(0)?,
            timestamp: row.get(1)?,
            path: row.get(2)?,
            previous_path: row.get(3)?,
            previous_hash: row.get(4)?,
        })
    })?;

    let events: Vec<HistoryEvent> = rows.filter_map(|r| r.ok()).collect();

    let versions: i64 = if content_hash.is_some() {
        conn.query_row(
            "SELECT COUNT(DISTINCT content_hash) FROM events WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )
        .unwrap_or(0)
    } else {
        0
    };

    let current_path = content_hash
        .as_ref()
        .and_then(|h| current_path(conn, h).ok().flatten());

    Ok(HistoryResult {
        current_path,
        content_hash,
        events,
        versions,
        envelope,
    })
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ExtensionStats {
    pub files: i64,
    pub bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct CatalogEntry {
    pub path: String,
    pub total_bytes: i64,
    pub file_count: i64,
    pub regenerable: bool,
}

#[derive(Debug, Serialize)]
pub struct AuditResult {
    pub tier1_total_files: i64,
    pub tier1_total_bytes: i64,
    pub tier1_by_extension: std::collections::HashMap<String, ExtensionStats>,
    pub tier2_total_dirs: i64,
    pub tier2_total_bytes: i64,
    pub tier2_largest: Vec<CatalogEntry>,
    pub excluded_from_embedding_files: i64,
    pub excluded_from_embedding_bytes: i64,
    pub roots: Vec<String>,
    pub backup_target_bytes: i64,
    #[serde(flatten)]
    pub envelope: FreshnessEnvelope,
}

pub fn audit(
    conn: &Connection,
    min_bytes: Option<u64>,
    sort_by: Option<&str>,
    config: &Config,
) -> Result<AuditResult> {
    let envelope = freshness_envelope(conn, config)?;

    // Tier 1 stats
    let (tier1_total_files, tier1_total_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(d.byte_size), 0)
         FROM paths p JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // By extension: parse from file path
    let mut by_extension = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT p.path, d.byte_size
             FROM paths p JOIN documents d ON d.content_hash = p.content_hash
             WHERE p.disappeared IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        for row in rows {
            let (path, size) = row?;
            let ext = std::path::Path::new(&path)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| "(none)".to_string());
            let entry = by_extension
                .entry(ext)
                .or_insert(ExtensionStats { files: 0, bytes: 0 });
            entry.files += 1;
            entry.bytes += size.unwrap_or(0);
        }
    }

    // Tier 2 stats
    let (tier2_total_dirs, tier2_total_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(total_bytes), 0) FROM catalog",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let order = match sort_by {
        Some("count") => "file_count DESC",
        _ => "total_bytes DESC",
    };

    let min_filter = min_bytes.unwrap_or(0) as i64;
    let sql = format!(
        "SELECT path, total_bytes, file_count, regenerable FROM catalog
         WHERE total_bytes >= ?1
         ORDER BY {order} LIMIT 20"
    );
    let mut stmt = conn.prepare(&sql)?;
    let tier2_largest: Vec<CatalogEntry> = stmt
        .query_map(params![min_filter], |row| {
            Ok(CatalogEntry {
                path: row.get(0)?,
                total_bytes: row.get(1)?,
                file_count: row.get(2)?,
                regenerable: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Embed-excluded
    let (excl_files, excl_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(d.byte_size), 0)
         FROM paths p JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL AND d.embed_excluded = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let roots: Vec<String> = config
        .roots
        .iter()
        .map(|r| r.to_string_lossy().to_string())
        .collect();

    Ok(AuditResult {
        tier1_total_files,
        tier1_total_bytes,
        tier1_by_extension: by_extension,
        tier2_total_dirs,
        tier2_total_bytes,
        tier2_largest,
        excluded_from_embedding_files: excl_files,
        excluded_from_embedding_bytes: excl_bytes,
        roots,
        backup_target_bytes: tier1_total_bytes,
        envelope,
    })
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ManifestResult {
    pub format: String,
    pub entries: Vec<String>,
    #[serde(flatten)]
    pub envelope: FreshnessEnvelope,
}

pub fn manifest(conn: &Connection, format: &str, config: &Config) -> Result<ManifestResult> {
    let envelope = freshness_envelope(conn, config)?;

    let mut stmt = conn.prepare(
        "SELECT p.path, p.content_hash, d.byte_size
         FROM paths p JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM catalog c
               WHERE substr(p.path, 1, length(c.path) + 1) = c.path || '/'
           )
         ORDER BY p.path",
    )?;

    let entries: Vec<String> = if format == "ndjson" {
        stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let hash: String = row.get(1)?;
            let size: Option<i64> = row.get(2)?;
            Ok(serde_json::json!({
                "path": path,
                "content_hash": hash,
                "byte_size": size,
            })
            .to_string())
        })?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect()
    };

    Ok(ManifestResult {
        format: format.to_string(),
        entries,
        envelope,
    })
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct WatcherStatus {
    pub running: bool,
    pub state: String,
    pub pid: i64,
    pub uptime_seconds: i64,
    pub watch_count: i64,
    pub pending_events: i64,
    pub started_at: String,
    pub updated_at: String,
    pub last_event_processed_at: Option<String>,
    pub last_full_scan_at: Option<String>,
    pub last_full_scan_duration_ms: Option<i64>,
}

pub fn read_watcher_status(conn: &Connection) -> Result<Option<WatcherStatus>> {
    let mut stmt = conn.prepare(
        "SELECT pid, started_at, updated_at, state, watch_count, pending_events,
                last_event_processed_at, last_full_scan_at, last_full_scan_duration_ms
         FROM watcher_heartbeat WHERE id = 1",
    )?;

    let row = stmt.query_row([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    });

    match row {
        Ok((
            pid,
            started_at,
            updated_at,
            state,
            watch_count,
            pending_events,
            last_event_processed_at,
            last_full_scan_at,
            last_full_scan_duration_ms,
        )) => {
            let updated = chrono::NaiveDateTime::parse_from_str(&updated_at, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|dt| dt.and_utc());
            let started = chrono::NaiveDateTime::parse_from_str(&started_at, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|dt| dt.and_utc());

            let now = Utc::now();
            let stale = updated
                .map(|u| (now - u).num_seconds() > 30)
                .unwrap_or(true);
            let running = !stale && state != "stopped";
            let uptime_seconds = started.map(|s| (now - s).num_seconds()).unwrap_or(0);

            Ok(Some(WatcherStatus {
                running,
                state,
                pid,
                uptime_seconds,
                watch_count,
                pending_events,
                started_at,
                updated_at,
                last_event_processed_at,
                last_full_scan_at,
                last_full_scan_duration_ms,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(SmritiError::Db(e)),
    }
}

#[derive(Debug, Serialize)]
pub struct HealthResult {
    pub status: String,
    pub db_path: String,
    pub roots: Vec<String>,
    pub total_indexed: i64,
    pub total_cataloged: i64,
    pub last_scan: Option<String>,
    pub embedder_ok: bool,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<WatcherStatus>,
}

pub fn health(conn: &Connection, config: &Config) -> Result<HealthResult> {
    let total_indexed = count_documents(conn)?;

    let total_cataloged: i64 =
        conn.query_row("SELECT COUNT(*) FROM catalog", [], |row| row.get(0))?;

    let last_scan: Option<String> = conn
        .query_row(
            "SELECT timestamp FROM snapshots ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    let roots: Vec<String> = config
        .roots
        .iter()
        .map(|r| r.to_string_lossy().to_string())
        .collect();

    let watcher = read_watcher_status(conn).unwrap_or(None);

    Ok(HealthResult {
        status: "ok".to_string(),
        db_path: config.db_path.to_string_lossy().to_string(),
        roots,
        total_indexed,
        total_cataloged,
        last_scan,
        embedder_ok: config.model_path.is_some(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        watcher,
    })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn count_documents(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?)
}

fn current_path(conn: &Connection, content_hash: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT path FROM paths WHERE content_hash = ?1 AND disappeared IS NULL LIMIT 1",
            params![content_hash],
            |row| row.get(0),
        )
        .ok())
}

fn all_current_paths(conn: &Connection, content_hash: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT path FROM paths WHERE content_hash = ?1 AND disappeared IS NULL ORDER BY path",
    )?;
    let paths: Vec<String> = stmt
        .query_map(params![content_hash], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(paths)
}

fn freshness_envelope(conn: &Connection, config: &Config) -> Result<FreshnessEnvelope> {
    let last_scan: Option<String> = conn
        .query_row(
            "SELECT timestamp FROM snapshots ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    let as_of = last_scan
        .and_then(|s| chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").ok())
        .map(|naive| naive.and_utc())
        .unwrap_or_else(Utc::now);

    Ok(FreshnessEnvelope::new(as_of, config.stale_threshold_sec))
}
