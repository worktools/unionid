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
        /// Store data in a durable redb database.
        #[arg(long, conflicts_with_all = ["wal_path", "snapshot_path", "snapshot_every"])]
        db: Option<PathBuf>,
        #[arg(long)]
        wal_path: Option<PathBuf>,
        #[arg(long)]
        snapshot_path: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        snapshot_every: usize,
    },
    /// Execute an atomic script in memory or in a local redb database.
    Run {
        /// Execute against a durable redb database instead of fresh memory.
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(short, long, conflicts_with = "file")]
        query: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Connect to a server, or use --memory for a local interactive session.
    #[command(
        after_long_help = "Examples:\n  unionid cli --memory\n  unionid cli --db app.redb\n  unionid cli --addr 127.0.0.1:7878 --query \"from tasks | take 10\"\n\nInteractive commands:\n  .schema  .tables  .types  .storage  .help  .quit"
    )]
    Cli {
        #[arg(long, default_value = "127.0.0.1:7878")]
        addr: String,
        #[arg(long)]
        memory: bool,
        /// Use a local durable redb database instead of connecting over TCP.
        #[arg(long, conflicts_with = "memory")]
        db: Option<PathBuf>,
        #[arg(short, long, conflicts_with = "file")]
        query: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
        /// Store safe interactive entries at this JSON Lines path.
        #[arg(long, value_name = "PATH", conflicts_with = "no_history")]
        history: Option<PathBuf>,
        /// Disable persistent interactive history.
        #[arg(long)]
        no_history: bool,
    },
    /// Format a script using the canonical semicolon-free style.
    Fmt {
        /// Read source from a file instead of stdin.
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Exit nonzero when the input differs from canonical formatting.
        #[arg(long)]
        check: bool,
    },
    /// Verify redb and unionid logical storage integrity.
    Check {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Create, inspect, and apply ordered schema migrations.
    Migration {
        #[command(subcommand)]
        command: MigrationCommand,
    },
    /// Validate and print declarative schema files.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Create a verified logical backup from a redb database.
    Backup {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Restore a verified backup to a new redb path.
    Restore {
        #[arg(long)]
        backup: PathBuf,
        #[arg(long)]
        db: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Explicitly convert the supported prototype snapshot/WAL into redb.
    ImportLegacy {
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long)]
        wal: Option<PathBuf>,
        #[arg(long)]
        db: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
}

#[derive(Debug, Subcommand)]
enum MigrationCommand {
    /// Create the next migration file.
    New {
        name: String,
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
    },
    /// Validate and preview all pending migrations without changing the database.
    Plan {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Apply pending migrations, committing each file atomically.
    Apply {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Show applied and pending migrations.
    Status {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Generate an explicit migration draft from a target schema.
    Diff {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        schema: PathBuf,
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
        #[arg(long, default_value = "schema update")]
        name: String,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
}

#[derive(Debug, Subcommand)]
enum SchemaCommand {
    /// Validate a schema file and print its normalized form.
    Check {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Print the normalized schema stored in a redb database.
    Print {
        #[arg(long)]
        db: PathBuf,
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
            db,
            wal_path,
            snapshot_path,
            snapshot_every,
        } => server::run_server_with_db(&addr, db, wal_path, snapshot_path, snapshot_every),
        Command::Run {
            db,
            query,
            file,
            format,
        } => {
            let source = match source(query, file)? {
                Some(source) => source,
                None => cli::read_source(std::io::stdin().lock())?,
            };
            match db {
                Some(path) => {
                    cli::run_local_redb(path, Some(source), matches!(format, Format::Json))
                }
                None => cli::run_local(Some(source), matches!(format, Format::Json)),
            }
        }
        Command::Cli {
            addr,
            memory,
            db,
            query,
            file,
            format,
            history,
            no_history,
        } => {
            let source = source(query, file)?;
            let history = cli::HistoryOptions {
                disabled: no_history,
                path: history,
            };
            if let Some(path) = db {
                cli::run_local_redb_with_options(
                    path,
                    source,
                    matches!(format, Format::Json),
                    history,
                )
            } else if memory {
                cli::run_local_with_options(source, matches!(format, Format::Json), history)
            } else {
                cli::run_cli_with_options(&addr, source, matches!(format, Format::Json), history)
            }
        }
        Command::Fmt { file, check } => {
            let source = match file {
                Some(path) => cli::read_source(
                    std::fs::File::open(&path)
                        .map_err(|error| format!("open '{}': {error}", path.display()))?,
                )?,
                None => cli::read_source(std::io::stdin().lock())?,
            };
            cli::format_source(&source, check)
        }
        Command::Check { db, format } => cli::check_redb(db, matches!(format, Format::Json)),
        Command::Migration { command } => match command {
            MigrationCommand::New { name, dir } => {
                let path = cli::migration_new(dir, &name)?;
                println!("{}", path.display());
                Ok(())
            }
            MigrationCommand::Plan { db, dir, format } => {
                cli::migration_plan(db, dir, matches!(format, Format::Json))
            }
            MigrationCommand::Apply { db, dir, format } => {
                cli::migration_apply(db, dir, matches!(format, Format::Json))
            }
            MigrationCommand::Status { db, dir, format } => {
                cli::migration_status(db, dir, matches!(format, Format::Json))
            }
            MigrationCommand::Diff {
                db,
                schema,
                dir,
                name,
                format,
            } => cli::migration_diff(db, schema, dir, &name, matches!(format, Format::Json)),
        },
        Command::Schema { command } => match command {
            SchemaCommand::Check { file, format } => {
                cli::schema_check(file, matches!(format, Format::Json))
            }
            SchemaCommand::Print { db, format } => {
                cli::schema_print(db, matches!(format, Format::Json))
            }
        },
        Command::Backup { db, output, format } => {
            cli::backup_create(db, output, matches!(format, Format::Json))
        }
        Command::Restore { backup, db, format } => {
            cli::backup_restore(backup, db, matches!(format, Format::Json))
        }
        Command::ImportLegacy {
            snapshot,
            wal,
            db,
            format,
        } => cli::import_legacy(snapshot, wal, db, matches!(format, Format::Json)),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
