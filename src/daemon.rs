//! Daemon entry point: runs the MCP server over stdio.
//!
//! v0.1 uses stdio transport (MCP subprocess pattern).
//! Unix socket transport for long-lived daemon is planned for v0.2.

use rmcp::ServiceExt;

use crate::config::Config;
use crate::mcp::SmritiServer;

pub async fn run_stdio(config: Config) -> anyhow::Result<()> {
    let conn = crate::db::open(&config.db_path)?;
    let server = SmritiServer::new(conn, config);

    let transport = rmcp::transport::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;

    Ok(())
}
