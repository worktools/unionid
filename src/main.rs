mod cli;
mod db;
mod model;
mod query;
mod server;
mod snapshot;
mod wal;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "unionid",
    version,
    about = "Enum + Struct based in-memory DB with PRQL-style pipeline queries"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
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
    Cli {
        #[arg(long, default_value = "127.0.0.1:7878")]
        addr: String,
        #[arg(short, long)]
        query: Option<String>,
    },
}

fn main() {
    let args = Args::parse();

    let result = match args.command {
        Command::Server {
            addr,
            wal_path,
            snapshot_path,
            snapshot_every,
        } => server::run_server(&addr, wal_path, snapshot_path, snapshot_every),
        Command::Cli { addr, query } => cli::run_cli(&addr, query),
    };

    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
