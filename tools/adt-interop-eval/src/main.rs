use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Connection, Row, SqliteConnection};
use unionid::Engine;

type AnyResult<T> = Result<T, Box<dyn Error>>;

const UNIONID_SCHEMA: &str =
    include_str!("../../../tests/fixtures/adt-interop/unionid/schema_v1.uid");
const UNIONID_MIGRATION: &str =
    include_str!("../../../tests/fixtures/adt-interop/unionid/migrate_v2.uid");
const SQLITE_SCHEMA: &str =
    include_str!("../../../tests/fixtures/adt-interop/sqlite/schema_v1.sql");
const SQLITE_MIGRATION: &str =
    include_str!("../../../tests/fixtures/adt-interop/sqlite/migrate_v2.sql");

#[derive(Serialize)]
struct Report {
    backend: &'static str,
    rows: usize,
    durability: &'static str,
    startup_micros: u128,
    write_micros: u128,
    read_micros: u128,
    migration_micros: u128,
    matched_rows: usize,
    total_price_cents: i64,
    migrated_rows: usize,
    database_bytes: u64,
    peak_rss_bytes: u64,
    os: &'static str,
    arch: &'static str,
}

struct Measurements {
    startup_micros: u128,
    write_micros: u128,
    read_micros: u128,
    migration_micros: u128,
    matched_rows: usize,
    total_price_cents: i64,
}

#[allow(dead_code)]
#[derive(Debug, PartialEq)]
enum ApplicationState {
    Queued,
    Running {
        attempt: i64,
    },
    Failed {
        message: String,
        retry_at_micros: Option<i64>,
    },
    Done,
    Archived {
        at_micros: i64,
    },
}

#[allow(dead_code)]
#[derive(Debug)]
struct ApplicationTask {
    id: i64,
    title: String,
    state: ApplicationState,
    note: Option<Option<String>>,
    price_cents: i64,
    created_at_micros: i64,
    priority: Option<i64>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let [_, backend, path, rows] = args.as_slice() else {
        return Err("usage: unionid-adt-interop-eval <unionid|sqlite-sqlx> <db> <rows>".into());
    };
    let rows = rows.parse::<usize>()?;
    if rows == 0 {
        return Err("rows must be greater than zero".into());
    }
    remove_database(Path::new(path))?;
    let report = match backend.as_str() {
        "unionid" => evaluate_unionid(Path::new(path), rows)?,
        "sqlite-sqlx" => evaluate_sqlite(Path::new(path), rows).await?,
        _ => return Err("backend must be unionid or sqlite-sqlx".into()),
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn evaluate_unionid(path: &Path, rows: usize) -> AnyResult<Report> {
    let started = Instant::now();
    let mut engine = Engine::open_redb(path)?;
    require_ok(engine.execute(UNIONID_SCHEMA), "schema")?;
    drop(engine);
    let mut engine = Engine::open_redb(path)?;
    let startup_micros = started.elapsed().as_micros();

    let values = (0..rows)
        .map(|id| {
            let state = if id % 2 == 0 { "State.Queued".to_owned() } else { format!("State.Running {{attempt = {}}}", id % 4 + 1) };
            format!("{{id = {id}, title = \"task-{id}\", state = {state}, note = Some None, price = decimal \"10.25\", created_at = @2026-09-13T08:00:00Z}}")
        })
        .collect::<Vec<_>>()
        .join(",");
    let write_started = Instant::now();
    require_ok(
        engine.execute(&format!("insert many tasks [{values}]")),
        "insert",
    )?;
    let write_micros = write_started.elapsed().as_micros();

    let read_started = Instant::now();
    let response = engine.execute("from tasks\nfilter match state\n  State.Running {..} => true\n  _ => false\naggregate\n  matched = count\n  total = sum price");
    require_ok(response.clone(), "read")?;
    let read_micros = read_started.elapsed().as_micros();
    let matched_rows = rows / 2;
    let total_price_cents = i64::try_from(matched_rows)? * 1025;
    if response.rows.len() != 1 {
        return Err("unionid aggregate did not return one row".into());
    }
    let row = &response.rows[0];
    if row["matched"].source_text() != matched_rows.to_string()
        || row["total"].source_text() != decimal_source(total_price_cents)
    {
        return Err("unionid aggregate did not match the reference model".into());
    }

    let migration_started = Instant::now();
    require_ok(engine.execute(UNIONID_MIGRATION), "migration")?;
    let migrated = engine.execute("from tasks\nfilter priority == 0\naggregate\n  rows = count");
    require_ok(migrated.clone(), "migrated read")?;
    if migrated.rows.len() != 1 || migrated.rows[0]["rows"].source_text() != rows.to_string() {
        return Err("unionid migration did not default every existing row".into());
    }
    require_ok(engine.execute(&format!("insert tasks {{id = {rows}, title = \"archived\", state = State.Archived {{at = @2026-09-13T08:00:00Z}}, note = Some None, price = decimal \"10.25\", created_at = @2026-09-13T08:00:00Z, priority = 3}}")), "post-migration insert")?;
    let archived = engine.execute(&format!(
        "from tasks\nfilter id == {rows}\nselect {{id, state, priority}}"
    ));
    require_ok(archived.clone(), "post-migration projection")?;
    if archived.rows.len() != 1 || archived.rows[0]["priority"].source_text() != "3" {
        return Err("unionid projection did not expose the evolved field".into());
    }
    let migration_micros = migration_started.elapsed().as_micros();
    drop(engine);
    report(
        "unionid",
        rows,
        Measurements {
            startup_micros,
            write_micros,
            read_micros,
            migration_micros,
            matched_rows,
            total_price_cents,
        },
        path,
    )
}

async fn evaluate_sqlite(path: &Path, rows: usize) -> AnyResult<Report> {
    let options = sqlite_options(path);
    let started = Instant::now();
    let mut connection = SqliteConnection::connect_with(&options).await?;
    sqlx::raw_sql(SQLITE_SCHEMA)
        .execute(&mut connection)
        .await?;
    connection.close().await?;
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let startup_micros = started.elapsed().as_micros();

    let prepared_rows = (0..rows)
        .map(|id| {
            let (tag, attempt) = if id % 2 == 0 {
                ("Queued", None)
            } else {
                ("Running", Some(i64::try_from(id % 4 + 1)?))
            };
            Ok((i64::try_from(id)?, format!("task-{id}"), tag, attempt))
        })
        .collect::<AnyResult<Vec<_>>>()?;
    let write_started = Instant::now();
    let mut transaction = connection.begin().await?;
    for (id, title, tag, attempt) in prepared_rows {
        sqlx::query("INSERT INTO tasks (id,title,state_tag,state_attempt,state_message,state_retry_at_micros,note_outer_some,note_value,price_cents,created_at_micros) VALUES (?,?,?,?,?,?,?,?,?,?)")
            .bind(id).bind(title).bind(tag).bind(attempt)
            .bind(Option::<String>::None).bind(Option::<i64>::None).bind(1_i64)
            .bind(Option::<String>::None).bind(1025_i64).bind(1_778_400_000_000_000_i64)
            .execute(&mut *transaction).await?;
    }
    transaction.commit().await?;
    let write_micros = write_started.elapsed().as_micros();

    let read_started = Instant::now();
    let row = sqlx::query("SELECT COUNT(*) AS matched, COALESCE(SUM(price_cents),0) AS total FROM tasks WHERE state_tag = 'Running'")
        .fetch_one(&mut connection).await?;
    let matched_rows = usize::try_from(row.get::<i64, _>("matched"))?;
    let total_price_cents = row.get::<i64, _>("total");
    let application_row = sqlx::query("SELECT id,title,state_tag,state_attempt,state_message,state_retry_at_micros,note_outer_some,note_value,price_cents,created_at_micros FROM tasks WHERE id = 1")
        .fetch_one(&mut connection).await?;
    let application_task = decode_sqlite_task(&application_row, None)?;
    if application_task.state != (ApplicationState::Running { attempt: 2 })
        || application_task.note != Some(None)
        || application_task.price_cents != 1025
    {
        return Err("SQLite relational columns did not reconstruct the application ADT".into());
    }
    let read_micros = read_started.elapsed().as_micros();

    let migration_started = Instant::now();
    sqlx::raw_sql(SQLITE_MIGRATION)
        .execute(&mut connection)
        .await?;
    let migrated_rows = usize::try_from(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tasks WHERE priority = 0")
            .fetch_one(&mut connection)
            .await?,
    )?;
    if migrated_rows != rows {
        return Err("SQLite migration did not default every existing row".into());
    }
    sqlx::query("INSERT INTO tasks (id,title,state_tag,state_archived_at_micros,note_outer_some,price_cents,created_at_micros,priority) VALUES (?,?,?,?,?,?,?,?)")
        .bind(i64::try_from(rows)?).bind("archived").bind("Archived")
        .bind(1_778_400_000_000_000_i64).bind(1_i64).bind(1025_i64)
        .bind(1_778_400_000_000_000_i64).bind(3_i64)
        .execute(&mut connection).await?;
    let archived_row = sqlx::query("SELECT id,title,state_tag,state_attempt,state_message,state_retry_at_micros,state_archived_at_micros,note_outer_some,note_value,price_cents,created_at_micros,priority FROM tasks WHERE id = ?")
        .bind(i64::try_from(rows)?)
        .fetch_one(&mut connection).await?;
    let archived = decode_sqlite_task(&archived_row, Some(archived_row.try_get("priority")?))?;
    if archived.state
        != (ApplicationState::Archived {
            at_micros: 1_778_400_000_000_000,
        })
        || archived.priority != Some(3)
    {
        return Err("SQLite migration did not reconstruct the added variant".into());
    }
    let migration_micros = migration_started.elapsed().as_micros();
    connection.close().await?;
    report(
        "sqlite-sqlx",
        rows,
        Measurements {
            startup_micros,
            write_micros,
            read_micros,
            migration_micros,
            matched_rows,
            total_price_cents,
        },
        path,
    )
}

fn sqlite_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
}

fn decimal_source(cents: i64) -> String {
    format!("decimal \"{}.{:02}\"", cents / 100, cents % 100)
}

fn decode_sqlite_task(
    row: &sqlx::sqlite::SqliteRow,
    priority: Option<i64>,
) -> AnyResult<ApplicationTask> {
    let state = match row.try_get::<&str, _>("state_tag")? {
        "Queued" => ApplicationState::Queued,
        "Running" => ApplicationState::Running {
            attempt: row.try_get("state_attempt")?,
        },
        "Failed" => ApplicationState::Failed {
            message: row.try_get("state_message")?,
            retry_at_micros: row.try_get("state_retry_at_micros")?,
        },
        "Done" => ApplicationState::Done,
        "Archived" => ApplicationState::Archived {
            at_micros: row.try_get("state_archived_at_micros")?,
        },
        tag => return Err(format!("unknown SQLite state tag {tag:?}").into()),
    };
    let note = match row.try_get::<i64, _>("note_outer_some")? {
        0 => None,
        1 => Some(row.try_get("note_value")?),
        value => return Err(format!("invalid nested option presence bit {value}").into()),
    };
    Ok(ApplicationTask {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        state,
        note,
        price_cents: row.try_get("price_cents")?,
        created_at_micros: row.try_get("created_at_micros")?,
        priority,
    })
}

fn report(
    backend: &'static str,
    rows: usize,
    measurements: Measurements,
    path: &Path,
) -> AnyResult<Report> {
    Ok(Report {
        backend,
        rows,
        durability: "single-file synchronous durable commit",
        startup_micros: measurements.startup_micros,
        write_micros: measurements.write_micros,
        read_micros: measurements.read_micros,
        migration_micros: measurements.migration_micros,
        matched_rows: measurements.matched_rows,
        total_price_cents: measurements.total_price_cents,
        migrated_rows: rows,
        database_bytes: database_bytes(path)?,
        peak_rss_bytes: peak_rss_bytes()?,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    })
}

fn require_ok(response: unionid::QueryResponse, phase: &str) -> AnyResult<()> {
    if response.ok {
        Ok(())
    } else {
        Err(format!(
            "{phase}: {}",
            response
                .error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "unknown error".into())
        )
        .into())
    }
}

fn remove_database(path: &Path) -> AnyResult<()> {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.exists() {
            std::fs::remove_file(candidate)?;
        }
    }
    Ok(())
}

fn database_bytes(path: &Path) -> AnyResult<u64> {
    let mut bytes = std::fs::metadata(path)?.len();
    for suffix in ["-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        if candidate.exists() {
            bytes += std::fs::metadata(candidate)?.len();
        }
    }
    Ok(bytes)
}

#[cfg(unix)]
fn peak_rss_bytes() -> AnyResult<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let rss = unsafe { usage.assume_init() }.ru_maxrss as u64;
    Ok(if cfg!(target_os = "macos") {
        rss
    } else {
        rss * 1024
    })
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> AnyResult<u64> {
    Ok(0)
}
