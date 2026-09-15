use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use unionid::backup::{self, incremental};
use unionid::{Engine, MutationProfile, QueryResponse, Value};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const REPORT_VERSION: u16 = 1;
const DEFAULT_SAMPLES: usize = 20;
const DEFAULT_BATCH_ROWS: usize = 1_000;
const WARMUPS: usize = 2;
const PAGE_ROWS: usize = 1_000;
const MAX_ROWS: usize = 100_000;
const MAX_SAMPLES: usize = 1_000;

#[derive(Debug, Serialize)]
struct EnvironmentReport {
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    rustc: String,
}

#[derive(Debug, Serialize)]
struct SampleReport {
    iterations: usize,
    samples_micros: Vec<u64>,
    p50_micros: u64,
    p95_micros: u64,
    journal_samples_micros: Vec<u64>,
    journal_p50_micros: u64,
    journal_p95_micros: u64,
    journal_samples_bytes: Vec<u64>,
    journal_p50_bytes: u64,
    journal_p95_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ArchiveSizeReport {
    baseline_stored_bytes: u64,
    baseline_expanded_bytes: u64,
    segment_count: usize,
    segment_stored_bytes: u64,
    segment_expanded_bytes: u64,
    archive_stored_bytes: u64,
    logical_before_bytes: u64,
    logical_after_bytes: u64,
    segment_to_logical_basis_points: u64,
    segment_smaller_than_logical: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct RestoreReport {
    sequence: u64,
    rows: usize,
    restore_micros: u64,
    check_micros: u64,
    compare_micros: u64,
    peak_rss_bytes: u64,
    schema_revision: u64,
    schema_hash: String,
    source_sequence: u64,
    restored_sequence: u64,
}

#[derive(Debug, Serialize)]
struct EvaluationReport {
    version: u16,
    environment: EnvironmentReport,
    rows: usize,
    batch_rows: usize,
    samples: usize,
    warmups: usize,
    work_directory: String,
    source_database_bytes: u64,
    prepare_millis: u128,
    logical_before_micros: u64,
    incremental_init_micros: u64,
    incremental_export_micros: u64,
    logical_after_micros: u64,
    control_writes: SampleReport,
    journal_writes: SampleReport,
    archive: ArchiveSizeReport,
    restore: RestoreReport,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("incremental backup evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, command, repo, source, target, sequence, rows] if command == "measure-restore" => {
            measure_restore(
                Path::new(repo),
                Path::new(source),
                Path::new(target),
                sequence.parse()?,
                rows.parse()?,
            )
        }
        [_, directory, rows] => evaluate(
            Path::new(directory),
            rows.parse()?,
            DEFAULT_SAMPLES,
            DEFAULT_BATCH_ROWS,
        ),
        [_, directory, rows, samples] => evaluate(
            Path::new(directory),
            rows.parse()?,
            samples.parse()?,
            DEFAULT_BATCH_ROWS,
        ),
        [_, directory, rows, samples, batch_rows] => evaluate(
            Path::new(directory),
            rows.parse()?,
            samples.parse()?,
            batch_rows.parse()?,
        ),
        _ => Err(
            "usage: unionid-incremental-backup-eval <new-directory> <rows> [samples] [batch-rows]"
                .into(),
        ),
    }
}

fn evaluate(directory: &Path, rows: usize, samples: usize, batch_rows: usize) -> AnyResult<()> {
    validate_sizes(rows, samples, batch_rows)?;
    if directory.exists() {
        return Err(format!(
            "evaluation directory already exists: {}",
            directory.display()
        )
        .into());
    }
    fs::create_dir_all(directory)?;
    let source = directory.join("source.redb");
    let repo = directory.join("archive");
    let logical_before = directory.join("logical-before.json");
    let logical_after = directory.join("logical-after.json");
    let restored = directory.join("restored.redb");

    let prepare_started = Instant::now();
    prepare(&source, rows, batch_rows)?;
    let prepare_millis = prepare_started.elapsed().as_millis();

    let mut engine = Engine::open_redb(&source)?;
    let control_writes = measure_writes(&mut engine, rows, samples, false)?;
    drop(engine);

    let started = Instant::now();
    backup::create(&source, &logical_before)?;
    let logical_before_micros = elapsed_micros(started);

    let started = Instant::now();
    incremental::init(&source, &repo, Default::default())?;
    let incremental_init_micros = elapsed_micros(started);

    let mut engine = Engine::open_redb(&source)?;
    let journal_writes = measure_writes(&mut engine, rows, samples, true)?;
    drop(engine);

    let started = Instant::now();
    let export = incremental::export(&source, &repo, Default::default())?;
    let incremental_export_micros = elapsed_micros(started);
    if export.exported_commits != u64::try_from(samples + WARMUPS)? || export.no_op {
        return Err(format!("unexpected incremental export report: {export:?}").into());
    }

    let started = Instant::now();
    backup::create(&source, &logical_after)?;
    let logical_after_micros = elapsed_micros(started);

    let listed = incremental::list(&repo)?;
    let verified = incremental::verify(&repo, Default::default())?;
    if listed.recoverable_last_sequence != verified.verified_last_sequence
        || !verified.orphan_files.is_empty()
        || listed.segments.is_empty()
    {
        return Err("incremental list and verify reports are inconsistent".into());
    }
    let segment_stored_bytes = listed
        .segments
        .iter()
        .map(|artifact| artifact.stored_bytes)
        .sum::<u64>();
    let segment_expanded_bytes = listed
        .segments
        .iter()
        .map(|artifact| artifact.expanded_bytes)
        .sum::<u64>();
    let logical_before_bytes = fs::metadata(&logical_before)?.len();
    let logical_after_bytes = fs::metadata(&logical_after)?.len();
    let segment_to_logical_basis_points = segment_stored_bytes
        .saturating_mul(10_000)
        .checked_div(logical_after_bytes)
        .unwrap_or(u64::MAX);
    let archive = ArchiveSizeReport {
        baseline_stored_bytes: listed.baseline.stored_bytes,
        baseline_expanded_bytes: listed.baseline.expanded_bytes,
        segment_count: listed.segments.len(),
        segment_stored_bytes,
        segment_expanded_bytes,
        archive_stored_bytes: listed.stored_bytes,
        logical_before_bytes,
        logical_after_bytes,
        segment_to_logical_basis_points,
        segment_smaller_than_logical: segment_stored_bytes < logical_after_bytes,
    };

    let restore = run_restore_child(
        &std::env::current_exe()?,
        &repo,
        &source,
        &restored,
        listed.recoverable_last_sequence,
        rows,
    )?;
    let report = EvaluationReport {
        version: REPORT_VERSION,
        environment: environment()?,
        rows,
        batch_rows,
        samples,
        warmups: WARMUPS,
        work_directory: directory.display().to_string(),
        source_database_bytes: fs::metadata(&source)?.len(),
        prepare_millis,
        logical_before_micros,
        incremental_init_micros,
        incremental_export_micros,
        logical_after_micros,
        control_writes,
        journal_writes,
        archive,
        restore,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn prepare(path: &Path, rows: usize, batch_rows: usize) -> AnyResult<()> {
    let mut engine = Engine::open_redb(path)?;
    require_ok(engine.execute(
        "type State =\n  Pending\n  | Running\n    worker text\n    attempt int\n  | Done\n    result text\ntype Task =\n  id int\n  title text\n  tags list text\n  state State\n  score int\ntable tasks Task\n  key id\ncreate index tasks (state)\ncreate index tasks (title)",
    ))?;
    for start in (0..rows).step_by(batch_rows) {
        let end = rows.min(start + batch_rows);
        let mut source = String::new();
        for id in start..end {
            source.push_str("insert tasks {id = ");
            source.push_str(&id.to_string());
            source.push_str(", title = \"task-");
            source.push_str(&format!("{id:06}"));
            source.push_str("\", tags = [\"backup\", \"benchmark\"], state = ");
            match id % 3 {
                0 => source.push_str("Pending"),
                1 => source.push_str(&format!(
                    "Running {{worker = \"worker-{}\", attempt = {}}}",
                    id % 16,
                    id % 8
                )),
                _ => source.push_str(&format!("Done {{result = \"result-{id:06}\"}}")),
            }
            source.push_str(", score = ");
            source.push_str(&(id % 100).to_string());
            source.push_str("}\n");
        }
        require_ok(engine.execute(&source))?;
    }
    require_count(&mut engine, rows)?;
    Ok(())
}

fn measure_writes(
    engine: &mut Engine,
    rows: usize,
    samples: usize,
    expect_journal: bool,
) -> AnyResult<SampleReport> {
    let total = samples + WARMUPS;
    let mut elapsed = Vec::with_capacity(samples);
    let mut journal_micros = Vec::with_capacity(samples);
    let mut journal_bytes = Vec::with_capacity(samples);
    for iteration in 0..total {
        let id = iteration % rows;
        let started = Instant::now();
        let response = engine.execute(&format!(
            "update tasks | filter id == {id} | set score = score + 1"
        ));
        let outer = elapsed_micros(started);
        require_ok(response)?;
        let profile = require_write_profile(engine, expect_journal)?;
        if iteration >= WARMUPS {
            elapsed.push(outer);
            journal_micros.push(
                profile
                    .durable
                    .expect("validated durable profile")
                    .journal_micros,
            );
            journal_bytes.push(
                profile
                    .durable
                    .expect("validated durable profile")
                    .journal_bytes,
            );
        }
    }
    Ok(sample_report(elapsed, journal_micros, journal_bytes))
}

fn require_write_profile(engine: &Engine, expect_journal: bool) -> AnyResult<MutationProfile> {
    let profile = engine
        .last_mutation_profile()
        .ok_or("mutation profile is missing")?;
    let durable = profile.durable.ok_or("durable commit profile is missing")?;
    if profile.full_rebuild
        || profile.row_updates != 1
        || durable.row_changes != 1
        || (durable.journal_bytes != 0) != expect_journal
    {
        return Err(format!("unexpected write profile: {profile:?}").into());
    }
    Ok(profile)
}

fn sample_report(
    samples_micros: Vec<u64>,
    journal_samples_micros: Vec<u64>,
    journal_samples_bytes: Vec<u64>,
) -> SampleReport {
    SampleReport {
        iterations: samples_micros.len(),
        p50_micros: percentile(&samples_micros, 50),
        p95_micros: percentile(&samples_micros, 95),
        journal_p50_micros: percentile(&journal_samples_micros, 50),
        journal_p95_micros: percentile(&journal_samples_micros, 95),
        journal_p50_bytes: percentile(&journal_samples_bytes, 50),
        journal_p95_bytes: percentile(&journal_samples_bytes, 95),
        samples_micros,
        journal_samples_micros,
        journal_samples_bytes,
    }
}

fn run_restore_child(
    executable: &Path,
    repo: &Path,
    source: &Path,
    target: &Path,
    sequence: u64,
    rows: usize,
) -> AnyResult<RestoreReport> {
    let output = Command::new(executable)
        .args([
            "measure-restore",
            path_text(repo)?,
            path_text(source)?,
            path_text(target)?,
            &sequence.to_string(),
            &rows.to_string(),
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "restore child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn measure_restore(
    repo: &Path,
    source_path: &Path,
    target: &Path,
    sequence: u64,
    rows: usize,
) -> AnyResult<()> {
    let started = Instant::now();
    let restored = incremental::restore(repo, target, sequence, Default::default())?;
    let restore_micros = elapsed_micros(started);
    if restored.restored_sequence != sequence || restored.row_count != u64::try_from(rows)? {
        return Err(format!("unexpected restore report: {restored:?}").into());
    }

    let mut target_engine = Engine::open_redb(target)?;
    let started = Instant::now();
    let integrity = target_engine.check_integrity()?;
    let check_micros = elapsed_micros(started);
    if !integrity.backend_clean || !integrity.profile.bounded {
        return Err("restored database did not pass a bounded full check".into());
    }
    let (restored_rows, restored_sequence) = require_count(&mut target_engine, rows)?;

    let mut source_engine = Engine::open_redb(source_path)?;
    let (source_rows, source_sequence) = require_count(&mut source_engine, rows)?;
    if source_rows != restored_rows || source_sequence != sequence || restored_sequence != sequence
    {
        return Err("source and restored sequence or row count differs".into());
    }
    let started = Instant::now();
    compare_rows(&mut source_engine, &mut target_engine, rows)?;
    let compare_micros = elapsed_micros(started);
    if source_engine.schema_info() != target_engine.schema_info() {
        return Err("source and restored schema identity differs".into());
    }
    let schema = target_engine.schema_info();
    let report = RestoreReport {
        sequence,
        rows,
        restore_micros,
        check_micros,
        compare_micros,
        peak_rss_bytes: peak_rss_bytes()?,
        schema_revision: schema.revision,
        schema_hash: schema.hash,
        source_sequence,
        restored_sequence,
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn compare_rows(source: &mut Engine, restored: &mut Engine, expected: usize) -> AnyResult<()> {
    let mut source_cursor = None;
    let mut restored_cursor = None;
    let mut compared = 0usize;
    loop {
        let source_page = require_page(source.execute(&page_source(source_cursor.as_deref())?))?;
        let restored_page =
            require_page(restored.execute(&page_source(restored_cursor.as_deref())?))?;
        if serde_json::to_vec(&source_page.rows)? != serde_json::to_vec(&restored_page.rows)? {
            return Err(format!("restored rows differ at offset {compared}").into());
        }
        compared = compared.saturating_add(source_page.rows.len());
        source_cursor = source_page.page.and_then(|page| page.next_cursor);
        restored_cursor = restored_page.page.and_then(|page| page.next_cursor);
        if source_cursor.is_none() || restored_cursor.is_none() {
            if source_cursor.is_some() != restored_cursor.is_some() {
                return Err("source and restored pagination ended at different points".into());
            }
            break;
        }
    }
    if compared != expected {
        return Err(format!("expected to compare {expected} rows, compared {compared}").into());
    }
    Ok(())
}

fn page_source(cursor: Option<&str>) -> AnyResult<String> {
    let mut source = format!("from tasks\nsort id\npage {PAGE_ROWS}");
    if let Some(cursor) = cursor {
        source.push_str(" after ");
        source.push_str(&serde_json::to_string(cursor)?);
    }
    Ok(source)
}

fn require_page(response: QueryResponse) -> AnyResult<QueryResponse> {
    if !response.ok {
        return Err(response.message.into());
    }
    if response.rows.len() > PAGE_ROWS || response.page.is_none() {
        return Err("paged query returned an invalid shape".into());
    }
    Ok(response)
}

fn require_count(engine: &mut Engine, expected: usize) -> AnyResult<(usize, u64)> {
    let response = engine.execute("from tasks\naggregate\n  rows = count");
    if !response.ok {
        return Err(response.message.into());
    }
    let count = response
        .rows
        .first()
        .and_then(|row| row.get("rows"))
        .ok_or("count query returned no rows value")?;
    let count = match count.unwrapped() {
        Value::Int(count) => usize::try_from(*count)?,
        other => return Err(format!("count query returned {}", other.source_text()).into()),
    };
    if count != expected {
        return Err(format!("expected {expected} rows, found {count}").into());
    }
    let sequence = engine.backup_journal_status()?.head_sequence;
    Ok((count, sequence))
}

fn require_ok(response: QueryResponse) -> AnyResult<()> {
    if !response.ok {
        return Err(response.message.into());
    }
    Ok(())
}

fn validate_sizes(rows: usize, samples: usize, batch_rows: usize) -> AnyResult<()> {
    if rows == 0 || rows > MAX_ROWS {
        return Err(format!("rows must be in 1..={MAX_ROWS}").into());
    }
    if samples == 0 || samples > MAX_SAMPLES {
        return Err(format!("samples must be in 1..={MAX_SAMPLES}").into());
    }
    if batch_rows == 0 || batch_rows > rows {
        return Err("batch rows must be in 1..=rows".into());
    }
    Ok(())
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = percent
        .saturating_mul(sorted.len())
        .saturating_add(99)
        .checked_div(100)
        .unwrap_or(1)
        .max(1);
    sorted[rank.saturating_sub(1).min(sorted.len().saturating_sub(1))]
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn path_text(path: &Path) -> AnyResult<&str> {
    path.to_str().ok_or_else(|| "path is not UTF-8".into())
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
    // SAFETY: getrusage initializes the supplied structure on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success, so the structure is initialized.
    let rss = unsafe { usage.assume_init() }.ru_maxrss;
    if cfg!(target_os = "macos") {
        Ok(u64::try_from(rss)?)
    } else {
        Ok(u64::try_from(rss)?.saturating_mul(1024))
    }
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> AnyResult<u64> {
    Ok(0)
}
