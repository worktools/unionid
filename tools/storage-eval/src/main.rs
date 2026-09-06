use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use rusqlite::{Connection, MAIN_DB, params};
use serde::Serialize;

const KV: TableDefinition<&str, &[u8]> = TableDefinition::new("logical_records");
const DOMAINS: [&str; 5] = ["schema", "catalog", "row", "index", "ledger"];
const CRASH_BEFORE: i32 = 91;
const CRASH_AFTER: i32 = 92;

type AnyResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Backend {
    Redb,
    Sqlite,
}

impl Backend {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "redb" => Ok(Self::Redb),
            "sqlite" => Ok(Self::Sqlite),
            _ => Err(format!("unknown backend '{value}'; expected redb or sqlite").into()),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    backend: Backend,
    transactions: usize,
    records_per_transaction: usize,
    committed_records: usize,
    write_millis: u128,
    reopen_millis: u128,
    backup_millis: u128,
    database_bytes: u64,
    backup_bytes: u64,
    backend_exclusive_open: bool,
    uncommitted_crash_visible: bool,
    committed_crash_records: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("storage evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, command, backend, path] if command == "crash-before" => {
            crash(Backend::parse(backend)?, Path::new(path), false)
        }
        [_, command, backend, path] if command == "crash-after" => {
            crash(Backend::parse(backend)?, Path::new(path), true)
        }
        [_, backend, path] => evaluate(Backend::parse(backend)?, Path::new(path), 100),
        [_, backend, path, transactions] => evaluate(
            Backend::parse(backend)?,
            Path::new(path),
            transactions.parse()?,
        ),
        _ => Err("usage: unionid-storage-eval <redb|sqlite> <path> [transactions]".into()),
    }
}

fn evaluate(backend: Backend, path: &Path, transactions: usize) -> AnyResult<()> {
    if transactions == 0 {
        return Err("transactions must be greater than zero".into());
    }
    remove_database(path)?;
    let backup = suffixed(path, "backup");
    remove_database(&backup)?;

    initialize(backend, path)?;
    let backend_exclusive_open = check_exclusive_open(backend, path)?;
    let started = Instant::now();
    write_transactions(backend, path, transactions)?;
    let write_millis = started.elapsed().as_millis();

    run_crash_child("crash-before", backend, path, CRASH_BEFORE)?;
    let reopen_started = Instant::now();
    let committed_records = read_count(backend, path, "batch/")?;
    let uncommitted_crash_visible = read_count(backend, path, "crash-before/")? != 0;
    let reopen_millis = reopen_started.elapsed().as_millis();
    if committed_records != transactions * DOMAINS.len() || uncommitted_crash_visible {
        return Err("reopen exposed a partial or uncommitted batch".into());
    }

    run_crash_child("crash-after", backend, path, CRASH_AFTER)?;
    let committed_crash_records = read_count(backend, path, "crash-after/")?;
    if committed_crash_records != DOMAINS.len() {
        return Err("commit returned before the complete batch was recoverable".into());
    }

    let backup_started = Instant::now();
    backup_database(backend, path, &backup)?;
    let backup_millis = backup_started.elapsed().as_millis();
    let backup_records = read_count(backend, &backup, "")?;
    if backup_records != committed_records + committed_crash_records {
        return Err("backup is not a complete logical snapshot".into());
    }

    let report = Report {
        backend,
        transactions,
        records_per_transaction: DOMAINS.len(),
        committed_records,
        write_millis,
        reopen_millis,
        backup_millis,
        database_bytes: database_size(path)?,
        backup_bytes: database_size(&backup)?,
        backend_exclusive_open,
        uncommitted_crash_visible,
        committed_crash_records,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn initialize(backend: Backend, path: &Path) -> AnyResult<()> {
    match backend {
        Backend::Redb => {
            let database = Database::create(path)?;
            let mut transaction = database.begin_write()?;
            transaction.set_durability(Durability::Immediate)?;
            transaction.set_two_phase_commit(true);
            transaction.open_table(KV)?;
            transaction.commit()?;
        }
        Backend::Sqlite => {
            let connection = Connection::open(path)?;
            configure_sqlite(&connection)?;
            connection.execute(
                "CREATE TABLE logical_records (key TEXT PRIMARY KEY, value BLOB NOT NULL)",
                [],
            )?;
        }
    }
    Ok(())
}

fn check_exclusive_open(backend: Backend, path: &Path) -> AnyResult<bool> {
    match backend {
        Backend::Redb => {
            let first = Database::open(path)?;
            let denied = Database::open(path).is_err();
            drop(first);
            Ok(denied)
        }
        Backend::Sqlite => {
            let first = Connection::open(path)?;
            let denied = Connection::open(path).is_err();
            drop(first);
            Ok(denied)
        }
    }
}

fn write_transactions(backend: Backend, path: &Path, transactions: usize) -> AnyResult<()> {
    match backend {
        Backend::Redb => {
            let database = Database::open(path)?;
            for transaction_id in 0..transactions {
                write_redb_batch(&database, "batch", transaction_id)?;
            }
        }
        Backend::Sqlite => {
            let mut connection = Connection::open(path)?;
            configure_sqlite(&connection)?;
            for transaction_id in 0..transactions {
                write_sqlite_batch(&mut connection, "batch", transaction_id)?;
            }
        }
    }
    Ok(())
}

fn crash(backend: Backend, path: &Path, commit: bool) -> AnyResult<()> {
    match backend {
        Backend::Redb => {
            let database = Database::open(path)?;
            let mut transaction = database.begin_write()?;
            transaction.set_durability(Durability::Immediate)?;
            transaction.set_two_phase_commit(true);
            {
                let mut table = transaction.open_table(KV)?;
                insert_domains(
                    |key, value| {
                        table.insert(key, value)?;
                        Ok(())
                    },
                    if commit {
                        "crash-after"
                    } else {
                        "crash-before"
                    },
                    0,
                )?;
            }
            if commit {
                transaction.commit()?;
                std::process::exit(CRASH_AFTER);
            }
            std::process::exit(CRASH_BEFORE);
        }
        Backend::Sqlite => {
            let mut connection = Connection::open(path)?;
            configure_sqlite(&connection)?;
            let transaction = connection.transaction()?;
            insert_domains(
                |key, value| {
                    transaction.execute(
                        "INSERT INTO logical_records (key, value) VALUES (?1, ?2)",
                        params![key, value],
                    )?;
                    Ok(())
                },
                if commit {
                    "crash-after"
                } else {
                    "crash-before"
                },
                0,
            )?;
            if commit {
                transaction.commit()?;
                std::process::exit(CRASH_AFTER);
            }
            std::process::exit(CRASH_BEFORE);
        }
    }
}

fn write_redb_batch(database: &Database, prefix: &str, id: usize) -> AnyResult<()> {
    let mut transaction = database.begin_write()?;
    transaction.set_durability(Durability::Immediate)?;
    transaction.set_two_phase_commit(true);
    {
        let mut table = transaction.open_table(KV)?;
        insert_domains(
            |key, value| {
                table.insert(key, value)?;
                Ok(())
            },
            prefix,
            id,
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn write_sqlite_batch(connection: &mut Connection, prefix: &str, id: usize) -> AnyResult<()> {
    let transaction = connection.transaction()?;
    insert_domains(
        |key, value| {
            transaction.execute(
                "INSERT INTO logical_records (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        },
        prefix,
        id,
    )?;
    transaction.commit()?;
    Ok(())
}

fn insert_domains(
    mut insert: impl FnMut(&str, &[u8]) -> AnyResult<()>,
    prefix: &str,
    id: usize,
) -> AnyResult<()> {
    for domain in DOMAINS {
        let key = format!("{prefix}/{id:08}/{domain}");
        let value = format!("logical-{domain}-value-{id:08}");
        insert(&key, value.as_bytes())?;
    }
    Ok(())
}

fn read_count(backend: Backend, path: &Path, prefix: &str) -> AnyResult<usize> {
    match backend {
        Backend::Redb => {
            let database = Database::open(path)?;
            let transaction = database.begin_read()?;
            let table = transaction.open_table(KV)?;
            let mut count = 0;
            for entry in table.iter()? {
                let (key, _) = entry?;
                if key.value().starts_with(prefix) {
                    count += 1;
                }
            }
            Ok(count)
        }
        Backend::Sqlite => {
            let connection = Connection::open(path)?;
            configure_sqlite(&connection)?;
            let pattern = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
            let count = connection.query_row(
                "SELECT count(*) FROM logical_records WHERE key LIKE ?1 ESCAPE '\\'",
                [pattern],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(usize::try_from(count)?)
        }
    }
}

fn backup_database(backend: Backend, source: &Path, destination: &Path) -> AnyResult<()> {
    match backend {
        Backend::Redb => {
            let source_database = Database::open(source)?;
            let source_transaction = source_database.begin_read()?;
            let source_table = source_transaction.open_table(KV)?;
            let destination_database = Database::create(destination)?;
            let mut destination_transaction = destination_database.begin_write()?;
            destination_transaction.set_durability(Durability::Immediate)?;
            destination_transaction.set_two_phase_commit(true);
            {
                let mut destination_table = destination_transaction.open_table(KV)?;
                for entry in source_table.iter()? {
                    let (key, value) = entry?;
                    destination_table.insert(key.value(), value.value())?;
                }
            }
            destination_transaction.commit()?;
        }
        Backend::Sqlite => {
            let connection = Connection::open(source)?;
            configure_sqlite(&connection)?;
            connection.backup(MAIN_DB, destination, None)?;
        }
    }
    Ok(())
}

fn configure_sqlite(connection: &Connection) -> AnyResult<()> {
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON;")?;
    Ok(())
}

fn run_crash_child(
    command: &str,
    backend: Backend,
    path: &Path,
    expected_code: i32,
) -> AnyResult<()> {
    let backend = match backend {
        Backend::Redb => "redb",
        Backend::Sqlite => "sqlite",
    };
    let status = Command::new(std::env::current_exe()?)
        .arg(command)
        .arg(backend)
        .arg(path)
        .status()?;
    if status.code() != Some(expected_code) {
        return Err(format!("{command} child exited with {status}").into());
    }
    Ok(())
}

fn remove_database(path: &Path) -> AnyResult<()> {
    for candidate in [
        path.to_path_buf(),
        suffixed(path, "wal"),
        suffixed(path, "shm"),
    ] {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn database_size(path: &Path) -> AnyResult<u64> {
    let mut total = 0;
    for candidate in [
        path.to_path_buf(),
        suffixed(path, "wal"),
        suffixed(path, "shm"),
    ] {
        match fs::metadata(candidate) {
            Ok(metadata) => total += metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(total)
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.display()))
}
