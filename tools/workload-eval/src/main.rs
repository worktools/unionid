use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use unionid::{
    DurableCommitMode, DurableCommitProfile, Engine, IndexTraversal, PageAccessKind, PagePlan,
    QueryAccessKind, QueryAccessPlan, QueryPlan, QueryResponse, StorageOpenProfile, UpsertAction,
    Value,
};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const DEFAULT_SAMPLES: usize = 20;
const DEFAULT_BATCH_ROWS: usize = 1_500;
const WARMUPS: usize = 5;
const BATCH_WRITE_ROWS: usize = 100;
const ORDERED_READ_ROWS: usize = 25;
const MIN_ROWS: usize = 100;
const MAX_ROWS: usize = 1_000_000;

const MIGRATION: &str = r#"migration benchmark_task_v2
  rename variant State.Running to Claimed
  change variant State.Claimed to {worker text, attempt int, lease option text}
    using old -> {worker = old.worker, attempt = old.attempt, lease = None}
  add field Task.migrated_rank int = 0
  add index tasks.migrated_rank"#;

#[derive(Debug, Deserialize, Serialize)]
struct RawSamples {
    name: String,
    samples_micros: Vec<u64>,
    candidate_samples_micros: Option<Vec<u64>>,
    durable_commit_samples_micros: Option<Vec<u64>>,
    peak_rss_bytes: u64,
    access_kind: Option<QueryAccessKind>,
    index: Option<String>,
    access_plan: Option<QueryAccessPlan>,
    page_plan: Option<PagePlan>,
    open_profiles: Option<Vec<StorageOpenProfile>>,
    durable_profiles: Option<Vec<DurableCommitProfile>>,
}

#[derive(Debug, Serialize)]
struct PhasePercentiles {
    p50_micros: BTreeMap<String, u64>,
    p95_micros: BTreeMap<String, u64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    access_plan: Option<QueryAccessPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_plan: Option<PagePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_profiles: Option<Vec<StorageOpenProfile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    durable_profiles: Option<Vec<DurableCommitProfile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_phase_percentiles: Option<PhasePercentiles>,
}

#[derive(Debug, Serialize)]
struct EnvironmentReport {
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    rustc: String,
}

#[derive(Debug, Serialize)]
struct EvaluationReport {
    environment: EnvironmentReport,
    rows: usize,
    samples: usize,
    warmups: usize,
    preparation_batch_rows: usize,
    batch_write_rows: usize,
    database_bytes: u64,
    prepare_millis: u128,
    schema_revision: u64,
    schema_hash: String,
    open: CaseReport,
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
        [_, command, path, rows] if command == "measure-open" => {
            measure_open(Path::new(path), rows.parse()?)
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

    let mut open_runs = Vec::with_capacity(samples);
    for _ in 0..samples {
        open_runs.push(run_child_samples(
            &executable,
            "measure-open",
            path,
            rows,
            None,
            None,
        )?);
    }
    let open = summarize(combine_open_runs(open_runs)?)?;

    let mut queries = Vec::new();
    for case in [
        "primary_key",
        "secondary_index",
        "full_scan",
        "composite_range",
        "composite_order",
        "page_seek_forward",
        "page_seek_backward",
    ] {
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

    let mut template = Engine::open_redb(path.to_path_buf())?;
    require_count(&mut template, rows)?;
    require_integrity(&mut template)?;
    let schema = template.schema_info();
    drop(template);

    let report = EvaluationReport {
        environment: environment_report()?,
        rows,
        samples,
        warmups: WARMUPS,
        preparation_batch_rows: batch_rows,
        batch_write_rows: BATCH_WRITE_ROWS,
        database_bytes: fs::metadata(path)?.len(),
        prepare_millis,
        schema_revision: schema.revision,
        schema_hash: schema.hash,
        open,
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
        "type State =\n  Pending\n  | Running\n    worker text\n    attempt int\n  | Done\n    result text\ntype Task =\n  id int\n  tenant text\n  priority int\n  title text\n  tags list text\n  state State\n  score int\ntable tasks Task\n  key id\ncreate index tasks (state)\ncreate index tasks (title)\ncreate index tasks (tenant, -priority, id)",
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
    require_integrity(&mut engine)?;
    Ok(())
}

fn measure_query(path: &Path, rows: usize, samples: usize, case: &str) -> AnyResult<()> {
    validate_sizes(rows, samples, DEFAULT_BATCH_ROWS)?;
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let target = rows / 2;
    let tenant = format!("tenant-{}", target % 10);
    let ordered = format!("from tasks\nfilter tenant == \"{tenant}\"\nsort {{-priority, id}}");
    let ordered_read_rows = ORDERED_READ_ROWS.min(rows / 30);
    let page_source = match case {
        "page_seek_forward" => Some(page_seek_source(
            &mut engine,
            &ordered,
            ordered_read_rows,
            false,
        )?),
        "page_seek_backward" => Some(page_seek_source(
            &mut engine,
            &ordered,
            ordered_read_rows,
            true,
        )?),
        _ => None,
    };
    let composite_index = Some("tasks (tenant, -priority, id)");
    let (source, expected_access, expected_index, expected_rows) = match case {
        "primary_key" => (
            format!("from tasks | filter id == {target} | take 1"),
            QueryAccessKind::PrimaryKeyLookup,
            Some("tasks.id"),
            1,
        ),
        "secondary_index" => (
            format!("from tasks | filter title == \"task-{target:06}\" | take 1"),
            QueryAccessKind::SecondaryIndexLookup,
            Some("tasks.title"),
            1,
        ),
        "full_scan" => (
            "from tasks | filter score >= 0 | take 1".to_string(),
            QueryAccessKind::FullScan,
            None,
            1,
        ),
        "composite_range" => (
            format!(
                "from tasks\nfilter tenant == \"{tenant}\"\nfilter priority >= 50\nsort {{-priority, id}}\ntake {ordered_read_rows}"
            ),
            QueryAccessKind::RangeScan,
            composite_index,
            ordered_read_rows,
        ),
        "composite_order" => (
            format!("{ordered}\ntake {ordered_read_rows}"),
            QueryAccessKind::OrderedScan,
            composite_index,
            ordered_read_rows,
        ),
        "page_seek_forward" | "page_seek_backward" => (
            page_source.expect("page source was prepared"),
            QueryAccessKind::PageSeek,
            composite_index,
            ordered_read_rows,
        ),
        _ => return Err(format!("unknown query case '{case}'").into()),
    };
    let plan = require_plan(&mut engine, &source, expected_access, expected_index)?;
    validate_query_plan(case, &plan, ordered_read_rows)?;
    for _ in 0..WARMUPS {
        require_rows(engine.execute(&source), expected_rows)?;
    }
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let response = engine.execute(&source);
        timings.push(elapsed_micros(started));
        require_rows(response, expected_rows)?;
    }
    require_integrity(&mut engine)?;
    print_raw(RawSamples {
        name: case.into(),
        samples_micros: timings,
        candidate_samples_micros: None,
        durable_commit_samples_micros: None,
        peak_rss_bytes: peak_rss_bytes()?,
        access_kind: Some(expected_access),
        index: expected_index.map(Into::into),
        access_plan: Some(plan.access),
        page_plan: plan.page,
        open_profiles: None,
        durable_profiles: None,
    })
}

fn page_seek_source(
    engine: &mut Engine,
    ordered: &str,
    read_rows: usize,
    backward: bool,
) -> AnyResult<String> {
    let first_source = format!("{ordered}\npage {read_rows}");
    let first = require_rows(engine.execute(&first_source), read_rows)?;
    let next = first
        .page
        .and_then(|page| page.next_cursor)
        .ok_or("first ordered page did not return a next cursor")?;
    let second_source = format!(
        "{ordered}\npage {read_rows} after {}",
        serde_json::to_string(&next)?
    );
    if !backward {
        return Ok(second_source);
    }
    let second = require_rows(engine.execute(&second_source), read_rows)?;
    let previous = second
        .page
        .and_then(|page| page.previous_cursor)
        .ok_or("second ordered page did not return a previous cursor")?;
    Ok(format!(
        "{ordered}\npage {read_rows} before {}",
        serde_json::to_string(&previous)?
    ))
}

fn validate_query_plan(case: &str, plan: &QueryPlan, read_rows: usize) -> AnyResult<()> {
    match case {
        "composite_range" => {
            let range = plan
                .access
                .range
                .as_ref()
                .ok_or("range plan omitted range")?;
            if plan.access.equality_prefix != ["tenant"]
                || range.column != "priority"
                || range.lower_inclusive != Some(true)
                || range.upper_inclusive.is_some()
                || plan.access.traversal != Some(IndexTraversal::Forward)
                || !plan.access.sort_satisfied
            {
                return Err(format!("unexpected composite range plan: {:?}", plan.access).into());
            }
        }
        "composite_order" => {
            if plan.access.equality_prefix != ["tenant"]
                || plan.access.range.is_some()
                || plan.access.traversal != Some(IndexTraversal::Forward)
                || !plan.access.sort_satisfied
            {
                return Err(format!("unexpected composite order plan: {:?}", plan.access).into());
            }
        }
        "page_seek_forward" | "page_seek_backward" => {
            let page = plan.page.as_ref().ok_or("page seek omitted page plan")?;
            if !plan.access.page_seek
                || plan.access.traversal != Some(IndexTraversal::Forward)
                || !plan.access.sort_satisfied
                || page.access != PageAccessKind::IndexSeek
                || !page.resume_boundary
                || page.read_limit != read_rows + 1
            {
                return Err(format!("unexpected page seek plan: {plan:?}").into());
            }
        }
        _ => {}
    }
    Ok(())
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
    let mut durable_profiles = Vec::with_capacity(samples);
    for source in &sources[WARMUPS..] {
        let started = Instant::now();
        let response = engine.execute(source);
        let timing = elapsed_micros(started);
        timings.push(timing);
        require_write(response, case)?;
        let profile = require_write_profile(&engine, case)?;
        validate_durable_profile(
            profile
                .durable
                .ok_or("durable write omitted its commit phase profile")?,
            timing,
        )?;
        candidate_timings.push(profile.candidate_micros);
        durable_commit_timings.push(profile.durable_commit_micros);
        durable_profiles.push(
            profile
                .durable
                .ok_or("durable write omitted its commit phase profile")?,
        );
    }
    let peak_rss_bytes = peak_rss_bytes()?;
    validate_write_result(&mut engine, case, rows, target, total)?;
    require_integrity(&mut engine)?;
    print_raw(RawSamples {
        name: case.into(),
        samples_micros: timings,
        candidate_samples_micros: Some(candidate_timings),
        durable_commit_samples_micros: Some(durable_commit_timings),
        peak_rss_bytes,
        access_kind,
        index: index.map(Into::into),
        access_plan: None,
        page_plan: None,
        open_profiles: None,
        durable_profiles: Some(durable_profiles),
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
    let durable_profile = profile
        .durable
        .ok_or("durable migration omitted its commit phase profile")?;
    if durable_profile.mode != DurableCommitMode::FullRebuild
        || durable_profile.total_micros != profile.durable_commit_micros
    {
        return Err("migration returned inconsistent full-rebuild phase metadata".into());
    }
    validate_durable_profile(durable_profile, timing)?;
    if durable_profile.row_changes != rows
        || durable_profile.index_changes == 0
        || durable_profile.encoded_change_bytes == 0
    {
        return Err("migration returned incomplete full-rebuild change counts".into());
    }
    let peak_rss_bytes = peak_rss_bytes()?;
    let migrated = require_one(engine.execute("from tasks | filter migrated_rank == 0 | take 1"))?;
    if migrated.rows[0]["migrated_rank"].cmp_eq(&Value::Int(0)) {
        let _ = require_plan(
            &mut engine,
            "from tasks | filter migrated_rank == 0 | take 1",
            QueryAccessKind::SecondaryIndexLookup,
            Some("tasks.migrated_rank"),
        )?;
    } else {
        return Err("migration did not backfill Task.migrated_rank".into());
    }
    require_integrity(&mut engine)?;
    print_raw(RawSamples {
        name: "deep_migration".into(),
        samples_micros: vec![timing],
        candidate_samples_micros: Some(vec![profile.candidate_micros]),
        durable_commit_samples_micros: Some(vec![profile.durable_commit_micros]),
        peak_rss_bytes,
        access_kind: Some(QueryAccessKind::SecondaryIndexLookup),
        index: Some("tasks.migrated_rank".into()),
        access_plan: None,
        page_plan: None,
        open_profiles: None,
        durable_profiles: Some(vec![durable_profile]),
    })
}

fn measure_open(path: &Path, rows: usize) -> AnyResult<()> {
    validate_sizes(rows, DEFAULT_SAMPLES, DEFAULT_BATCH_ROWS)?;
    let started = Instant::now();
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    let timing = elapsed_micros(started);
    let open_profile = engine
        .open_profile()
        .ok_or("durable open omitted its phase profile")?;
    validate_open_profile(open_profile, timing, rows)?;
    let peak_rss_bytes = peak_rss_bytes()?;
    require_count(&mut engine, rows)?;
    require_integrity(&mut engine)?;
    print_raw(RawSamples {
        name: "open".into(),
        samples_micros: vec![timing],
        candidate_samples_micros: None,
        durable_commit_samples_micros: None,
        peak_rss_bytes,
        access_kind: None,
        index: None,
        access_plan: None,
        page_plan: None,
        open_profiles: Some(vec![open_profile]),
        durable_profiles: None,
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
        "{{id = {id}, tenant = \"tenant-{}\", priority = {}, title = \"task-{id:06}\", tags = [\"workload\", \"benchmark\"], state = {state}, score = {score}}}",
        id % 10,
        id % 100,
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
    let durable = profile
        .durable
        .ok_or_else(|| format!("{case} omitted its durable phase profile"))?;
    if durable.mode != DurableCommitMode::Incremental
        || durable.total_micros != profile.durable_commit_micros
        || durable.reload_previous_micros != 0
        || durable.encode_next_micros != 0
        || durable.diff_micros != 0
    {
        return Err(format!("{case} returned inconsistent incremental phase metadata").into());
    }
    let expected_inserts = usize::from(case == "atomic_batch") * BATCH_WRITE_ROWS;
    let expected_updates = usize::from(case != "atomic_batch");
    let expected_index_inserts = usize::from(case == "atomic_batch") * BATCH_WRITE_ROWS * 4;
    let expected_durable_rows = expected_inserts + expected_updates;
    let expected_durable_indexes = expected_index_inserts;
    let expected_catalog_changes = usize::from(case == "atomic_batch");
    if profile.touched_tables != 1
        || profile.row_inserts != expected_inserts
        || profile.row_updates != expected_updates
        || profile.row_deletes != 0
        || profile.index_inserts != expected_index_inserts
        || profile.index_deletes != 0
        || profile.receipt_changes != 0
        || durable.catalog_changes != expected_catalog_changes
        || durable.row_changes != expected_durable_rows
        || durable.index_changes != expected_durable_indexes
        || durable.migration_changes != 0
        || durable.receipt_changes != 0
        || durable.encoded_change_bytes == 0
    {
        return Err(format!("{case} returned unexpected mutation profile {profile:?}").into());
    }
    Ok(profile)
}

fn validate_open_profile(
    profile: StorageOpenProfile,
    outer_micros: u64,
    expected_rows: usize,
) -> AnyResult<()> {
    let exclusive_sum = profile
        .redb_open_micros
        .saturating_add(profile.bootstrap_micros)
        .saturating_add(profile.meta_micros)
        .saturating_add(profile.catalog_micros)
        .saturating_add(profile.rows_micros)
        .saturating_add(profile.indexes_micros)
        .saturating_add(profile.migrations_micros)
        .saturating_add(profile.receipts_micros)
        .saturating_add(profile.database_construct_micros)
        .saturating_add(profile.validation_micros);
    if profile.fresh
        || profile.read_only
        || profile.cursor_upgrade
        || profile.total_micros > outer_micros
        || exclusive_sum > profile.total_micros
        || profile.row_entries != expected_rows
        || profile.index_entries != expected_rows.saturating_mul(4)
        || profile.catalog_entries == 0
        || profile.row_bytes == 0
        || profile.index_key_bytes == 0
    {
        return Err(format!("open returned inconsistent phase metadata {profile:?}").into());
    }
    Ok(())
}

fn validate_durable_profile(profile: DurableCommitProfile, outer_micros: u64) -> AnyResult<()> {
    let top_level_sum = profile
        .prepare_micros
        .saturating_add(profile.transaction_apply_micros)
        .saturating_add(profile.sync_micros);
    let prepare_subphase_sum = profile
        .reload_previous_micros
        .saturating_add(profile.encode_next_micros)
        .saturating_add(profile.diff_micros);
    if profile.total_micros > outer_micros
        || top_level_sum > profile.total_micros
        || prepare_subphase_sum > profile.prepare_micros
    {
        return Err(
            format!("durable commit returned inconsistent phase timings {profile:?}").into(),
        );
    }
    Ok(())
}

fn require_plan(
    engine: &mut Engine,
    source: &str,
    expected: QueryAccessKind,
    index: Option<&str>,
) -> AnyResult<QueryPlan> {
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
    Ok(plan)
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

fn require_rows(response: QueryResponse, expected: usize) -> AnyResult<QueryResponse> {
    let response = require_ok(response)?;
    if response.rows.len() != expected {
        return Err(format!("expected {expected} rows, received {}", response.rows.len()).into());
    }
    Ok(response)
}

fn require_integrity(engine: &mut Engine) -> AnyResult<()> {
    let integrity = engine.check_integrity()?;
    if !integrity.backend_clean {
        return Err("redb backend integrity check was not clean".into());
    }
    Ok(())
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
        index: Some("tasks.migrated_rank".into()),
        access_plan: None,
        page_plan: None,
        open_profiles: None,
        durable_profiles: Some(Vec::with_capacity(runs.len())),
    };
    for run in runs {
        if run.name != combined.name
            || run.samples_micros.len() != 1
            || run.access_kind != combined.access_kind
            || run.index != combined.index
            || run.open_profiles.is_some()
            || run.durable_profiles.as_ref().is_none_or(|profiles| {
                profiles.len() != 1 || profiles[0].mode != DurableCommitMode::FullRebuild
            })
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
        combined
            .durable_profiles
            .as_mut()
            .expect("combined migration profiles are initialized")
            .push(run.durable_profiles.expect("validated migration profile")[0]);
    }
    Ok(combined)
}

fn combine_open_runs(runs: Vec<RawSamples>) -> AnyResult<RawSamples> {
    let mut combined = RawSamples {
        name: "open".into(),
        samples_micros: Vec::with_capacity(runs.len()),
        candidate_samples_micros: None,
        durable_commit_samples_micros: None,
        peak_rss_bytes: 0,
        access_kind: None,
        index: None,
        access_plan: None,
        page_plan: None,
        open_profiles: Some(Vec::with_capacity(runs.len())),
        durable_profiles: None,
    };
    for run in runs {
        if run.name != combined.name
            || run.samples_micros.len() != 1
            || run.candidate_samples_micros.is_some()
            || run.durable_commit_samples_micros.is_some()
            || run.access_kind.is_some()
            || run.index.is_some()
            || run.durable_profiles.is_some()
            || run
                .open_profiles
                .as_ref()
                .is_none_or(|profiles| profiles.len() != 1)
        {
            return Err("open child returned inconsistent metadata".into());
        }
        combined.samples_micros.push(run.samples_micros[0]);
        combined.peak_rss_bytes = combined.peak_rss_bytes.max(run.peak_rss_bytes);
        combined
            .open_profiles
            .as_mut()
            .expect("combined open profiles are initialized")
            .push(run.open_profiles.expect("validated open profile")[0]);
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
    let storage_phase_percentiles = summarize_storage_phases(
        &raw.open_profiles,
        &raw.durable_profiles,
        raw.samples_micros.len(),
    )?;
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
        access_plan: raw.access_plan,
        page_plan: raw.page_plan,
        open_profiles: raw.open_profiles.take(),
        durable_profiles: raw.durable_profiles.take(),
        storage_phase_percentiles,
    })
}

fn summarize_storage_phases(
    open_profiles: &Option<Vec<StorageOpenProfile>>,
    durable_profiles: &Option<Vec<DurableCommitProfile>>,
    expected: usize,
) -> AnyResult<Option<PhasePercentiles>> {
    if open_profiles.is_some() && durable_profiles.is_some() {
        return Err("case mixed open and durable storage profiles".into());
    }
    let mut phases = BTreeMap::<String, Vec<u64>>::new();
    if let Some(profiles) = open_profiles {
        if profiles.len() != expected {
            return Err("open profile count does not match total sample count".into());
        }
        for profile in profiles {
            for (name, value) in open_phase_values(*profile) {
                phases.entry(name.into()).or_default().push(value);
            }
        }
    } else if let Some(profiles) = durable_profiles {
        if profiles.len() != expected {
            return Err("durable profile count does not match total sample count".into());
        }
        for profile in profiles {
            for (name, value) in durable_phase_values(*profile) {
                phases.entry(name.into()).or_default().push(value);
            }
        }
    } else {
        return Ok(None);
    }
    let mut p50_micros = BTreeMap::new();
    let mut p95_micros = BTreeMap::new();
    for (name, mut samples) in phases {
        if samples.len() != expected {
            return Err("storage phase schema changed between samples".into());
        }
        samples.sort_unstable();
        p50_micros.insert(name.clone(), percentile(&samples, 50));
        p95_micros.insert(name, percentile(&samples, 95));
    }
    Ok(Some(PhasePercentiles {
        p50_micros,
        p95_micros,
    }))
}

fn open_phase_values(profile: StorageOpenProfile) -> [(&'static str, u64); 11] {
    [
        ("total", profile.total_micros),
        ("redb_open", profile.redb_open_micros),
        ("bootstrap", profile.bootstrap_micros),
        ("meta", profile.meta_micros),
        ("catalog", profile.catalog_micros),
        ("rows", profile.rows_micros),
        ("indexes", profile.indexes_micros),
        ("migrations", profile.migrations_micros),
        ("receipts", profile.receipts_micros),
        ("database_construct", profile.database_construct_micros),
        ("validation", profile.validation_micros),
    ]
}

fn durable_phase_values(profile: DurableCommitProfile) -> [(&'static str, u64); 7] {
    [
        ("total", profile.total_micros),
        ("prepare", profile.prepare_micros),
        ("reload_previous", profile.reload_previous_micros),
        ("encode_next", profile.encode_next_micros),
        ("diff", profile.diff_micros),
        ("transaction_apply", profile.transaction_apply_micros),
        ("sync", profile.sync_micros),
    ]
}

fn environment_report() -> AnyResult<EnvironmentReport> {
    let rustc = Command::new("rustc").arg("--version").output()?;
    if !rustc.status.success() {
        return Err("rustc --version failed".into());
    }
    Ok(EnvironmentReport {
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        logical_cpus: std::thread::available_parallelism()?.get(),
        rustc: String::from_utf8(rustc.stdout)?.trim().to_string(),
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
    if rows < MIN_ROWS {
        return Err(format!("rows must be at least {MIN_ROWS} for ordered/page cases").into());
    }
    if samples > 1_000 {
        return Err("samples must not exceed 1,000".into());
    }
    if batch_rows > DEFAULT_BATCH_ROWS {
        return Err(format!(
            "batch-rows must not exceed {DEFAULT_BATCH_ROWS} to stay below parser limits"
        )
        .into());
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
