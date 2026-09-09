use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use unionid::{Engine, ExecutionObservation, StorageOpenProfile, Value};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const DEFAULT_BATCH_ROWS: usize = 3_000;

#[derive(Debug, Deserialize, Serialize)]
struct PhaseReport {
    phase: String,
    rows: usize,
    millis: u128,
    peak_rss_bytes: u64,
    database_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open_profile: Option<StorageOpenProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cold_indexed_read: Option<ExecutionObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    warm_indexed_read: Option<ExecutionObservation>,
}

#[derive(Debug, Serialize)]
struct EvaluationReport {
    rows: usize,
    batch_rows: usize,
    database_bytes: u64,
    prepare_millis: u128,
    open: PhaseReport,
    check: PhaseReport,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("recovery evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, command, path, rows, batch_rows] if command == "prepare" => {
            prepare(Path::new(path), rows.parse()?, batch_rows.parse()?)
        }
        [_, command, path, rows] if command == "measure-open" => {
            measure_open(Path::new(path), rows.parse()?)
        }
        [_, command, path, rows] if command == "measure-check" => {
            measure_check(Path::new(path), rows.parse()?)
        }
        [_, path, rows] => evaluate(Path::new(path), rows.parse()?, DEFAULT_BATCH_ROWS),
        [_, path, rows, batch_rows] => {
            evaluate(Path::new(path), rows.parse()?, batch_rows.parse()?)
        }
        _ => Err("usage: unionid-recovery-eval <path> <rows> [batch-rows]"
            .to_string()
            .into()),
    }
}

fn evaluate(path: &Path, rows: usize, batch_rows: usize) -> AnyResult<()> {
    validate_sizes(rows, batch_rows)?;
    remove_if_exists(path)?;
    let executable = std::env::current_exe()?;

    let prepare_started = Instant::now();
    let status = Command::new(&executable)
        .args([
            "prepare",
            path.to_str().ok_or("database path is not UTF-8")?,
            &rows.to_string(),
            &batch_rows.to_string(),
        ])
        .status()?;
    if !status.success() {
        return Err("workload preparation failed".into());
    }
    let prepare_millis = prepare_started.elapsed().as_millis();

    let open = run_phase(&executable, "measure-open", path, rows)?;
    let check = run_phase(&executable, "measure-check", path, rows)?;
    let report = EvaluationReport {
        rows,
        batch_rows,
        database_bytes: fs::metadata(path)?.len(),
        prepare_millis,
        open,
        check,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_phase(executable: &Path, phase: &str, path: &Path, rows: usize) -> AnyResult<PhaseReport> {
    let output = Command::new(executable)
        .args([
            phase,
            path.to_str().ok_or("database path is not UTF-8")?,
            &rows.to_string(),
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "{phase} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn prepare(path: &Path, rows: usize, batch_rows: usize) -> AnyResult<()> {
    validate_sizes(rows, batch_rows)?;
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let schema = engine.execute(
        "type State =\n  Pending\n  | Running\n    worker text\n    attempt int\n  | Done\n    result text\ntype Task =\n  id int\n  title text\n  tags list text\n  state State\ntable tasks Task\n  key id\ncreate index tasks (state)\ncreate index tasks (title)",
    );
    if !schema.ok {
        return Err(schema.message.into());
    }
    for start in (0..rows).step_by(batch_rows) {
        let end = rows.min(start + batch_rows);
        let mut source = String::new();
        for id in start..end {
            source.push_str("insert tasks {id = ");
            source.push_str(&id.to_string());
            source.push_str(", title = \"task-");
            source.push_str(&format!("{id:06}"));
            source.push_str("\", tags = [\"recovery\", \"benchmark\"], state = ");
            match id % 3 {
                0 => source.push_str("Pending"),
                1 => source.push_str(&format!(
                    "Running {{worker = \"worker-{}\", attempt = {}}}",
                    id % 16,
                    id % 8
                )),
                _ => source.push_str(&format!("Done {{result = \"result-{id:06}\"}}")),
            }
            source.push_str("}\n");
        }
        let response = engine.execute(&source);
        if !response.ok {
            return Err(format!("insert batch {start}..{end}: {}", response.message).into());
        }
    }
    let response = engine.execute("from tasks\naggregate\n  rows = count");
    require_count(&response, rows)?;
    Ok(())
}

fn measure_open(path: &Path, rows: usize) -> AnyResult<()> {
    let started = Instant::now();
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let millis = started.elapsed().as_millis();
    let open_profile = engine
        .open_profile()
        .ok_or("redb open profile is missing")?;
    if !open_profile.bounded_view
        || open_profile.row_entries != 0
        || open_profile.index_entries != 0
    {
        return Err("open materialized durable rows or indexes".into());
    }
    let target = rows / 2;
    let query = format!("from tasks | filter title == \"task-{target:06}\" | select id");
    let cold = engine.execute(&query);
    require_id(&cold, target)?;
    let cold_indexed_read = cold.execution.ok_or("cold query observation is missing")?;
    if cold_indexed_read.index_entries_examined != 1
        || cold_indexed_read.rows_decoded != 1
        || cold_indexed_read.row_cache_misses != 1
        || cold_indexed_read.row_cache_hits != 0
    {
        return Err(
            format!("unexpected cold indexed-read observation: {cold_indexed_read:?}").into(),
        );
    }
    let warm = engine.execute(&query);
    require_id(&warm, target)?;
    let warm_indexed_read = warm.execution.ok_or("warm query observation is missing")?;
    if warm_indexed_read.index_entries_examined != 1
        || warm_indexed_read.rows_decoded != 0
        || warm_indexed_read.row_cache_misses != 0
        || warm_indexed_read.row_cache_hits != 1
    {
        return Err(
            format!("unexpected warm indexed-read observation: {warm_indexed_read:?}").into(),
        );
    }
    let peak_rss_bytes = peak_rss_bytes()?;
    let response = engine.execute("from tasks\naggregate\n  rows = count");
    require_count(&response, rows)?;
    print_phase(PhaseReport {
        phase: "open".into(),
        rows,
        millis,
        peak_rss_bytes,
        database_bytes: fs::metadata(path)?.len(),
        open_profile: Some(open_profile),
        cold_indexed_read: Some(cold_indexed_read),
        warm_indexed_read: Some(warm_indexed_read),
    })
}

fn measure_check(path: &Path, rows: usize) -> AnyResult<()> {
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let started = Instant::now();
    engine.check_integrity()?;
    let millis = started.elapsed().as_millis();
    let peak_rss_bytes = peak_rss_bytes()?;
    let response = engine.execute("from tasks\naggregate\n  rows = count");
    require_count(&response, rows)?;
    print_phase(PhaseReport {
        phase: "check".into(),
        rows,
        millis,
        peak_rss_bytes,
        database_bytes: fs::metadata(path)?.len(),
        open_profile: None,
        cold_indexed_read: None,
        warm_indexed_read: None,
    })
}

fn require_id(response: &unionid::QueryResponse, expected: usize) -> AnyResult<()> {
    if !response.ok {
        return Err(response.message.clone().into());
    }
    let value = response
        .rows
        .first()
        .and_then(|row| row.get("id"))
        .ok_or("indexed query returned no id")?;
    let expected = i64::try_from(expected)?;
    if !value.cmp_eq(&Value::Int(expected)) {
        return Err(format!("expected id {expected}, received {}", value.source_text()).into());
    }
    Ok(())
}

fn require_count(response: &unionid::QueryResponse, expected: usize) -> AnyResult<()> {
    if !response.ok {
        return Err(response.message.clone().into());
    }
    let value = response
        .rows
        .first()
        .and_then(|row| row.get("rows"))
        .ok_or("count query returned no rows value")?;
    let expected = i64::try_from(expected)?;
    if !value.cmp_eq(&Value::Int(expected)) {
        return Err(format!("expected {expected} rows, received {}", value.source_text()).into());
    }
    Ok(())
}

fn print_phase(report: PhaseReport) -> AnyResult<()> {
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn peak_rss_bytes() -> AnyResult<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the provided rusage structure for this process.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success, so the structure is initialized.
    let peak = unsafe { usage.assume_init() }.ru_maxrss;
    let peak = u64::try_from(peak)?;
    if cfg!(target_os = "macos") {
        Ok(peak)
    } else {
        Ok(peak.saturating_mul(1024))
    }
}

fn validate_sizes(rows: usize, batch_rows: usize) -> AnyResult<()> {
    if rows == 0 || batch_rows == 0 {
        return Err("rows and batch-rows must be greater than zero".into());
    }
    if batch_rows > 3_000 {
        return Err("batch-rows must not exceed 3,000 to stay below parser limits".into());
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> AnyResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
