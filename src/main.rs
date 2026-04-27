use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "smriti", about = "Content-addressed filesystem indexer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init,
    Scan {
        #[arg(long)]
        paths: Option<Vec<PathBuf>>,
    },
    Audit {
        #[arg(long)]
        min_bytes: Option<u64>,
        #[arg(long)]
        sort_by: Option<String>,
    },
    Manifest {
        #[arg(long, default_value = "paths")]
        format: String,
    },
    Find {
        query: String,
        #[arg(short, default_value = "10")]
        k: u32,
    },
    Roots {
        #[command(subcommand)]
        action: RootsAction,
    },
    Prune {
        #[arg(long)]
        older_than: Option<String>,
        #[arg(long)]
        keep_versions: Option<u32>,
    },
    Health,
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

    match cli.command {
        Commands::Init => println!("not implemented"),
        Commands::Scan { .. } => println!("not implemented"),
        Commands::Audit { .. } => println!("not implemented"),
        Commands::Manifest { .. } => println!("not implemented"),
        Commands::Find { .. } => println!("not implemented"),
        Commands::Roots { .. } => println!("not implemented"),
        Commands::Prune { .. } => println!("not implemented"),
        Commands::Health => println!("not implemented"),
        Commands::Daemon => println!("not implemented"),
    }

    Ok(())
}
