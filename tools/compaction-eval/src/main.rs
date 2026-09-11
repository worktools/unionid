use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::Serialize;
use unionid::{Engine, Value};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const TABLE: &str = "entries";

#[derive(Debug, Serialize)]
struct EnvironmentReport {
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    rustc: String,
}

#[derive(Debug, Serialize)]
struct CompactionReport {
    environment: EnvironmentReport,
    database_path: String,
    rows: usize,
    before_rows: usize,
    after_rows: usize,
    schema_revision: u64,
    schema_hash: String,
    before_bytes: u64,
    after_bytes: u64,
    durable_bytes: u64,
    reclaimed_bytes: u64,
    changed: bool,
    identity_preserved: bool,
    open_micros: u128,
    compact_micros: u128,
    check_micros: u128,
    peak_rss_bytes: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("compaction evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, command, path, rows] if command == "measure" => {
            measure(Path::new(path), Some(rows.parse()?))
        }
        [_, command, path] if command == "measure" => measure(Path::new(path), None),
        [_, path, rows] => evaluate(Path::new(path), Some(rows.parse()?)),
        [_, path] => evaluate(Path::new(path), None),
        _ => Err("usage: unionid-compaction-eval <database> [rows]".into()),
    }
}

fn evaluate(path: &Path, rows: Option<usize>) -> AnyResult<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(&executable);
    command.args([
        "measure",
        path.to_str().ok_or("database path is not UTF-8")?,
    ]);
    if let Some(rows) = rows {
        command.arg(rows.to_string());
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "measurement failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn measure(path: &Path, expected_rows: Option<usize>) -> AnyResult<()> {
    let open_started = Instant::now();
    let mut engine = Engine::open_redb(path)?;
    let open_micros = open_started.elapsed().as_micros();
    let schema = engine.schema_info();
    let before_rows = count_rows(&mut engine)?;
    if let Some(rows) = expected_rows
        && before_rows != rows
    {
        return Err(format!("expected {rows} rows before compaction, found {before_rows}").into());
    }
    let rows = before_rows;

    let compact_started = Instant::now();
    let report = engine.compact_storage()?;
    let compact_micros = compact_started.elapsed().as_micros();

    let check_started = Instant::now();
    let integrity = engine.check_integrity()?;
    let check_micros = check_started.elapsed().as_micros();
    if !integrity.backend_clean {
        return Err("post-compaction integrity check reported an unclean backend".into());
    }
    let after_rows = count_rows(&mut engine)?;
    if after_rows != rows {
        return Err(format!("expected {rows} rows after compaction, found {after_rows}").into());
    }
    if engine.schema_info() != schema {
        return Err("schema identity changed across compaction".into());
    }
    if !report.identity_preserved {
        return Err("compaction did not preserve durable identity".into());
    }
    let peak_rss_bytes = peak_rss_bytes()?;
    let reported_after = report.after_bytes;
    if fs::metadata(path)?.len() != reported_after {
        return Err("reported after_bytes does not match the file size".into());
    }
    let before_bytes = report.before_bytes;
    let reclaimed_bytes = report.reclaimed_bytes;
    let changed = report.changed;
    let identity_preserved = report.identity_preserved;
    drop(engine);
    let durable_bytes = fs::metadata(path)?.len();

    let output = CompactionReport {
        environment: environment()?,
        database_path: path.display().to_string(),
        rows,
        before_rows,
        after_rows,
        schema_revision: schema.revision,
        schema_hash: schema.hash,
        before_bytes,
        after_bytes: reported_after,
        reclaimed_bytes,
        changed,
        identity_preserved,
        durable_bytes,
        open_micros,
        compact_micros,
        check_micros,
        peak_rss_bytes,
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn count_rows(engine: &mut Engine) -> AnyResult<usize> {
    let response = engine.execute(&format!("from {TABLE}\naggregate\n  rows = count"));
    if !response.ok {
        return Err(response.message.into());
    }
    let value = response
        .rows
        .first()
        .and_then(|row| row.get("rows"))
        .ok_or("count query returned no rows value")?;
    match value.unwrapped() {
        Value::Int(count) => Ok(usize::try_from(*count)?),
        other => Err(format!("count query returned {}", other.source_text()).into()),
    }
}

fn environment() -> AnyResult<EnvironmentReport> {
    let rustc = Command::new("rustc").arg("--version").output()?;
    if !rustc.status.success() {
        return Err("rustc --version failed".into());
    }
    Ok(EnvironmentReport {
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        logical_cpus: std::thread::available_parallelism()?.get(),
        rustc: String::from_utf8(rustc.stdout)?.trim().into(),
    })
}

#[cfg(unix)]
fn peak_rss_bytes() -> AnyResult<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the provided rusage structure for this process.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success, so the structure is initialized.
    let rss = unsafe { usage.assume_init() }.ru_maxrss;
    let bytes = if cfg!(target_os = "macos") {
        u64::try_from(rss)?
    } else {
        u64::try_from(rss)?.saturating_mul(1024)
    };
    Ok(bytes)
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> AnyResult<u64> {
    Ok(0)
}
