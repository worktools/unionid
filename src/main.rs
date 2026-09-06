use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use unionid::{cli, server};

#[derive(Debug, Parser)]
#[command(
    name = "unionid",
    version,
    about = "A lightweight database with algebraic types and pipeline queries"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Table,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    Server {
        #[arg(long, default_value = "127.0.0.1:7878")]
        addr: String,
        #[arg(long)]
        wal_path: Option<PathBuf>,
        #[arg(long)]
        snapshot_path: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        snapshot_every: usize,
    },
    /// Execute an atomic script in a fresh in-memory database (stdin by default).
    Run {
        #[arg(short, long, conflicts_with = "file")]
        query: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Connect to a server, or use --memory for a local interactive session.
    Cli {
        #[arg(long, default_value = "127.0.0.1:7878")]
        addr: String,
        #[arg(long)]
        memory: bool,
        #[arg(short, long, conflicts_with = "file")]
        query: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
}

fn source(query: Option<String>, file: Option<PathBuf>) -> Result<Option<String>, String> {
    match file {
        Some(path) => cli::read_source(
            std::fs::File::open(&path).map_err(|e| format!("open '{}': {e}", path.display()))?,
        )
        .map(Some),
        None => Ok(query),
    }
}

fn run() -> Result<(), String> {
    match Args::parse().command {
        Command::Server {
            addr,
            wal_path,
            snapshot_path,
            snapshot_every,
        } => server::run_server(&addr, wal_path, snapshot_path, snapshot_every),
        Command::Run {
            query,
            file,
            format,
        } => {
            let source = match source(query, file)? {
                Some(source) => source,
                None => cli::read_source(std::io::stdin().lock())?,
            };
            cli::run_local(Some(source), matches!(format, Format::Json))
        }
        Command::Cli {
            addr,
            memory,
            query,
            file,
            format,
        } => {
            let source = source(query, file)?;
            if memory {
                cli::run_local(source, matches!(format, Format::Json))
            } else {
                cli::run_cli(&addr, source, matches!(format, Format::Json))
            }
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
