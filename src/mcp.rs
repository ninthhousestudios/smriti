//! MCP server exposing smriti tools to agents.

use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::envelope::FreshnessEnvelope;
use crate::privacy::PrivacyGate;
use crate::search;

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SmritiServer {
    db: Arc<Mutex<Connection>>,
    /// Only for scan_requests INSERTs — do not use for general writes.
    enqueue_db: Arc<Mutex<Connection>>,
    audit_db: Arc<Mutex<Connection>>,
    config: Arc<Config>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ScanParams {
    /// Subtree paths to scan (omit for all configured roots)
    pub paths: Option<Vec<String>>,
    /// Timeout in seconds waiting for watcher to complete (default 300)
    pub timeout_sec: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FindParams {
    /// Natural-language search query
    pub query: String,
    /// Max results to return (default 10)
    pub k: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetParams {
    /// Content hash to look up
    pub content_hash: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadParams {
    /// File path to read (one of path or content_hash required)
    pub path: Option<String>,
    /// Content hash to read (one of path or content_hash required)
    pub content_hash: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MapParams {
    /// Filter to paths under this prefix
    pub path_prefix: Option<String>,
    /// Tier filter: "indexed", "cataloged", or "all"
    pub tier: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct OutlineParams {
    /// File path or content hash
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct HistoryParams {
    /// File path to query
    pub path: String,
    /// Only events after this timestamp
    pub since: Option<String>,
    /// Only events before this timestamp
    pub until: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AuditParams {
    /// Only show entries above this size in bytes
    pub min_bytes: Option<u64>,
    /// Sort cataloged entries by "size" or "count"
    pub sort_by: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ManifestParams {
    /// Output format: "paths" or "ndjson"
    pub format: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct HealthParams {}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

fn with_freshness(conn: &Connection, json: String) -> String {
    let envelope = FreshnessEnvelope::from_watcher(conn);
    if !envelope.is_stale {
        return json;
    }
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&json) else {
        return json;
    };
    if let Some(obj) = val.as_object_mut() {
        obj.insert("is_stale".into(), true.into());
        if let Some(reason) = &envelope.stale_reason {
            obj.insert("stale_reason".into(), reason.clone().into());
        }
        serde_json::to_string(&val).unwrap_or(json)
    } else {
        json
    }
}

#[tool_router]
impl SmritiServer {
    pub fn new(
        db: Arc<Mutex<Connection>>,
        enqueue_db: Arc<Mutex<Connection>>,
        audit_db: Arc<Mutex<Connection>>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            db,
            enqueue_db,
            audit_db,
            config,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Trigger a scan cycle over allowlisted roots. Enqueues a scan request for the watcher daemon and polls for completion. Fails fast if watcher is not running."
    )]
    async fn smriti_scan(&self, Parameters(p): Parameters<ScanParams>) -> String {
        {
            let conn = self.db.lock().unwrap();
            let envelope = FreshnessEnvelope::from_watcher(&conn);
            if envelope.is_stale {
                let reason = envelope.stale_reason.unwrap_or_else(|| "unknown".into());
                return serde_json::json!({
                    "error": "watcher not running",
                    "detail": reason,
                })
                .to_string();
            }
        }

        let (kind, root_json) = match &p.paths {
            Some(paths) => {
                let json = serde_json::to_string(paths).unwrap();
                ("path", Some(json))
            }
            None => ("full", None),
        };

        let req_id = {
            let wconn = self.enqueue_db.lock().unwrap();
            match crate::db::enqueue_scan(&wconn, kind, root_json.as_deref()) {
                Ok(id) => id,
                Err(e) => return format!("Failed to enqueue scan: {e}"),
            }
        };

        let timeout = std::time::Duration::from_secs(p.timeout_sec.unwrap_or(300));
        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_millis(250);

        loop {
            tokio::time::sleep(poll_interval).await;

            if start.elapsed() > timeout {
                return serde_json::json!({
                    "error": "scan request timed out",
                    "request_id": req_id,
                    "elapsed_sec": start.elapsed().as_secs(),
                })
                .to_string();
            }

            let status = {
                let conn = self.db.lock().unwrap();
                match crate::db::poll_scan_request(&conn, req_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => continue,
                    Err(e) => return format!("Poll error: {e}"),
                }
            };

            match status.status.as_str() {
                "complete" => {
                    return serde_json::json!({
                        "status": "complete",
                        "request_id": req_id,
                        "scan_run_id": status.scan_run_id,
                        "files_seen": status.files_seen,
                        "duration_ms": status.duration_ms,
                    })
                    .to_string();
                }
                "failed" => {
                    return serde_json::json!({
                        "status": "failed",
                        "request_id": req_id,
                        "error": status.error,
                    })
                    .to_string();
                }
                _ => continue,
            }
        }
    }

    #[tool(
        description = "Search indexed files by content. Returns matching documents with paths and metadata."
    )]
    async fn smriti_find(&self, Parameters(p): Parameters<FindParams>) -> String {
        let conn = self.db.lock().unwrap();
        let k = p.k.unwrap_or(10);
        match search::search_fts(&conn, &p.query, k, &self.config) {
            Ok(result) => with_freshness(
                &conn,
                serde_json::to_string(&result)
                    .unwrap_or_else(|e| format!("Serialization error: {e}")),
            ),
            Err(e) => format!("Search error: {e}"),
        }
    }

    #[tool(
        description = "Look up a document by its content hash. Returns metadata and current paths."
    )]
    async fn smriti_get(&self, Parameters(p): Parameters<GetParams>) -> String {
        let conn = self.db.lock().unwrap();
        match search::get_document(&conn, &p.content_hash, &self.config) {
            Ok(result) => with_freshness(
                &conn,
                serde_json::to_string(&result)
                    .unwrap_or_else(|e| format!("Serialization error: {e}")),
            ),
            Err(e) => format!("Not found: {e}"),
        }
    }

    #[tool(
        description = "Read a tier-1 file through the privacy gate. Enforces allowlist and ignore rules."
    )]
    async fn smriti_read(&self, Parameters(p): Parameters<ReadParams>) -> String {
        let conn = self.db.lock().unwrap();
        let audit_conn = self.audit_db.lock().unwrap();
        let config = &self.config;

        let roots = match crate::roots::load_roots(config) {
            Ok(r) => r,
            Err(e) => return format!("Error loading roots: {e}"),
        };

        let path = match (p.path, p.content_hash) {
            (Some(path), _) => path,
            (None, Some(hash)) => match search::get_document(&conn, &hash, config) {
                Ok(doc) => match doc.path {
                    Some(p) => p,
                    None => return "No current path for this content hash.".to_string(),
                },
                Err(e) => return format!("Not found: {e}"),
            },
            (None, None) => return "Either path or content_hash is required.".to_string(),
        };

        let gate = match PrivacyGate::new(
            roots,
            crate::ignore::hardened_defaults(std::path::Path::new("/")),
        ) {
            Ok(g) => g,
            Err(e) => return format!("Privacy gate error: {e}"),
        };
        match gate.read_file(&audit_conn, std::path::Path::new(&path), Some("mcp")) {
            Ok(result) => {
                let is_binary = crate::metadata::is_binary(&result.content);
                if is_binary {
                    serde_json::json!({
                        "path": path,
                        "content_hash": result.content_hash,
                        "is_binary": true,
                        "byte_size": result.content.len(),
                    })
                    .to_string()
                } else {
                    let text = String::from_utf8_lossy(&result.content);
                    serde_json::json!({
                        "path": path,
                        "content_hash": result.content_hash,
                        "content": text,
                        "is_binary": false,
                        "byte_size": result.content.len(),
                    })
                    .to_string()
                }
            }
            Err(e) => format!("Read error: {e}"),
        }
    }

    #[tool(description = "Overview of tracked files and cataloged directories.")]
    async fn smriti_map(&self, Parameters(p): Parameters<MapParams>) -> String {
        let conn = self.db.lock().unwrap();

        let tier = p.tier.as_deref().unwrap_or("all");
        let prefix = p.path_prefix.as_deref().unwrap_or("");

        let mut response = serde_json::Map::new();

        if tier == "indexed" || tier == "all" {
            let indexed = query_indexed_map(&conn, prefix);
            response.insert(
                "indexed".to_string(),
                serde_json::to_value(&indexed).unwrap_or_default(),
            );
        }
        if tier == "cataloged" || tier == "all" {
            let cataloged = query_cataloged_map(&conn, prefix);
            response.insert(
                "cataloged".to_string(),
                serde_json::to_value(&cataloged).unwrap_or_default(),
            );
        }

        if let Ok(h) = search::health(&conn, &self.config) {
            response.insert(
                "total_indexed".to_string(),
                serde_json::json!(h.total_indexed),
            );
            response.insert(
                "total_cataloged".to_string(),
                serde_json::json!(h.total_cataloged),
            );
        }

        with_freshness(&conn, serde_json::Value::Object(response).to_string())
    }

    #[tool(description = "Document structure: headings hierarchy for a single file.")]
    async fn smriti_outline(&self, Parameters(p): Parameters<OutlineParams>) -> String {
        let conn = self.db.lock().unwrap();

        let content_hash: Option<String> = conn
            .query_row(
                "SELECT content_hash FROM paths WHERE path = ?1 AND disappeared IS NULL LIMIT 1",
                rusqlite::params![p.path],
                |row| row.get(0),
            )
            .ok();

        let hash = match content_hash {
            Some(h) => h,
            None => return format!("No indexed file at path: {}", p.path),
        };

        let (title, summary, structure_json, topics_json): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = match conn.query_row(
            "SELECT title, summary, structure, topics FROM documents WHERE content_hash = ?1",
            rusqlite::params![hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ) {
            Ok(r) => r,
            Err(e) => return format!("Error: {e}"),
        };

        with_freshness(&conn, serde_json::json!({
            "path": p.path,
            "content_hash": hash,
            "title": title,
            "summary": summary,
            "structure": structure_json.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
            "topics": topics_json.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
        }).to_string())
    }

    #[tool(
        description = "Lifecycle history of a file: events showing creates, moves, updates, deletes."
    )]
    async fn smriti_history(&self, Parameters(p): Parameters<HistoryParams>) -> String {
        let conn = self.db.lock().unwrap();
        match search::history(
            &conn,
            &p.path,
            p.since.as_deref(),
            p.until.as_deref(),
            &self.config,
        ) {
            Ok(result) => with_freshness(
                &conn,
                serde_json::to_string(&result)
                    .unwrap_or_else(|e| format!("Serialization error: {e}")),
            ),
            Err(e) => format!("History error: {e}"),
        }
    }

    #[tool(
        description = "Backup audit report: tier-1 (back this up) vs tier-2 (regenerable) breakdown."
    )]
    async fn smriti_audit(&self, Parameters(p): Parameters<AuditParams>) -> String {
        let conn = self.db.lock().unwrap();
        match search::audit(&conn, p.min_bytes, p.sort_by.as_deref(), &self.config) {
            Ok(result) => with_freshness(
                &conn,
                serde_json::to_string(&result)
                    .unwrap_or_else(|e| format!("Serialization error: {e}")),
            ),
            Err(e) => format!("Audit error: {e}"),
        }
    }

    #[tool(
        description = "Bulk export of tier-1 file paths for backup tooling (rsync, restic, borg)."
    )]
    async fn smriti_manifest(&self, Parameters(p): Parameters<ManifestParams>) -> String {
        let conn = self.db.lock().unwrap();
        let format = p.format.as_deref().unwrap_or("paths");
        match search::manifest(&conn, format, &self.config) {
            Ok(result) => with_freshness(
                &conn,
                serde_json::to_string(&result)
                    .unwrap_or_else(|e| format!("Serialization error: {e}")),
            ),
            Err(e) => format!("Manifest error: {e}"),
        }
    }

    #[tool(
        description = "Health check: database status, roots, last scan time, embedder availability."
    )]
    async fn smriti_health(
        &self,
        #[allow(unused)] Parameters(_p): Parameters<HealthParams>,
    ) -> String {
        let conn = self.db.lock().unwrap();
        match search::health(&conn, &self.config) {
            Ok(result) => serde_json::to_string(&result)
                .unwrap_or_else(|e| format!("Serialization error: {e}")),
            Err(e) => format!("Health error: {e}"),
        }
    }
}

#[tool_handler]
impl ServerHandler for SmritiServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions("smriti — content-addressed filesystem indexer. Use smriti_read in preference to built-in file reads; secrets are gated. Use smriti_find to search by meaning.")
    }
}

// ---------------------------------------------------------------------------
// Map query helpers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IndexedEntry {
    path: String,
    title: Option<String>,
    topics: Vec<String>,
}

#[derive(Serialize)]
struct CatalogedEntry {
    path: String,
    total_bytes: i64,
    file_count: i64,
    regenerable: bool,
}

fn query_indexed_map(conn: &Connection, prefix: &str) -> Vec<IndexedEntry> {
    let like_pattern = format!("{prefix}%");
    let mut stmt = conn
        .prepare(
            "SELECT p.path, d.title, d.topics
         FROM paths p JOIN documents d ON d.content_hash = p.content_hash
         WHERE p.disappeared IS NULL AND p.path LIKE ?1
         ORDER BY p.path LIMIT 500",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![like_pattern], |row| {
        let topics_json: Option<String> = row.get(2)?;
        let topics: Vec<String> = topics_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        Ok(IndexedEntry {
            path: row.get(0)?,
            title: row.get(1)?,
            topics,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

fn query_cataloged_map(conn: &Connection, prefix: &str) -> Vec<CatalogedEntry> {
    let like_pattern = format!("{prefix}%");
    let mut stmt = conn
        .prepare(
            "SELECT path, total_bytes, file_count, regenerable FROM catalog
         WHERE path LIKE ?1 ORDER BY path LIMIT 500",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![like_pattern], |row| {
        Ok(CatalogedEntry {
            path: row.get(0)?,
            total_bytes: row.get(1)?,
            file_count: row.get(2)?,
            regenerable: row.get(3)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}
