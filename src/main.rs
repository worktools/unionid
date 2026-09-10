use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompactFormat {
    Plain,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print software, protocol, storage, codec, and target versions.
    Version {
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Diagnose this binary and optionally inspect an existing database without repairing it.
    Doctor {
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
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
        /// Reject every mutating request; requires --db.
        #[arg(long, requires = "db")]
        read_only: bool,
    },
    /// Execute an atomic script in memory or in a local redb database.
    Run {
        /// Execute against a durable redb database instead of fresh memory.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Reject every mutating script; requires --db.
        #[arg(long, requires = "db")]
        read_only: bool,
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
        /// Reject every mutating script; requires --db.
        #[arg(long, requires = "db")]
        read_only: bool,
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
    /// Offline compact an existing redb file after complete pre/post checks.
    Compact {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, value_enum, default_value = "plain")]
        format: CompactFormat,
    },
    /// Explicitly upgrade all durable codecs in one transaction.
    Upgrade {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        target: u32,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Create, inspect, and apply ordered schema migrations.
    Migration {
        #[command(subcommand)]
        command: MigrationCommand,
    },
    /// Inspect or explicitly prune durable idempotency receipts.
    Receipts {
        #[command(subcommand)]
        command: ReceiptCommand,
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
enum ReceiptCommand {
    /// Show receipt capacity and oldest/newest retained boundaries.
    Status {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, value_enum, default_value = "table")]
        format: Format,
    },
    /// Preview eligible receipts; pass --confirm to delete that exact bounded selection.
    Prune {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        before_unix_ms: Option<u64>,
        #[arg(long)]
        through_sequence: Option<u64>,
        #[arg(long, default_value_t = 1000)]
        max_receipts: usize,
        #[arg(long)]
        confirm: bool,
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
    /// Commit a deterministic bounded number of format-6 migration steps.
    Advance {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value = "migrations")]
        dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        max_steps: usize,
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
    /// Abort and clean an unfinished shadow-generation migration.
    Abort {
        #[arg(long)]
        db: PathBuf,
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

#[derive(Serialize)]
struct VersionReport {
    schema_version: u32,
    software_version: &'static str,
    target: &'static str,
    protocol_versions: [u32; 2],
    stream_protocol_versions: [u32; 1],
    readable_storage_formats: [u32; 6],
    readable_backup_formats: [u32; 4],
    current_storage: unionid::StorageVersions,
}

#[derive(Serialize)]
struct DoctorReport {
    schema_version: u32,
    ok: bool,
    version: VersionReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<DatabaseDiagnostics>,
}

#[derive(Serialize)]
struct DatabaseDiagnostics {
    storage: unionid::StorageMode,
    storage_versions: Option<unionid::StorageVersions>,
    read_only: bool,
    schema: unionid::SchemaInfo,
    migration_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    migration_head: Option<String>,
    table_count: usize,
    type_count: usize,
}

struct DoctorCopy(PathBuf);

impl DoctorCopy {
    fn create(source: &std::path::Path) -> Result<Self, String> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| format!("E_IO: generate doctor copy name: {error}"))?;
        let nonce = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!(
            "unionid-doctor-{}-{nonce}.redb",
            std::process::id()
        ));
        let mut input = std::fs::File::open(source)
            .map_err(|error| format!("E_IO: open database for read-only diagnosis: {error}"))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&path)
            .map_err(|error| format!("E_IO: create private doctor copy: {error}"))?;
        if let Err(error) = std::io::copy(&mut input, &mut output) {
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "E_IO: copy database for read-only diagnosis: {error}"
            ));
        }
        drop(output);
        Ok(Self(path))
    }
}

impl Drop for DoctorCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    schema_version: u32,
    ok: bool,
    exit_code: i32,
    error: unionid::Error,
}

#[derive(Clone, Copy)]
struct ErrorOutput {
    json: bool,
    query_response: bool,
    integrity: bool,
}

impl Args {
    fn error_output(&self) -> ErrorOutput {
        match &self.command {
            Command::Run { format, .. } | Command::Cli { format, .. } => ErrorOutput {
                json: matches!(format, Format::Json),
                query_response: true,
                integrity: false,
            },
            Command::Check { format, .. } => ErrorOutput {
                json: matches!(format, Format::Json),
                query_response: false,
                integrity: true,
            },
            Command::Compact { format, .. } => ErrorOutput {
                json: matches!(format, CompactFormat::Json),
                query_response: false,
                integrity: false,
            },
            Command::Version { format }
            | Command::Doctor { format, .. }
            | Command::Upgrade { format, .. }
            | Command::Backup { format, .. }
            | Command::Restore { format, .. }
            | Command::ImportLegacy { format, .. }
            | Command::Receipts {
                command:
                    ReceiptCommand::Status { format, .. } | ReceiptCommand::Prune { format, .. },
            }
            | Command::Schema {
                command: SchemaCommand::Check { format, .. } | SchemaCommand::Print { format, .. },
            }
            | Command::Migration {
                command:
                    MigrationCommand::Plan { format, .. }
                    | MigrationCommand::Apply { format, .. }
                    | MigrationCommand::Advance { format, .. }
                    | MigrationCommand::Status { format, .. }
                    | MigrationCommand::Abort { format, .. }
                    | MigrationCommand::Diff { format, .. },
            } => ErrorOutput {
                json: matches!(format, Format::Json),
                query_response: false,
                integrity: false,
            },
            _ => ErrorOutput {
                json: false,
                query_response: false,
                integrity: false,
            },
        }
    }
}

fn version_report() -> VersionReport {
    VersionReport {
        schema_version: 1,
        software_version: env!("CARGO_PKG_VERSION"),
        target: env!("UNIONID_BUILD_TARGET"),
        protocol_versions: [
            unionid::protocol::VERSION,
            unionid::protocol::PRODUCTION_VERSION,
        ],
        stream_protocol_versions: [unionid::stream::VERSION],
        readable_storage_formats: [1, 2, 3, 4, 5, 6],
        readable_backup_formats: [1, 2, 3, 4],
        current_storage: unionid::Engine::current_storage_versions(),
    }
}

fn print_version(json: bool) -> Result<(), String> {
    let report = version_report();
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "unionid {}\ntarget {}\nprotocols 1,2; streams 1\nstorage read 1,2,3,4,5,6; write {}\ncodecs catalog/value/index/migration/receipt/maintenance/backup {}/{}/{}/{}/{}/{}/{}",
            report.software_version,
            report.target,
            report.current_storage.format,
            report.current_storage.catalog_codec,
            report.current_storage.value_codec,
            report.current_storage.index_key_codec,
            report.current_storage.migration_codec,
            report.current_storage.receipt_codec,
            report.current_storage.maintenance_codec,
            report.current_storage.backup_codec,
        );
    }
    Ok(())
}

fn doctor(db: Option<PathBuf>, json: bool) -> Result<(), String> {
    let database = match db {
        Some(path) => {
            if !path.is_file() {
                return Err("E_CONFIG: doctor database must already exist and be a file".into());
            }
            // redb may perform recovery bookkeeping while opening. Diagnose a
            // private byte-for-byte copy so the requested path is never repaired,
            // upgraded, created, or otherwise changed by doctor.
            let copy = DoctorCopy::create(&path)?;
            let introspection = unionid::Engine::open_redb_read_only(&copy.0)
                .map_err(|error| error.to_string())?
                .introspection();
            Some(DatabaseDiagnostics {
                storage: introspection.storage,
                storage_versions: introspection.storage_versions,
                read_only: introspection.read_only,
                schema: introspection.schema,
                migration_count: introspection.migration_count,
                migration_head: introspection.migration_head,
                table_count: introspection.tables.len(),
                type_count: introspection.types.len(),
            })
        }
        None => None,
    };
    let report = DoctorReport {
        schema_version: 1,
        ok: true,
        version: version_report(),
        database,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|e| e.to_string())?
        );
    } else {
        print_version(false)?;
        if let Some(database) = report.database {
            println!(
                "database {:?}\nread only yes\nschema revision {}\nschema hash {}\nmigrations {}",
                database.storage,
                database.schema.revision,
                database.schema.hash,
                database.migration_count
            );
        } else {
            println!("database not requested");
        }
        println!("doctor ok");
    }
    Ok(())
}

fn run(args: Args) -> Result<(), String> {
    match args.command {
        Command::Version { format } => print_version(matches!(format, Format::Json)),
        Command::Doctor { db, format } => doctor(db, matches!(format, Format::Json)),
        Command::Server {
            addr,
            db,
            wal_path,
            snapshot_path,
            snapshot_every,
            read_only,
        } => server::run_server_with_db_read_only(
            &addr,
            db,
            wal_path,
            snapshot_path,
            snapshot_every,
            read_only,
        ),
        Command::Run {
            db,
            query,
            file,
            format,
            read_only,
        } => {
            let source = match source(query, file)? {
                Some(source) => source,
                None => cli::read_source(std::io::stdin().lock())?,
            };
            match db {
                Some(path) => cli::run_local_redb_read_only_with_options(
                    path,
                    Some(source),
                    matches!(format, Format::Json),
                    cli::HistoryOptions::default(),
                    read_only,
                ),
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
            read_only,
        } => {
            let source = source(query, file)?;
            let history = cli::HistoryOptions {
                disabled: no_history,
                path: history,
            };
            if let Some(path) = db {
                cli::run_local_redb_read_only_with_options(
                    path,
                    source,
                    matches!(format, Format::Json),
                    history,
                    read_only,
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
        Command::Compact { db, format } => {
            cli::compact_redb(db, matches!(format, CompactFormat::Json))
        }
        Command::Upgrade { db, target, format } => {
            cli::upgrade_redb(db, target, matches!(format, Format::Json))
        }
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
            MigrationCommand::Advance {
                db,
                dir,
                max_steps,
                format,
            } => cli::migration_advance(db, dir, max_steps, matches!(format, Format::Json)),
            MigrationCommand::Status { db, dir, format } => {
                cli::migration_status(db, dir, matches!(format, Format::Json))
            }
            MigrationCommand::Abort { db, format } => {
                cli::migration_abort(db, matches!(format, Format::Json))
            }
            MigrationCommand::Diff {
                db,
                schema,
                dir,
                name,
                format,
            } => cli::migration_diff(db, schema, dir, &name, matches!(format, Format::Json)),
        },
        Command::Receipts { command } => match command {
            ReceiptCommand::Status { db, format } => {
                cli::receipt_status(db, matches!(format, Format::Json))
            }
            ReceiptCommand::Prune {
                db,
                before_unix_ms,
                through_sequence,
                max_receipts,
                confirm,
                format,
            } => cli::receipt_prune(
                db,
                unionid::IdempotencyPruneOptions {
                    completed_before_unix_ms: before_unix_ms,
                    committed_through_sequence: through_sequence,
                    max_receipts,
                },
                confirm,
                matches!(format, Format::Json),
            ),
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
    let raw = std::env::args_os().collect::<Vec<_>>();
    let json_requested = raw.iter().any(|argument| argument == "--format=json")
        || raw
            .windows(2)
            .any(|pair| pair[0] == "--format" && pair[1] == "json");
    let query_command = raw
        .get(1)
        .and_then(|value| value.to_str())
        .is_some_and(|value| matches!(value, "run" | "cli"));
    let args = match Args::try_parse_from(raw) {
        Ok(args) => args,
        Err(error) => {
            let exit_code = error.exit_code();
            if exit_code == 0 {
                let _ = error.print();
                std::process::exit(0);
            }
            if json_requested && !query_command {
                exit_error(
                    format!("E_ARGUMENT: {error}"),
                    ErrorOutput {
                        json: true,
                        query_response: false,
                        integrity: false,
                    },
                    Some(2),
                );
            }
            let _ = error.print();
            std::process::exit(exit_code);
        }
    };
    let output = args.error_output();
    if let Err(error) = run(args) {
        exit_error(error, output, None);
    }
}

fn exit_error(message: String, output: ErrorOutput, forced: Option<i32>) -> ! {
    let error = structured_error(&message);
    let exit_code = forced.unwrap_or_else(|| classify_exit(&error.code, output.integrity));
    if output.json && !output.query_response {
        println!(
            "{}",
            serde_json::to_string(&ErrorEnvelope {
                schema_version: 1,
                ok: false,
                exit_code,
                error,
            })
            .expect("error envelope serialization cannot fail")
        );
    } else {
        eprintln!("{message}");
    }
    std::process::exit(exit_code);
}

fn structured_error(message: &str) -> unionid::Error {
    let code = message
        .split_once(':')
        .map(|(prefix, _)| prefix)
        .filter(|prefix| {
            prefix.starts_with("E_") && prefix.chars().all(|c| c == '_' || c.is_ascii_uppercase())
        })
        .unwrap_or_else(|| {
            if message.starts_with("connect ") || message.contains("connection") {
                "E_CONNECTION"
            } else if message.contains("canonically formatted") {
                "E_INPUT"
            } else {
                "E_IO"
            }
        });
    let detail = if code == "E_ARGUMENT" {
        "invalid command-line arguments; run --help"
    } else {
        message
            .strip_prefix(code)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
            .unwrap_or(message)
    };
    unionid::Error::new(code, redact_quoted(detail))
}

fn redact_quoted(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut quoted = false;
    for character in message.chars() {
        if character == '\'' {
            if quoted {
                output.push_str("<redacted>'");
            } else {
                output.push('\'');
            }
            quoted = !quoted;
        } else if !quoted {
            output.push(character);
        }
    }
    if quoted {
        output.push_str("<redacted>");
    }
    output
}

fn classify_exit(code: &str, integrity: bool) -> i32 {
    if integrity {
        return 6;
    }
    match code {
        "E_ARGUMENT" | "E_CONFIG" => 2,
        "E_ARITH"
        | "E_BYTES"
        | "E_CELLS"
        | "E_CONSTRAINT"
        | "E_CURSOR_CODEC"
        | "E_CURSOR_DATABASE"
        | "E_CURSOR_INTEGRITY"
        | "E_CURSOR_LIMIT"
        | "E_CURSOR_QUERY"
        | "E_CURSOR_SCHEMA"
        | "E_CURSOR_STALE"
        | "E_DECIMAL_RANGE"
        | "E_DECIMAL_TYPE"
        | "E_EVALUATIONS"
        | "E_FIELD"
        | "E_IDEMPOTENCY_CAPACITY"
        | "E_IDEMPOTENCY_CONFLICT"
        | "E_IDEMPOTENCY_DIGEST"
        | "E_IDEMPOTENCY_KEY"
        | "E_IDEMPOTENCY_LIMIT"
        | "E_IDEMPOTENCY_NOT_MUTATION"
        | "E_IDEMPOTENCY_PRUNE"
        | "E_INCOMPLETE"
        | "E_INDEX"
        | "E_INDEX_KEY_LIMIT"
        | "E_INPUT"
        | "E_KEY"
        | "E_LIMIT"
        | "E_MAINTENANCE_REQUIRED"
        | "E_MATCH"
        | "E_MIGRATION"
        | "E_OUTPUTS"
        | "E_PAGE_ORDER"
        | "E_PAGE_SHAPE"
        | "E_PARAM_EXTRA"
        | "E_PARAM_MISSING"
        | "E_PARAM_TYPE"
        | "E_PREPARE"
        | "E_PROTOCOL_TYPE"
        | "E_QUERY"
        | "E_READ_ONLY"
        | "E_RECEIPTS"
        | "E_SCALAR_LITERAL"
        | "E_SCHEMA"
        | "E_SCHEMA_CHANGED"
        | "E_SCHEMA_DIFF"
        | "E_SERDE"
        | "E_STEPS"
        | "E_SYNTAX"
        | "E_TABLE"
        | "E_TYPE" => 3,
        "E_BUSY" | "E_CONNECTION" | "E_PROTOCOL" | "E_PROTOCOL_VERSION" | "E_READ_SNAPSHOT"
        | "E_SHUTDOWN" | "E_TIMEOUT" => 4,
        "E_BACKUP"
        | "E_CHECKPOINT"
        | "E_CODEC"
        | "E_CODEC_KEY"
        | "E_CODEC_VERSION"
        | "E_FORMAT_VERSION"
        | "E_IO"
        | "E_STORAGE"
        | "E_STORAGE_REOPEN_REQUIRED"
        | "E_STORAGE_UPGRADE"
        | "E_STORAGE_UPGRADE_REQUIRED"
        | "E_TIME" => 5,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::classify_exit;

    #[test]
    fn compaction_state_errors_keep_stable_exit_classes() {
        assert_eq!(classify_exit("E_MAINTENANCE_REQUIRED", false), 3);
        assert_eq!(classify_exit("E_BUSY", false), 4);
        assert_eq!(classify_exit("E_STORAGE_REOPEN_REQUIRED", false), 5);
    }
}
