use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use smriti::config::Config;
use smriti::ignore::SectionRules;
use smriti::roots;
use smriti::search;

#[derive(Parser)]
#[command(name = "smriti", about = "Content-addressed filesystem indexer")]
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
    /// Show backup audit report
    Audit {
        #[arg(long)]
        min_bytes: Option<u64>,
        #[arg(long)]
        sort_by: Option<String>,
    },
    /// Export tier-1 file paths for backup tooling
    Manifest {
        #[arg(long, default_value = "paths")]
        format: String,
    },
    /// Search indexed files by content
    Find {
        query: String,
        #[arg(short, default_value = "10")]
        k: u32,
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
    /// Run the background daemon (Wave 5)
    Daemon,
}

#[derive(Subcommand)]
enum RootsAction {
    Add { path: PathBuf },
    Remove { path: PathBuf },
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
        Commands::Audit { min_bytes, sort_by } => cmd_audit(&config, min_bytes, sort_by)?,
        Commands::Manifest { format } => cmd_manifest(&config, &format)?,
        Commands::Find { query, k } => cmd_find(&config, &query, k)?,
        Commands::Get { content_hash } => cmd_get(&config, &content_hash)?,
        Commands::History { path, since, until } => cmd_history(&config, &path, since, until)?,
        Commands::Roots { action } => cmd_roots(action)?,
        Commands::Prune { older_than, .. } => cmd_prune(&config, older_than)?,
        Commands::Health => cmd_health(&config)?,
        Commands::ScanStatus => cmd_scan_status(&config)?,
        Commands::Daemon => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(smriti::daemon::run_stdio(config))?;
        }
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
    if let Some(j) = jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .ok();
    }

    let mut scan_config = config.clone();
    if let Some(paths) = filter_paths {
        scan_config.roots = paths;
    }

    let effective_roots = roots::load_roots(&scan_config)?;
    if effective_roots.is_empty() {
        anyhow::bail!("No roots configured. Run `smriti roots add <path>` or set SMRITI_ROOTS.");
    }
    scan_config.roots = effective_roots;

    let mut conn = smriti::db::open(&config.db_path)?;
    smriti::db::checkpoint_wal(&conn)?;
    let global_rules = SectionRules::empty();
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

fn cmd_audit(config: &Config, min_bytes: Option<u64>, sort_by: Option<String>) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;
    let mut audit_config = config.clone();
    audit_config.roots = roots::load_roots(config)?;

    let result = search::audit(&conn, min_bytes, sort_by.as_deref(), &audit_config)?;

    println!("=== Backup Audit ===\n");
    println!("Roots: {}", if result.roots.is_empty() { "(none)".to_string() } else { result.roots.join(", ") });
    println!();

    println!("Tier 1 (indexed — back this up):");
    println!("  Files: {}", result.tier1_total_files);
    println!("  Size:  {}", format_bytes(result.tier1_total_bytes));
    if !result.tier1_by_extension.is_empty() {
        println!("  By extension:");
        let mut exts: Vec<_> = result.tier1_by_extension.iter().collect();
        exts.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes));
        for (ext, stats) in exts.iter().take(15) {
            println!("    {:<12} {:>6} files  {}", ext, stats.files, format_bytes(stats.bytes));
        }
        if exts.len() > 15 {
            println!("    ... and {} more extensions", exts.len() - 15);
        }
    }
    println!();

    println!("Tier 2 (cataloged — regenerable, don't back up):");
    println!("  Dirs:  {}", result.tier2_total_dirs);
    println!("  Size:  {}", format_bytes(result.tier2_total_bytes));
    if !result.tier2_largest.is_empty() {
        println!("  Largest:");
        for entry in &result.tier2_largest {
            println!(
                "    {}  {} ({} files)",
                format_bytes(entry.total_bytes),
                entry.path,
                entry.file_count,
            );
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
    let conn = smriti::db::open(&config.db_path)?;
    let result = search::manifest(&conn, format, config)?;

    for entry in &result.entries {
        println!("{entry}");
    }

    Ok(())
}

fn cmd_find(config: &Config, query: &str, k: u32) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;
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

fn cmd_get(config: &Config, content_hash: &str) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;
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
    let conn = smriti::db::open(&config.db_path)?;
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
            let abs = if path.is_relative() {
                std::env::current_dir()?.join(&path)
            } else {
                path.clone()
            };
            roots::add_root(&abs)?;
            println!("Added root: {}", abs.display());
        }
        RootsAction::Remove { path } => {
            let abs = if path.is_relative() {
                std::env::current_dir()?.join(&path)
            } else {
                path.clone()
            };
            roots::remove_root(&abs)?;
            println!("Removed root: {}", abs.display());
        }
        RootsAction::List => {
            let list = roots::list_roots()?;
            if list.is_empty() {
                println!("No roots configured.");
            } else {
                for r in &list {
                    println!("{}", r.display());
                }
            }
        }
    }
    Ok(())
}

fn cmd_prune(config: &Config, older_than: Option<String>) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;

    let threshold = older_than
        .map(|s| parse_duration_string(&s))
        .transpose()?
        .unwrap_or(std::time::Duration::from_secs(30 * 86400));

    let events_pruned = smriti::db::prune_events(&conn, threshold)?;
    let audit_pruned = smriti::db::prune_audit_log(&conn, config.audit_retention_days)?;

    println!("Pruned {events_pruned} old events, {audit_pruned} audit log entries.");
    Ok(())
}

fn cmd_health(config: &Config) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;
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

    Ok(())
}

fn cmd_scan_status(config: &Config) -> Result<()> {
    let conn = smriti::db::open(&config.db_path)?;
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
