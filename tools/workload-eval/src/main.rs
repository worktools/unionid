use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use unionid::{Engine, QueryAccessKind, QueryResponse, UpsertAction, Value};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const DEFAULT_SAMPLES: usize = 20;
const DEFAULT_BATCH_ROWS: usize = 3_000;
const WARMUPS: usize = 5;
const BATCH_WRITE_ROWS: usize = 100;
const MAX_ROWS: usize = 1_000_000;

const MIGRATION: &str = r#"migration benchmark_task_v2
  rename variant State.Running to Claimed
  change variant State.Claimed to {worker text, attempt int, lease option text}
    using old -> {worker = old.worker, attempt = old.attempt, lease = None}
  add field Task.priority int = 0
  add index tasks.priority"#;

#[derive(Debug, Deserialize, Serialize)]
struct RawSamples {
    name: String,
    samples_micros: Vec<u64>,
    candidate_samples_micros: Option<Vec<u64>>,
    durable_commit_samples_micros: Option<Vec<u64>>,
    peak_rss_bytes: u64,
    access_kind: Option<QueryAccessKind>,
    index: Option<String>,
}

#[derive(Debug, Serialize)]
struct CaseReport {
    name: String,
    iterations: usize,
    samples_micros: Vec<u64>,
    p50_micros: u64,
    p95_micros: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_samples_micros: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_p50_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_p95_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    durable_commit_samples_micros: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    durable_commit_p50_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    durable_commit_p95_micros: Option<u64>,
    peak_rss_bytes: u64,
    access_kind: Option<QueryAccessKind>,
    index: Option<String>,
}

#[derive(Debug, Serialize)]
struct EvaluationReport {
    rows: usize,
    samples: usize,
    warmups: usize,
    preparation_batch_rows: usize,
    batch_write_rows: usize,
    database_bytes: u64,
    prepare_millis: u128,
    queries: Vec<CaseReport>,
    writes: Vec<CaseReport>,
    migration: CaseReport,
}

struct TemporaryDatabase(PathBuf);

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("workload evaluation failed: {error}");
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
        [_, command, path, rows, samples, case] if command == "measure-query" => {
            measure_query(Path::new(path), rows.parse()?, samples.parse()?, case)
        }
        [_, command, path, rows, samples, case] if command == "measure-write" => {
            measure_write(Path::new(path), rows.parse()?, samples.parse()?, case)
        }
        [_, command, path, rows] if command == "measure-migration" => {
            measure_migration(Path::new(path), rows.parse()?)
        }
        [_, path, rows] => evaluate(
            Path::new(path),
            rows.parse()?,
            DEFAULT_SAMPLES,
            DEFAULT_BATCH_ROWS,
        ),
        [_, path, rows, samples] => evaluate(
            Path::new(path),
            rows.parse()?,
            samples.parse()?,
            DEFAULT_BATCH_ROWS,
        ),
        [_, path, rows, samples, batch_rows] => evaluate(
            Path::new(path),
            rows.parse()?,
            samples.parse()?,
            batch_rows.parse()?,
        ),
        _ => Err(
            "usage: unionid-workload-eval <path> <rows> [samples] [batch-rows]"
                .to_string()
                .into(),
        ),
    }
}

fn evaluate(path: &Path, rows: usize, samples: usize, batch_rows: usize) -> AnyResult<()> {
    validate_sizes(rows, samples, batch_rows)?;
    remove_if_exists(path)?;
    let executable = std::env::current_exe()?;

    let prepare_started = Instant::now();
    run_child_status(
        &executable,
        &[
            "prepare",
            path.to_str().ok_or("database path is not UTF-8")?,
            &rows.to_string(),
            &batch_rows.to_string(),
        ],
    )?;
    let prepare_millis = prepare_started.elapsed().as_millis();

    let mut queries = Vec::new();
    for case in ["primary_key", "secondary_index", "full_scan"] {
        let database = copy_for_case(path, case, 0)?;
        let raw = run_child_samples(
            &executable,
            "measure-query",
            &database.0,
            rows,
            Some(samples),
            Some(case),
        )?;
        queries.push(summarize(raw)?);
    }

    let mut writes = Vec::new();
    for case in ["conditional_update", "upsert", "atomic_batch"] {
        let database = copy_for_case(path, case, 0)?;
        let raw = run_child_samples(
            &executable,
            "measure-write",
            &database.0,
            rows,
            Some(samples),
            Some(case),
        )?;
        writes.push(summarize(raw)?);
    }

    let mut migration_runs = Vec::with_capacity(samples);
    for sample in 0..samples {
        let database = copy_for_case(path, "migration", sample)?;
        migration_runs.push(run_child_samples(
            &executable,
            "measure-migration",
            &database.0,
            rows,
            None,
            None,
        )?);
    }
    let migration = summarize(combine_migration_runs(migration_runs)?)?;

    let report = EvaluationReport {
        rows,
        samples,
        warmups: WARMUPS,
        preparation_batch_rows: batch_rows,
        batch_write_rows: BATCH_WRITE_ROWS,
        database_bytes: fs::metadata(path)?.len(),
        prepare_millis,
        queries,
        writes,
        migration,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_child_status(executable: &Path, arguments: &[&str]) -> AnyResult<()> {
    let status = Command::new(executable).args(arguments).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("child process failed: {}", arguments.join(" ")).into())
    }
}

fn run_child_samples(
    executable: &Path,
    command: &str,
    path: &Path,
    rows: usize,
    samples: Option<usize>,
    case: Option<&str>,
) -> AnyResult<RawSamples> {
    let mut arguments = vec![
        command.to_string(),
        path.to_str()
            .ok_or("database path is not UTF-8")?
            .to_string(),
        rows.to_string(),
    ];
    if let Some(samples) = samples {
        arguments.push(samples.to_string());
    }
    if let Some(case) = case {
        arguments.push(case.to_string());
    }
    let output = Command::new(executable).args(&arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "{} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn copy_for_case(source: &Path, case: &str, sample: usize) -> AnyResult<TemporaryDatabase> {
    let destination = std::env::temp_dir().join(format!(
        "unionid-workload-{}-{case}-{sample}.redb",
        std::process::id()
    ));
    remove_if_exists(&destination)?;
    fs::copy(source, &destination)?;
    Ok(TemporaryDatabase(destination))
}

fn prepare(path: &Path, rows: usize, batch_rows: usize) -> AnyResult<()> {
    validate_sizes(rows, DEFAULT_SAMPLES, batch_rows)?;
    remove_if_exists(path)?;
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    require_ok(engine.execute(
        "type State =\n  Pending\n  | Running\n    worker text\n    attempt int\n  | Done\n    result text\ntype Task =\n  id int\n  title text\n  tags list text\n  state State\n  score int\ntable tasks Task\n  key id\ncreate index tasks (state)\ncreate index tasks (title)",
    ))?;
    for start in (0..rows).step_by(batch_rows) {
        let end = rows.min(start + batch_rows);
        let mut source = String::new();
        for id in start..end {
            source.push_str(&insert_source(id, (id % 100) as i64));
            source.push('\n');
        }
        require_ok(engine.execute(&source))?;
    }
    require_count(&mut engine, rows)?;
    Ok(())
}

fn measure_query(path: &Path, rows: usize, samples: usize, case: &str) -> AnyResult<()> {
    validate_sizes(rows, samples, DEFAULT_BATCH_ROWS)?;
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let target = rows / 2;
    let (source, expected_access, expected_index) = match case {
        "primary_key" => (
            format!("from tasks | filter id == {target} | take 1"),
            QueryAccessKind::PrimaryKeyLookup,
            Some("tasks.id"),
        ),
        "secondary_index" => (
            format!("from tasks | filter title == \"task-{target:06}\" | take 1"),
            QueryAccessKind::SecondaryIndexLookup,
            Some("tasks.title"),
        ),
        "full_scan" => (
            "from tasks | filter score >= 0 | take 1".to_string(),
            QueryAccessKind::FullScan,
            None,
        ),
        _ => return Err(format!("unknown query case '{case}'").into()),
    };
    require_plan(&mut engine, &source, expected_access, expected_index)?;
    for _ in 0..WARMUPS {
        require_one(engine.execute(&source))?;
    }
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let response = engine.execute(&source);
        timings.push(elapsed_micros(started));
        require_one(response)?;
    }
    print_raw(RawSamples {
        name: case.into(),
        samples_micros: timings,
        candidate_samples_micros: None,
        durable_commit_samples_micros: None,
        peak_rss_bytes: peak_rss_bytes()?,
        access_kind: Some(expected_access),
        index: expected_index.map(Into::into),
    })
}

fn measure_write(path: &Path, rows: usize, samples: usize, case: &str) -> AnyResult<()> {
    validate_sizes(rows, samples, DEFAULT_BATCH_ROWS)?;
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let target = rows / 2;
    let total = WARMUPS + samples;
    let sources = (0..total)
        .map(|iteration| write_source(case, rows, target, iteration))
        .collect::<AnyResult<Vec<_>>>()?;
    let (access_kind, index) = match case {
        "conditional_update" | "upsert" => {
            let lookup = format!("from tasks | filter id == {target} | take 1");
            require_plan(
                &mut engine,
                &lookup,
                QueryAccessKind::PrimaryKeyLookup,
                Some("tasks.id"),
            )?;
            (Some(QueryAccessKind::PrimaryKeyLookup), Some("tasks.id"))
        }
        "atomic_batch" => (None, None),
        _ => return Err(format!("unknown write case '{case}'").into()),
    };
    for source in &sources[..WARMUPS] {
        require_write(engine.execute(source), case)?;
        require_write_profile(&engine, case)?;
    }
    let mut timings = Vec::with_capacity(samples);
    let mut candidate_timings = Vec::with_capacity(samples);
    let mut durable_commit_timings = Vec::with_capacity(samples);
    for source in &sources[WARMUPS..] {
        let started = Instant::now();
        let response = engine.execute(source);
        timings.push(elapsed_micros(started));
        require_write(response, case)?;
        let profile = require_write_profile(&engine, case)?;
        candidate_timings.push(profile.candidate_micros);
        durable_commit_timings.push(profile.durable_commit_micros);
    }
    let peak_rss_bytes = peak_rss_bytes()?;
    validate_write_result(&mut engine, case, rows, target, total)?;
    print_raw(RawSamples {
        name: case.into(),
        samples_micros: timings,
        candidate_samples_micros: Some(candidate_timings),
        durable_commit_samples_micros: Some(durable_commit_timings),
        peak_rss_bytes,
        access_kind,
        index: index.map(Into::into),
    })
}

fn measure_migration(path: &Path, rows: usize) -> AnyResult<()> {
    validate_sizes(rows, DEFAULT_SAMPLES, DEFAULT_BATCH_ROWS)?;
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let started = Instant::now();
    let response = engine.execute(MIGRATION);
    let timing = elapsed_micros(started);
    require_ok(response)?;
    let profile = engine
        .last_mutation_profile()
        .ok_or("migration returned no mutation profile")?;
    if !profile.full_rebuild {
        return Err("migration unexpectedly used the incremental row-only path".into());
    }
    let peak_rss_bytes = peak_rss_bytes()?;
    let migrated = require_one(engine.execute("from tasks | filter priority == 0 | take 1"))?;
    if migrated.rows[0]["priority"].cmp_eq(&Value::Int(0)) {
        require_plan(
            &mut engine,
            "from tasks | filter priority == 0 | take 1",
            QueryAccessKind::SecondaryIndexLookup,
            Some("tasks.priority"),
        )?;
    } else {
        return Err("migration did not backfill Task.priority".into());
    }
    print_raw(RawSamples {
        name: "deep_migration".into(),
        samples_micros: vec![timing],
        candidate_samples_micros: Some(vec![profile.candidate_micros]),
        durable_commit_samples_micros: Some(vec![profile.durable_commit_micros]),
        peak_rss_bytes,
        access_kind: Some(QueryAccessKind::SecondaryIndexLookup),
        index: Some("tasks.priority".into()),
    })
}

fn write_source(case: &str, rows: usize, target: usize, iteration: usize) -> AnyResult<String> {
    Ok(match case {
        "conditional_update" => {
            format!("update tasks\nfilter id == {target}\nset score = score + 1")
        }
        "upsert" => {
            let score = i64::try_from(1_000 + iteration)?;
            format!("upsert tasks {}", record_source(target, score))
        }
        "atomic_batch" => {
            let start = rows + iteration * BATCH_WRITE_ROWS;
            let mut source = String::new();
            for id in start..start + BATCH_WRITE_ROWS {
                source.push_str(&insert_source(id, (id % 100) as i64));
                source.push('\n');
            }
            source
        }
        _ => return Err(format!("unknown write case '{case}'").into()),
    })
}

fn insert_source(id: usize, score: i64) -> String {
    format!("insert tasks {}", record_source(id, score))
}

fn record_source(id: usize, score: i64) -> String {
    let state = match id % 3 {
        0 => "Pending".to_string(),
        1 => format!(
            "Running {{worker = \"worker-{}\", attempt = {}}}",
            id % 16,
            id % 8
        ),
        _ => format!("Done {{result = \"result-{id:06}\"}}"),
    };
    format!(
        "{{id = {id}, title = \"task-{id:06}\", tags = [\"workload\", \"benchmark\"], state = {state}, score = {score}}}"
    )
}

fn validate_write_result(
    engine: &mut Engine,
    case: &str,
    rows: usize,
    target: usize,
    total: usize,
) -> AnyResult<()> {
    match case {
        "conditional_update" => {
            let response = require_one(
                engine.execute(&format!("from tasks | filter id == {target} | take 1")),
            )?;
            let initial = i64::try_from(target % 100)?;
            let expected = initial + i64::try_from(total)?;
            if !response.rows[0]["score"].cmp_eq(&Value::Int(expected)) {
                return Err("conditional update result mismatch".into());
            }
        }
        "upsert" => {
            let response = require_one(
                engine.execute(&format!("from tasks | filter id == {target} | take 1")),
            )?;
            let expected = i64::try_from(1_000 + total - 1)?;
            if !response.rows[0]["score"].cmp_eq(&Value::Int(expected)) {
                return Err("upsert result mismatch".into());
            }
        }
        "atomic_batch" => require_count(engine, rows + total * BATCH_WRITE_ROWS)?,
        _ => return Err(format!("unknown write case '{case}'").into()),
    }
    Ok(())
}

fn require_write(response: QueryResponse, case: &str) -> AnyResult<()> {
    if !response.ok {
        return Err(response.message.into());
    }
    if response.affected_rows != Some(1) {
        return Err(format!("{case} returned unexpected affected_rows").into());
    }
    if case == "upsert" && response.upsert_action != Some(UpsertAction::Updated) {
        return Err("upsert did not update the existing primary key".into());
    }
    Ok(())
}

fn require_write_profile(engine: &Engine, case: &str) -> AnyResult<unionid::MutationProfile> {
    let profile = engine
        .last_mutation_profile()
        .ok_or("write returned no mutation profile")?;
    if profile.full_rebuild {
        return Err(format!("{case} unexpectedly used the full-rebuild path").into());
    }
    let expected_inserts = usize::from(case == "atomic_batch") * BATCH_WRITE_ROWS;
    let expected_updates = usize::from(case != "atomic_batch");
    let expected_index_inserts = usize::from(case == "atomic_batch") * BATCH_WRITE_ROWS * 3;
    if profile.touched_tables != 1
        || profile.row_inserts != expected_inserts
        || profile.row_updates != expected_updates
        || profile.row_deletes != 0
        || profile.index_inserts != expected_index_inserts
        || profile.index_deletes != 0
        || profile.receipt_changes != 0
    {
        return Err(format!("{case} returned unexpected mutation profile {profile:?}").into());
    }
    Ok(profile)
}

fn require_plan(
    engine: &mut Engine,
    source: &str,
    expected: QueryAccessKind,
    index: Option<&str>,
) -> AnyResult<()> {
    let indented = source
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let response = require_ok(engine.execute(&format!("explain\n{indented}")))?;
    let plan = response.plan.ok_or("explain returned no plan")?;
    if plan.access.kind != expected || plan.access.index.as_deref() != index {
        return Err(format!(
            "expected {expected:?} via {index:?}, received {:?} via {:?}",
            plan.access.kind, plan.access.index
        )
        .into());
    }
    Ok(())
}

fn require_count(engine: &mut Engine, expected: usize) -> AnyResult<()> {
    let response = require_one(engine.execute("from tasks\naggregate\n  rows = count"))?;
    let expected = i64::try_from(expected)?;
    if !response.rows[0]["rows"].cmp_eq(&Value::Int(expected)) {
        return Err(format!(
            "expected {expected} rows, received {}",
            response.rows[0]["rows"].source_text()
        )
        .into());
    }
    Ok(())
}

fn require_one(response: QueryResponse) -> AnyResult<QueryResponse> {
    let response = require_ok(response)?;
    if response.rows.len() != 1 {
        return Err(format!("expected one row, received {}", response.rows.len()).into());
    }
    Ok(response)
}

fn require_ok(response: QueryResponse) -> AnyResult<QueryResponse> {
    if response.ok {
        Ok(response)
    } else {
        Err(response.message.into())
    }
}

fn combine_migration_runs(runs: Vec<RawSamples>) -> AnyResult<RawSamples> {
    let mut combined = RawSamples {
        name: "deep_migration".into(),
        samples_micros: Vec::with_capacity(runs.len()),
        candidate_samples_micros: Some(Vec::with_capacity(runs.len())),
        durable_commit_samples_micros: Some(Vec::with_capacity(runs.len())),
        peak_rss_bytes: 0,
        access_kind: Some(QueryAccessKind::SecondaryIndexLookup),
        index: Some("tasks.priority".into()),
    };
    for run in runs {
        if run.name != combined.name
            || run.samples_micros.len() != 1
            || run.access_kind != combined.access_kind
            || run.index != combined.index
        {
            return Err("migration child returned inconsistent metadata".into());
        }
        combined.samples_micros.push(run.samples_micros[0]);
        combined
            .candidate_samples_micros
            .as_mut()
            .ok_or("migration child omitted candidate samples")?
            .push(
                run.candidate_samples_micros
                    .as_ref()
                    .and_then(|samples| samples.first())
                    .copied()
                    .ok_or("migration child returned no candidate sample")?,
            );
        combined
            .durable_commit_samples_micros
            .as_mut()
            .ok_or("migration child omitted durable commit samples")?
            .push(
                run.durable_commit_samples_micros
                    .as_ref()
                    .and_then(|samples| samples.first())
                    .copied()
                    .ok_or("migration child returned no durable commit sample")?,
            );
        combined.peak_rss_bytes = combined.peak_rss_bytes.max(run.peak_rss_bytes);
    }
    Ok(combined)
}

fn summarize(mut raw: RawSamples) -> AnyResult<CaseReport> {
    if raw.samples_micros.is_empty() {
        return Err(format!("{} returned no samples", raw.name).into());
    }
    let mut sorted = raw.samples_micros.clone();
    sorted.sort_unstable();
    let p50_micros = percentile(&sorted, 50);
    let p95_micros = percentile(&sorted, 95);
    let (candidate_p50_micros, candidate_p95_micros) =
        summarize_phase(&raw.candidate_samples_micros, raw.samples_micros.len())?;
    let (durable_commit_p50_micros, durable_commit_p95_micros) =
        summarize_phase(&raw.durable_commit_samples_micros, raw.samples_micros.len())?;
    Ok(CaseReport {
        name: raw.name,
        iterations: raw.samples_micros.len(),
        samples_micros: std::mem::take(&mut raw.samples_micros),
        p50_micros,
        p95_micros,
        candidate_samples_micros: raw.candidate_samples_micros.take(),
        candidate_p50_micros,
        candidate_p95_micros,
        durable_commit_samples_micros: raw.durable_commit_samples_micros.take(),
        durable_commit_p50_micros,
        durable_commit_p95_micros,
        peak_rss_bytes: raw.peak_rss_bytes,
        access_kind: raw.access_kind,
        index: raw.index,
    })
}

fn summarize_phase(
    samples: &Option<Vec<u64>>,
    expected: usize,
) -> AnyResult<(Option<u64>, Option<u64>)> {
    let Some(samples) = samples else {
        return Ok((None, None));
    };
    if samples.len() != expected {
        return Err("phase sample count does not match total sample count".into());
    }
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    Ok((Some(percentile(&sorted, 50)), Some(percentile(&sorted, 95))))
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn print_raw(report: RawSamples) -> AnyResult<()> {
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
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

fn validate_sizes(rows: usize, samples: usize, batch_rows: usize) -> AnyResult<()> {
    if rows == 0 || samples == 0 || batch_rows == 0 {
        return Err("rows, samples, and batch-rows must be greater than zero".into());
    }
    if rows > MAX_ROWS {
        return Err(format!("rows must not exceed {MAX_ROWS}").into());
    }
    if samples > 1_000 {
        return Err("samples must not exceed 1,000".into());
    }
    if batch_rows > DEFAULT_BATCH_ROWS {
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
