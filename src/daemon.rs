//! Daemon entry point: runs the MCP server over stdio or streamable HTTP.

use std::sync::{Arc, Mutex};

use rmcp::ServiceExt;

use crate::config::Config;
use crate::mcp::SmritiServer;

pub async fn run_stdio(config: Config) -> anyhow::Result<()> {
    // Serve is read-only — migrations are watcher's responsibility (ADR 0001).
    // ScanEnqueuer is the only write path, narrowed to scan_requests INSERTs.
    // Read connections are opened per tool call (smriti/31): a long-lived
    // readonly connection wedges on FTS5 over time.
    let enqueuer = crate::db::ScanEnqueuer::open(&config.db_path)?;
    let enqueue_db = Arc::new(Mutex::new(enqueuer));
    let audit_dir = config.db_path.parent().unwrap_or(std::path::Path::new("."));
    let audit_conn = crate::db::open_audit(audit_dir)?;
    let audit_db = Arc::new(Mutex::new(audit_conn));
    let cfg = Arc::new(config);
    let server = SmritiServer::new(enqueue_db, audit_db, cfg);

    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}

pub async fn run_http(config: Config, host: &str, port: u16) -> anyhow::Result<()> {
    use axum::routing::any_service;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };
    use tokio_util::sync::CancellationToken;

    let enqueuer = crate::db::ScanEnqueuer::open(&config.db_path)?;
    let enqueue_db = Arc::new(Mutex::new(enqueuer));
    let audit_dir = config.db_path.parent().unwrap_or(std::path::Path::new("."));
    let audit_conn = crate::db::open_audit(audit_dir)?;
    let audit_db = Arc::new(Mutex::new(audit_conn));
    let cfg = Arc::new(config);

    let cancel = CancellationToken::new();

    let http_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_cancellation_token(cancel.clone());

    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = None;
    let session_manager = Arc::new(session_manager);

    let enqueue_clone = Arc::clone(&enqueue_db);
    let audit_clone = Arc::clone(&audit_db);
    let cfg_clone = Arc::clone(&cfg);
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(SmritiServer::new(
                Arc::clone(&enqueue_clone),
                Arc::clone(&audit_clone),
                Arc::clone(&cfg_clone),
            ))
        },
        session_manager,
        http_config,
    );

    // rmcp requires Accept to contain both application/json and text/event-stream,
    // but some clients (e.g. Claude Code) omit it. Normalize before rmcp sees it.
    let normalize_accept = axum::middleware::from_fn(
        |mut req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
            use axum::http::header::ACCEPT;
            let dominated = req
                .headers()
                .get(ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| {
                    !v.contains("application/json") || !v.contains("text/event-stream")
                });
            if dominated {
                req.headers_mut().insert(
                    ACCEPT,
                    "application/json, text/event-stream".parse().unwrap(),
                );
            }
            next.run(req).await
        },
    );

    #[allow(deprecated)]
    let app = axum::Router::new()
        .route("/mcp", any_service(mcp_service))
        .layer(normalize_accept);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e} — is the port in use?"))?;
    tracing::info!(%addr, "smriti HTTP MCP server listening");

    let cancel_for_shutdown = cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutdown signal received, draining connections");
            cancel_for_shutdown.cancel();
        })
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server exited with error: {e}"))?;

    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    // Non-Unix fallback: Ctrl-C only.
    let _ = tokio::signal::ctrl_c().await;
}
