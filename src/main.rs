use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use smriti::config::Config;
use smriti::roots;
use smriti::search;

#[derive(Parser)]
#[command(name = "smriti", about = "Content-addressed filesystem indexer", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize smriti database and config directory
    Init,
    /// Scan allowlisted roots for changes
    Scan {
        #[arg(long)]
        paths: Option<Vec<PathBuf>>,
        /// Max parallel hash threads (default: all cores)
        #[arg(short = 'j', long)]
        jobs: Option<usize>,
    },
    /// Show backup audit report (summary by default)
    Audit {
        #[arg(long)]
        min_bytes: Option<u64>,
        #[arg(long)]
        sort_by: Option<String>,
        /// Show all extensions and tier-2 entries
        #[arg(long)]
        full: bool,
        /// Drill into a specific extension (e.g., --ext .iso)
        #[arg(long)]
        ext: Option<String>,
        /// Show only tier-2 catalog entries
        #[arg(long)]
        tier2: bool,
    },
    /// Export tier-1 file paths for backup tooling
    Manifest {
        #[arg(long, default_value = "paths")]
        format: String,
    },
    /// Search indexed files by content (default) or by path/extension
    Find {
        /// FTS search query (omit if using --path or --ext)
        query: Option<String>,
        #[arg(short, default_value = "10")]
        k: u32,
        /// Search by path glob (e.g., "*.iso", "~/Downloads/**")
        #[arg(long)]
        path: Option<String>,
        /// Search by file extension (e.g., .iso)
        #[arg(long)]
        ext: Option<String>,
        /// Max results to display (default 200)
        #[arg(long, default_value = "200")]
        limit: u32,
    },
    /// Look up a document by content hash
    Get {
        content_hash: String,
    },
    /// Show lifecycle history for a file path
    History {
        path: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
    /// Manage allowlisted roots
    Roots {
        #[command(subcommand)]
        action: RootsAction,
    },
    /// Prune old events and audit log entries
    Prune {
        #[arg(long)]
        older_than: Option<String>,
        #[arg(long)]
        keep_versions: Option<u32>,
    },
    /// Show health status
    Health,
    /// Show status of the most recent scan run
    ScanStatus,
    /// Run the MCP server
    Serve {
        /// Port to listen on (default: 7333)
        #[arg(short, long, default_value = "7333")]
        port: u16,
        /// Host to bind to (default: 127.0.0.1)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Use stdio transport instead of HTTP
        #[arg(long)]
        stdio: bool,
    },
    /// Watch filesystem for changes and update index in real time
    Watch,
    /// Analyze index and recommend tier reclassifications
    Triage,
    /// Compare a root against other roots to find redundant, unique, and stale files
    BackupAudit {
        /// Root to audit (e.g., /mnt/usb-backup)
        root: PathBuf,
    },
    /// Install systemd user service for smriti-watch
    InstallServices {
        /// Enable and start the service after installing
        #[arg(long)]
        enable: bool,
    },
}

#[derive(Subcommand)]
enum RootsAction {
    Add { path: PathBuf },
    Remove { path: PathBuf },
    Enable { path: PathBuf },
    Disable { path: PathBuf },
    List,
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config = Config::from_env()?;

    match cli.command {
        Commands::Init => cmd_init(&config)?,
        Commands::Scan { paths, jobs } => cmd_scan(&config, paths, jobs)?,
        Commands::Audit { min_bytes, sort_by, full, ext, tier2 } => cmd_audit(&config, min_bytes, sort_by, full, ext.as_deref(), tier2)?,
        Commands::Manifest { format } => cmd_manifest(&config, &format)?,
        Commands::Find { query, k, path, ext, limit } => cmd_find(&config, query.as_deref(), k, path.as_deref(), ext.as_deref(), limit)?,
        Commands::Get { content_hash } => cmd_get(&config, &content_hash)?,
        Commands::History { path, since, until } => cmd_history(&config, &path, since, until)?,
        Commands::Roots { action } => cmd_roots(action)?,
        Commands::Prune { older_than, .. } => cmd_prune(&config, older_than)?,
        Commands::Health => cmd_health(&config)?,
        Commands::ScanStatus => cmd_scan_status(&config)?,
        Commands::Serve { port, host, stdio } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            if stdio {
                rt.block_on(smriti::daemon::run_stdio(config))?;
            } else {
                rt.block_on(smriti::daemon::run_http(config, &host, port))?;
            }
        }
        Commands::Watch => smriti::watcher::run_watch(&config)?,
        Commands::Triage => cmd_triage(&config)?,
        Commands::BackupAudit { root } => cmd_backup_audit(&config, &root)?,
        Commands::InstallServices { enable } => cmd_install_services(enable)?,
    }

    Ok(())
}

fn cmd_init(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _conn = smriti::db::open(&config.db_path)?;
    println!("Initialized smriti database at {}", config.db_path.display());
    Ok(())
}


fn cmd_scan(config: &Config, filter_paths: Option<Vec<PathBuf>>, jobs: Option<usize>) -> Result<()> {
    if smriti::db::watcher_holds_lock(&config.db_path) {
        return cmd_scan_via_watcher(config, filter_paths);
    }

    let _lock = smriti::db::acquire_writer_lock(&config.db_path)?;
    let (mut conn, scan_config, global_rules) =
        smriti::scanner::prepare_scan(config, filter_paths, jobs)?;
    let result = smriti::scanner::scan(&mut conn, &scan_config, &global_rules)?;

    println!("Scan complete in {}ms", result.duration_ms);
    println!(
        "Tier 1: {} created, {} updated, {} moved, {} deleted ({} total events)",
        result.tier1.created,
        result.tier1.updated,
        result.tier1.moved,
        result.tier1.deleted,
        result.tier1.total,
    );
    println!(
        "Tier 2: {} dirs cataloged",
        result.tier2.cataloged,
    );
    Ok(())
}

fn cmd_scan_via_watcher(config: &Config, filter_paths: Option<Vec<PathBuf>>) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;

    let (kind, root_json) = match &filter_paths {
        Some(paths) => ("path", Some(serde_json::to_string(paths)?)),
        None => ("full", None),
    };

    let req_id = smriti::db::enqueue_scan(&conn, kind, root_json.as_deref())?;
    eprintln!("Watcher is running — enqueued scan request {req_id}, polling...");

    let timeout = std::time::Duration::from_secs(300);
    let start = std::time::Instant::now();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));

        if start.elapsed() > timeout {
            anyhow::bail!("scan request {req_id} timed out");
        }

        let Some(status) = smriti::db::poll_scan_request(&conn, req_id)? else {
            continue;
        };

        match status.status.as_str() {
            "complete" => {
                if let Some(ms) = status.duration_ms {
                    println!("Scan complete in {ms}ms (via watcher)");
                } else {
                    println!("Scan complete (via watcher)");
                }
                if let Some(files) = status.files_seen {
                    println!("Files seen: {files}");
                }
                return Ok(());
            }
            "failed" => {
                let err = status.error.unwrap_or_else(|| "unknown error".into());
                anyhow::bail!("scan failed: {err}");
            }
            _ => continue,
        }
    }
}

fn cmd_audit(config: &Config, min_bytes: Option<u64>, sort_by: Option<String>, full: bool, ext: Option<&str>, tier2: bool) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let mut audit_config = config.clone();
    audit_config.roots = roots::load_roots(config)?;

    if let Some(ext) = ext {
        let result = search::search_extension(&conn, ext, u32::MAX, config)?;
        return print_path_results(&result, &format!("extension .{}", ext.trim_start_matches('.')));
    }

    let result = search::audit(&conn, min_bytes, sort_by.as_deref(), &audit_config)?;

    if tier2 {
        println!("Tier 2 (cataloged — regenerable, don't back up):");
        println!("  Dirs:  {}", result.tier2_total_dirs);
        println!("  Size:  {}", format_bytes(result.tier2_total_bytes));
        if !result.tier2_largest.is_empty() {
            for entry in &result.tier2_largest {
                println!(
                    "  {:<10}  {} ({} files){}",
                    format_bytes(entry.total_bytes),
                    entry.path,
                    entry.file_count,
                    if entry.regenerable { " [regenerable]" } else { "" },
                );
            }
        }
        return Ok(());
    }

    let ext_limit = if full { usize::MAX } else { 5 };
    let tier2_limit = if full { usize::MAX } else { 5 };

    println!("=== Backup Audit ===\n");
    println!("Roots: {}", if result.roots.is_empty() { "(none)".to_string() } else { result.roots.join(", ") });
    println!();

    println!("Tier 1 (indexed — back this up):");
    println!("  Files: {}", result.tier1_total_files);
    println!("  Size:  {}", format_bytes(result.tier1_total_bytes));
    if !result.tier1_by_extension.is_empty() {
        println!("  By extension:");
        let mut exts: Vec<_> = result.tier1_by_extension.iter().collect();
        exts.sort_by_key(|e| std::cmp::Reverse(e.1.bytes));
        for (ext, stats) in exts.iter().take(ext_limit) {
            println!("    {:<12} {:>6} files  {}", ext, stats.files, format_bytes(stats.bytes));
        }
        if exts.len() > ext_limit {
            println!("    ... and {} more extensions (use --full to see all)", exts.len() - ext_limit);
        }
    }
    println!();

    println!("Tier 2 (cataloged — regenerable, don't back up):");
    println!("  Dirs:  {}", result.tier2_total_dirs);
    println!("  Size:  {}", format_bytes(result.tier2_total_bytes));
    if !result.tier2_largest.is_empty() {
        println!("  Largest:");
        for entry in result.tier2_largest.iter().take(tier2_limit) {
            println!(
                "    {}  {} ({} files)",
                format_bytes(entry.total_bytes),
                entry.path,
                entry.file_count,
            );
        }
        if result.tier2_largest.len() > tier2_limit {
            println!("    ... and {} more (use --full or --tier2 to see all)", result.tier2_largest.len() - tier2_limit);
        }
    }
    println!();

    if result.excluded_from_embedding_files > 0 {
        println!(
            "Embedding-excluded: {} files ({})",
            result.excluded_from_embedding_files,
            format_bytes(result.excluded_from_embedding_bytes),
        );
        println!();
    }

    println!("Backup target: {}", format_bytes(result.backup_target_bytes));
    println!("Freshness: as_of={}, stale={}", result.envelope.as_of, result.envelope.is_stale);

    Ok(())
}

fn cmd_manifest(config: &Config, format: &str) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let result = search::manifest(&conn, format, config)?;

    for entry in &result.entries {
        println!("{entry}");
    }

    Ok(())
}

fn cmd_find(config: &Config, query: Option<&str>, k: u32, path: Option<&str>, ext: Option<&str>, limit: u32) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;

    if let Some(ext) = ext {
        let result = search::search_extension(&conn, ext, limit, config)?;
        return print_path_results(&result, &format!("extension .{}", ext.trim_start_matches('.')));
    }

    if let Some(pattern) = path {
        let result = search::search_path(&conn, pattern, limit, config)?;
        return print_path_results(&result, &format!("path {pattern}"));
    }

    let query = query.ok_or_else(|| anyhow::anyhow!("provide a query, or use --path/--ext"))?;
    let result = search::search_fts(&conn, query, k, config)?;

    if result.results.is_empty() {
        println!("No results for: {query}");
        return Ok(());
    }

    println!("Found {} results (of {} indexed):\n", result.results.len(), result.total_indexed);
    for (i, hit) in result.results.iter().enumerate() {
        let title = hit.title.as_deref().unwrap_or("(untitled)");
        println!("{}. {} [{}]", i + 1, title, hit.content_hash.get(..12).unwrap_or(&hit.content_hash));
        println!("   Path: {}", hit.path);
        if let Some(ref summary) = hit.summary {
            println!("   {summary}");
        }
        if !hit.topics.is_empty() {
            println!("   Topics: {}", hit.topics.join(", "));
        }
        println!();
    }

    Ok(())
}

fn print_path_results(result: &search::PathSearchResult, label: &str) -> Result<()> {
    if result.results.is_empty() {
        println!("No files matching {label}");
        return Ok(());
    }

    let total_bytes: i64 = result.results.iter().map(|h| h.byte_size).sum();
    let showing = result.results.len();
    let total = result.total_matched;

    if showing < total {
        println!("Showing {} of {} files matching {} ({} in shown):\n",
            showing, total, label, format_bytes(total_bytes));
    } else {
        println!("Found {} files matching {} ({}):\n", total, label, format_bytes(total_bytes));
    }
    for hit in &result.results {
        let size = format_bytes(hit.byte_size);
        println!("  {:<10}  {}", size, hit.path);
    }
    if showing < total {
        println!("\n  ... {} more (use --limit to show more)", total - showing);
    }

    Ok(())
}

fn cmd_get(config: &Config, content_hash: &str) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let doc = search::get_document(&conn, content_hash, config)?;

    let title = doc.title.as_deref().unwrap_or("(untitled)");
    println!("{title}");
    println!("  Hash:  {}", doc.content_hash);
    if let Some(ref path) = doc.path {
        println!("  Path:  {path}");
    }
    if doc.all_current_paths.len() > 1 {
        println!("  All paths:");
        for p in &doc.all_current_paths {
            println!("    {p}");
        }
    }
    if let Some(ref summary) = doc.summary {
        println!("  Summary: {summary}");
    }
    if !doc.topics.is_empty() {
        println!("  Topics: {}", doc.topics.join(", "));
    }
    if let Some(size) = doc.byte_size {
        println!("  Size: {}", format_bytes(size));
    }

    Ok(())
}

fn cmd_history(config: &Config, path: &str, since: Option<String>, until: Option<String>) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let result = search::history(&conn, path, since.as_deref(), until.as_deref(), config)?;

    if let Some(ref current) = result.current_path {
        println!("Current path: {current}");
    }
    if let Some(ref hash) = result.content_hash {
        println!("Content hash: {hash}");
    }
    println!("Versions: {}", result.versions);
    println!();

    if result.events.is_empty() {
        println!("No events found for: {path}");
    } else {
        for event in &result.events {
            let mut line = format!("[{}] {} {}", event.timestamp, event.event_type, event.path);
            if let Some(ref prev_path) = event.previous_path {
                line.push_str(&format!(" (from {prev_path})"));
            }
            println!("{line}");
        }
    }

    Ok(())
}

fn cmd_roots(action: RootsAction) -> Result<()> {
    match action {
        RootsAction::Add { path } => {
            let abs = abs_path(&path)?;
            roots::add_root(&abs)?;
            println!("Added root: {}", abs.display());
        }
        RootsAction::Remove { path } => {
            let abs = abs_path(&path)?;
            roots::remove_root(&abs)?;
            println!("Removed root: {}", abs.display());
        }
        RootsAction::Enable { path } => {
            let abs = abs_path(&path)?;
            roots::enable_root(&abs)?;
            println!("Enabled root: {}", abs.display());
        }
        RootsAction::Disable { path } => {
            let abs = abs_path(&path)?;
            roots::disable_root(&abs)?;
            println!("Disabled root: {}", abs.display());
        }
        RootsAction::List => {
            let list = roots::list_all_roots()?;
            if list.is_empty() {
                println!("No roots configured.");
            } else {
                for e in &list {
                    let status = if e.enabled { "enabled" } else { "disabled" };
                    println!("[{status}] {}", e.path.display());
                }
            }
        }
    }
    Ok(())
}

fn abs_path(path: &PathBuf) -> Result<PathBuf> {
    Ok(if path.is_relative() {
        std::env::current_dir()?.join(path)
    } else {
        path.clone()
    })
}

fn cmd_prune(config: &Config, older_than: Option<String>) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;

    let threshold = older_than
        .map(|s| parse_duration_string(&s))
        .transpose()?
        .unwrap_or(std::time::Duration::from_secs(30 * 86400));

    let events_pruned = smriti::db::prune_events(&conn, threshold)?;

    let audit_dir = config.db_path.parent().unwrap_or(std::path::Path::new("."));
    let audit_pruned = if audit_dir.join("audit.db").exists() {
        let audit_conn = smriti::db::open_audit(audit_dir)?;
        smriti::db::prune_audit_log(&audit_conn, config.audit_retention_days)?
    } else {
        0
    };

    println!("Pruned {events_pruned} old events, {audit_pruned} audit log entries.");
    Ok(())
}

fn cmd_health(config: &Config) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let result = search::health(&conn, config)?;

    println!("Status:    {}", result.status);
    println!("DB:        {}", result.db_path);
    println!("Version:   {}", result.version);
    println!("Indexed:   {} documents", result.total_indexed);
    println!("Cataloged: {} directories", result.total_cataloged);
    if let Some(ref scan) = result.last_scan {
        println!("Last scan: {scan}");
    } else {
        println!("Last scan: never");
    }
    println!("Embedder:  {}", if result.embedder_ok { "available" } else { "not configured" });
    if !result.roots.is_empty() {
        println!("Roots:");
        for r in &result.roots {
            println!("  {r}");
        }
    }

    if let Some(ref w) = result.watcher {
        println!();
        println!("Watcher:");
        println!("  Running:    {}", w.running);
        println!("  State:      {}", w.state);
        println!("  PID:        {}", w.pid);
        println!("  Uptime:     {}s", w.uptime_seconds);
        println!("  Watches:    {}", w.watch_count);
        println!("  Pending:    {}", w.pending_events);
        println!("  Updated:    {}", w.updated_at);
        if let Some(ref ts) = w.last_event_processed_at {
            println!("  Last event: {ts}");
        }
        if let Some(ref ts) = w.last_full_scan_at {
            print!("  Last scan:  {ts}");
            if let Some(ms) = w.last_full_scan_duration_ms {
                print!(" ({ms}ms)");
            }
            println!();
        }
    }

    Ok(())
}

fn cmd_scan_status(config: &Config) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    match smriti::scanner::scan_status(&conn)? {
        Some(status) => {
            println!("Scan #{}", status.id);
            println!("  Status:     {}", status.status);
            println!("  Started:    {}", status.started_at);
            if let Some(ref finished) = status.finished_at {
                println!("  Finished:   {finished}");
            }
            println!("  Files seen: {}", status.files_seen);
            if let Some(ref err) = status.error {
                println!("  Error:      {err}");
            }
        }
        None => {
            println!("No scan runs recorded yet.");
        }
    }
    Ok(())
}

fn cmd_triage(config: &Config) -> Result<()> {
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let global_rules = smriti::ignore::load_user_smritiignore();
    let report = smriti::triage::analyze(&conn, &global_rules)?;

    if report.recommendations.is_empty() && report.duplicates.is_empty() {
        println!("No recommendations. Index looks clean.");
        return Ok(());
    }

    let content = smriti::triage::format_triage_file(&report);

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &content)?;

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(tmp.path())
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to launch editor '{}': {}", editor, e))?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }

    let edited = std::fs::read_to_string(tmp.path())?;
    let decisions = smriti::triage::parse_triage_file(&edited)?;

    if decisions.is_empty() {
        println!("No changes to apply.");
        return Ok(());
    }

    let result = smriti::triage::apply_triage(&decisions)?;
    println!("Applied {} changes.", result.applied);
    for msg in &result.messages {
        println!("  {msg}");
    }

    Ok(())
}

fn cmd_backup_audit(config: &Config, root: &PathBuf) -> Result<()> {
    let abs_root = abs_path(root)?;
    let conn = smriti::db::open_readonly(&config.db_path)?;
    let report = smriti::backup::analyze(&conn, &abs_root)?;

    if report.total_files == 0 {
        println!("No files found under root: {}", abs_root.display());
        println!("(Is this root scanned? Run `smriti roots list` to check.)");
        return Ok(());
    }

    let content = smriti::backup::format_audit_file(&report);

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &content)?;

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(tmp.path())
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to launch editor '{}': {}", editor, e))?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }

    let edited = std::fs::read_to_string(tmp.path())?;
    let decisions = smriti::backup::parse_audit_file(&edited)?;

    if decisions.is_empty() {
        println!("No actions to apply.");
        return Ok(());
    }

    let result = smriti::backup::apply_audit(&decisions);
    for msg in &result.messages {
        println!("{msg}");
    }
    println!(
        "\nSummary: {} redundant, {} kept.",
        result.redundant_count, result.kept_count
    );

    Ok(())
}

fn format_bytes(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = 1024 * 1024;
    const GB: i64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn cmd_install_services(enable: bool) -> Result<()> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let smriti_bin = format!("{home}/.cargo/bin/smriti");

    let unit_content = format!(
        r#"[Unit]
Description=smriti-watch filesystem watcher
After=default.target

[Service]
Type=simple
ExecStart={smriti_bin} watch
Restart=always
RestartSec=2
TimeoutStopSec=30
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#
    );

    let service_dir = format!("{home}/.config/systemd/user");
    std::fs::create_dir_all(&service_dir)?;

    let service_path = format!("{service_dir}/smriti-watch.service");
    std::fs::write(&service_path, unit_content)?;
    println!("Wrote {service_path}");

    let status = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl --user daemon-reload failed");
    }
    println!("Reloaded systemd user daemon");

    if enable {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "smriti-watch.service"])
            .status()?;
        if !status.success() {
            anyhow::bail!("systemctl --user enable --now smriti-watch.service failed");
        }
        println!("Enabled and started smriti-watch.service");
    }

    Ok(())
}

fn parse_duration_string(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    if let Some(days) = s.strip_suffix('d') {
        let n: u64 = days.parse()?;
        Ok(std::time::Duration::from_secs(n * 86400))
    } else if let Some(hours) = s.strip_suffix('h') {
        let n: u64 = hours.parse()?;
        Ok(std::time::Duration::from_secs(n * 3600))
    } else {
        let secs: u64 = s.parse()?;
        Ok(std::time::Duration::from_secs(secs))
    }
}
