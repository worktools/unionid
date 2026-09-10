use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unionid::{
    Engine, IdempotencyPruneOptions, IdempotencyStatus, MigrationFile, MigrationMaintenancePhase,
    MigrationProfile, MigrationProgress, MigrationStatus, QueryAccessKind, SchemaInfo, Value,
    backup,
};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const MIN_ROWS: usize = 12;
const MAX_ROWS: usize = 1_000_000;
const MAX_ROUNDS: usize = 10_000;
const MAINTENANCE_BATCH_ROWS: usize = 1_024;
const VALIDATION_CHUNK_ROWS: usize = 10_000;

const SCHEMA: &str = r#"type Tree =
  Leaf text
  | Branch {
      label text
      children list Tree
    }
type Payload =
  Narrow {score int}
  | Wide {
      body text
      tags list text
    }
  | Deep {tree Tree}
type Entry =
  id int
  revision int
  payload Payload
table entries Entry
  key id
create index entries (revision)
create index entries (payload)"#;

const MIGRATIONS: [&str; 3] = [
    "migration churn_generation_1\n  add field Entry.maintenance_epoch int = 0\n  add index entries (maintenance_epoch)",
    "migration churn_generation_2\n  parent churn_generation_1\n  rename field Entry.maintenance_epoch to epoch",
    "migration churn_generation_3\n  parent churn_generation_2\n  add field Entry.audit option text = None\n  add index entries (audit)",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExpectedPayload {
    Narrow { score: i64 },
    Wide { body: String, tags: Vec<String> },
    Deep { root: String, leaf: String },
}

#[derive(Clone, Debug, Serialize)]
struct ExpectedEntry {
    id: i64,
    revision: i64,
    payload: ExpectedPayload,
}

#[derive(Debug, Serialize)]
struct RoundOperations {
    updated_id: i64,
    upserted_id: i64,
    replaced_id: i64,
    inserted_id: i64,
    idempotent_id: i64,
    pruned_receipts: usize,
}

#[derive(Debug, Serialize)]
struct RoundReport {
    round: usize,
    operations: RoundOperations,
    rows: usize,
    logical_row_bytes: usize,
    database_bytes: u64,
    database_growth_bytes: i128,
    logical_to_file_ratio: f64,
    receipt_status: IdempotencyStatus,
    open_micros: u64,
    check_micros: u64,
    backup_micros: u64,
    backup_bytes: u64,
    peak_rss_bytes: u64,
    schema_revision: u64,
    schema_hash: String,
    reference_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    migration: Option<MigrationRound>,
}

#[derive(Debug, Serialize)]
struct MigrationRound {
    id: String,
    ledger_count: usize,
    ledger_head: String,
    file_bytes_before: u64,
    file_bytes_after: u64,
    file_growth_bytes: i128,
    profile: MigrationProfile,
    bounded_checkpoints: Vec<MigrationCheckpoint>,
}

#[derive(Debug, Serialize)]
struct MigrationCheckpoint {
    point: &'static str,
    committed_steps: usize,
    complete: bool,
    phase: Option<MigrationMaintenancePhase>,
    source_rows_seen: u64,
    target_rows_written: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChildAdvance {
    progress: MigrationProgress,
    profile: Option<MigrationProfile>,
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
    initial_rows: usize,
    rounds: usize,
    seed: u64,
    preparation_batch_rows: usize,
    database_path: String,
    final_rows: usize,
    final_reference_digest: String,
    reports: Vec<RoundReport>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("churn evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--advance-child") {
        return run_advance_child(&args);
    }
    let [_, prefix, rows, rounds, seed, batch_rows] = args.as_slice() else {
        return Err(
            "usage: unionid-churn-eval <path-prefix> <rows> <rounds> <seed> <batch-rows>".into(),
        );
    };
    evaluate(
        Path::new(prefix),
        rows.parse()?,
        rounds.parse()?,
        seed.parse()?,
        batch_rows.parse()?,
    )
}

fn run_advance_child(args: &[String]) -> AnyResult<()> {
    let [_, _, database, migration_count, max_steps] = args else {
        return Err(
            "usage: unionid-churn-eval --advance-child <database> <migration-count> <max-steps>"
                .into(),
        );
    };
    let migration_count = migration_count.parse::<usize>()?;
    let max_steps = max_steps.parse::<usize>()?;
    let files = MIGRATIONS
        .into_iter()
        .take(migration_count)
        .map(MigrationFile::parse)
        .collect::<unionid::Result<Vec<_>>>()?;
    if files.len() != migration_count {
        return Err("advance child migration count exceeds the built-in chain".into());
    }
    let mut engine = Engine::open_redb(database)?;
    let progress = engine.advance_migrations(&files, max_steps)?;
    let output = ChildAdvance {
        progress,
        profile: engine.last_migration_profile(),
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn evaluate(
    prefix: &Path,
    rows: usize,
    rounds: usize,
    seed: u64,
    batch_rows: usize,
) -> AnyResult<()> {
    validate_sizes(rows, rounds, batch_rows)?;
    let database_path = suffixed(prefix, "redb");
    let backup_path = suffixed(prefix, "backup.json");
    remove_if_exists(&database_path)?;
    remove_if_exists(&backup_path)?;

    let mut reference = BTreeMap::new();
    let mut engine = Engine::open_redb(database_path.clone())?;
    require_ok(&engine.execute(SCHEMA), "create schema")?;
    prepare(&mut engine, &mut reference, rows, seed, batch_rows)?;
    validate_state(&mut engine, &reference)?;
    let migrations = MIGRATIONS
        .into_iter()
        .map(MigrationFile::parse)
        .collect::<unionid::Result<Vec<_>>>()?;

    let mut reports = Vec::with_capacity(rounds);
    let mut previous_database_bytes = fs::metadata(&database_path)?.len();
    for round in 0..rounds {
        let operations = mutate_round(&mut engine, &mut reference, round, rows, seed)?;
        let migration = if round < migrations.len() {
            Some(apply_migration_round(
                &mut engine,
                &database_path,
                &migrations[..=round],
                reference.len(),
            )?)
        } else {
            None
        };
        let validated = validate_state(&mut engine, &reference)?;
        let receipt_status = engine.idempotency_status()?;
        drop(engine);

        remove_if_exists(&backup_path)?;
        let backup_started = Instant::now();
        let backup_info = backup::create(&database_path, &backup_path)?;
        let backup_micros = elapsed_micros(backup_started);
        if backup_info.receipt_count != receipt_status.count {
            return Err(format!(
                "round {round}: backup retained {} receipts, expected {}",
                backup_info.receipt_count, receipt_status.count
            )
            .into());
        }

        let open_started = Instant::now();
        engine = Engine::open_redb(database_path.clone())?;
        let open_micros = elapsed_micros(open_started);
        let check_started = Instant::now();
        engine.check_integrity()?;
        let check_micros = elapsed_micros(check_started);
        let reopened = validate_state(&mut engine, &reference)?;
        if reopened.reference_digest != validated.reference_digest {
            return Err(format!("round {round}: reopen changed the logical state").into());
        }
        let reopened_receipts = engine.idempotency_status()?;
        if reopened_receipts != receipt_status {
            return Err(format!("round {round}: reopen changed receipt boundaries").into());
        }
        let expected_migrations = migrations.len().min(round + 1);
        if expected_migrations > 0 {
            let prefix = &migrations[..expected_migrations];
            let status = engine.migration_status(prefix)?;
            let schema = SchemaInfo {
                revision: reopened.schema_revision,
                hash: reopened.schema_hash.clone(),
            };
            validate_migration_state(&mut engine, prefix, &status, &schema, reference.len())?;
        }

        let database_bytes = fs::metadata(&database_path)?.len();
        reports.push(RoundReport {
            round,
            operations,
            rows: reference.len(),
            logical_row_bytes: reopened.logical_row_bytes,
            database_bytes,
            database_growth_bytes: i128::from(database_bytes) - i128::from(previous_database_bytes),
            logical_to_file_ratio: reopened.logical_row_bytes as f64 / database_bytes as f64,
            receipt_status,
            open_micros,
            check_micros,
            backup_micros,
            backup_bytes: fs::metadata(&backup_path)?.len(),
            peak_rss_bytes: peak_rss_bytes()?,
            schema_revision: reopened.schema_revision,
            schema_hash: reopened.schema_hash,
            reference_digest: reopened.reference_digest,
            migration,
        });
        previous_database_bytes = database_bytes;
    }

    let final_reference_digest = reference_digest(&reference)?;
    let report = EvaluationReport {
        environment: environment()?,
        initial_rows: rows,
        rounds,
        seed,
        preparation_batch_rows: batch_rows,
        database_path: database_path.display().to_string(),
        final_rows: reference.len(),
        final_reference_digest,
        reports,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn apply_migration_round(
    engine: &mut Engine,
    database_path: &Path,
    files: &[MigrationFile],
    expected_rows: usize,
) -> AnyResult<MigrationRound> {
    let file = files.last().ok_or("migration prefix is empty")?;
    let plan = engine.plan_migrations(files)?;
    if plan.applied_count + 1 != files.len()
        || plan.pending.len() != 1
        || plan.pending[0].id != file.id
    {
        return Err(format!(
            "migration {} did not produce one exact pending plan",
            file.id
        )
        .into());
    }
    let expected_schema = plan.target_schema.clone();
    let file_bytes_before = fs::metadata(database_path)?.len();
    if files.len() == 1 {
        return apply_interrupted_first_migration(
            engine,
            database_path,
            files,
            expected_rows,
            expected_schema,
            file_bytes_before,
        );
    }
    let applied = engine.apply_migrations(files)?;
    if applied.applied != [file.id.clone()]
        || !applied
            .skipped
            .iter()
            .eq(files[..files.len() - 1].iter().map(|file| &file.id))
    {
        return Err(format!("migration {} apply result differs from its plan", file.id).into());
    }
    let profile = engine
        .last_migration_profile()
        .ok_or("shadow migration has no phase profile")?;
    if !profile.reclaim_complete
        || profile.target_generation <= profile.source_generation
        || profile.source_rows_seen != u64::try_from(expected_rows)?
        || profile.target_rows_written != u64::try_from(expected_rows)?
    {
        return Err(format!(
            "migration {} did not complete generation reclamation",
            file.id
        )
        .into());
    }
    let status = engine.migration_status(files)?;
    validate_migration_state(engine, files, &status, &expected_schema, expected_rows)?;
    let file_bytes_after = fs::metadata(database_path)?.len();
    Ok(MigrationRound {
        id: file.id.clone(),
        ledger_count: status.applied.len(),
        ledger_head: file.id.clone(),
        file_bytes_before,
        file_bytes_after,
        file_growth_bytes: i128::from(file_bytes_after) - i128::from(file_bytes_before),
        profile,
        bounded_checkpoints: Vec::new(),
    })
}

fn apply_interrupted_first_migration(
    engine: &mut Engine,
    database_path: &Path,
    files: &[MigrationFile],
    expected_rows: usize,
    expected_schema: SchemaInfo,
    file_bytes_before: u64,
) -> AnyResult<MigrationRound> {
    let file = files.last().ok_or("migration prefix is empty")?;
    let total_started = Instant::now();
    close_engine(engine);
    let build = run_child_advance(database_path, files.len(), 2)?;
    let building = build
        .progress
        .status
        .maintenance
        .as_ref()
        .ok_or("build interruption did not retain maintenance")?;
    if build.progress.committed_steps != 2
        || build.progress.complete
        || building.phase != MigrationMaintenancePhase::Building
        || building.source_rows_seen != u64::try_from(expected_rows.min(MAINTENANCE_BATCH_ROWS))?
    {
        return Err("build child did not stop at the deterministic checkpoint".into());
    }
    let mut checkpoints = vec![migration_checkpoint("build_exit", &build.progress)];

    *engine = Engine::open_redb(database_path)?;
    let reopened_build = engine.migration_status(files)?;
    if reopened_build.maintenance.as_ref() != Some(building) {
        return Err("reopen changed the build maintenance checkpoint".into());
    }
    close_engine(engine);

    let source_batches = expected_rows.div_ceil(MAINTENANCE_BATCH_ROWS);
    let cutover_steps = source_batches.saturating_sub(1).saturating_add(2);
    let cutover = run_child_advance(database_path, files.len(), cutover_steps)?;
    let reclaimable = cutover
        .progress
        .status
        .maintenance
        .as_ref()
        .ok_or("cutover interruption did not retain maintenance")?;
    if cutover.progress.committed_steps != cutover_steps
        || cutover.progress.complete
        || cutover.progress.applied != [file.id.clone()]
        || reclaimable.phase != MigrationMaintenancePhase::Reclaimable
        || reclaimable.source_rows_seen != u64::try_from(expected_rows)?
        || reclaimable.target_rows_written != u64::try_from(expected_rows)?
    {
        return Err("cutover child did not stop before deterministic reclamation".into());
    }
    checkpoints.push(migration_checkpoint("cutover_exit", &cutover.progress));

    *engine = Engine::open_redb(database_path)?;
    let reopened_cutover = engine.migration_status(files)?;
    if reopened_cutover.maintenance.as_ref() != Some(reclaimable)
        || reopened_cutover.applied.len() != 1
        || !reopened_cutover.pending.is_empty()
    {
        return Err("reopen changed the cutover checkpoint or ledger".into());
    }
    close_engine(engine);

    let reclaim_started = Instant::now();
    let reclaim = run_child_advance(database_path, files.len(), 1)?;
    if reclaim.progress.committed_steps != 1 {
        return Err("reclaim child did not commit exactly one bounded step".into());
    }
    if expected_rows > MAINTENANCE_BATCH_ROWS
        && (reclaim.progress.complete
            || reclaim
                .progress
                .status
                .maintenance
                .as_ref()
                .is_none_or(|maintenance| {
                    maintenance.phase != MigrationMaintenancePhase::Reclaimable
                }))
    {
        return Err("reclaim child did not leave a resumable cleanup checkpoint".into());
    }
    checkpoints.push(migration_checkpoint("reclaim_exit", &reclaim.progress));

    *engine = Engine::open_redb(database_path)?;
    let resumed = engine.apply_migrations(files)?;
    if !resumed.applied.is_empty() || resumed.skipped != [file.id.clone()] {
        return Err("post-cutover apply duplicated the migration ledger entry".into());
    }
    let status = engine.migration_status(files)?;
    validate_migration_state(engine, files, &status, &expected_schema, expected_rows)?;
    let file_bytes_after = fs::metadata(database_path)?.len();
    let mut profile = cutover
        .profile
        .ok_or("cutover child did not report a migration profile")?;
    profile.total_micros = elapsed_micros(total_started);
    profile.reclaim_micros = elapsed_micros(reclaim_started);
    profile.reclaim_complete = true;
    if profile.source_rows_seen != u64::try_from(expected_rows)?
        || profile.target_rows_written != u64::try_from(expected_rows)?
    {
        return Err("bounded migration profile has incorrect row counters".into());
    }
    Ok(MigrationRound {
        id: file.id.clone(),
        ledger_count: status.applied.len(),
        ledger_head: file.id.clone(),
        file_bytes_before,
        file_bytes_after,
        file_growth_bytes: i128::from(file_bytes_after) - i128::from(file_bytes_before),
        profile,
        bounded_checkpoints: checkpoints,
    })
}

fn close_engine(engine: &mut Engine) {
    drop(std::mem::replace(engine, Engine::memory()));
}

fn run_child_advance(
    database_path: &Path,
    migration_count: usize,
    max_steps: usize,
) -> AnyResult<ChildAdvance> {
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--advance-child",
            database_path
                .to_str()
                .ok_or("database path is not valid UTF-8")?,
            &migration_count.to_string(),
            &max_steps.to_string(),
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "bounded migration child failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn migration_checkpoint(point: &'static str, progress: &MigrationProgress) -> MigrationCheckpoint {
    let maintenance = progress.status.maintenance.as_ref();
    MigrationCheckpoint {
        point,
        committed_steps: progress.committed_steps,
        complete: progress.complete,
        phase: maintenance.map(|maintenance| maintenance.phase.clone()),
        source_rows_seen: maintenance.map_or(0, |maintenance| maintenance.source_rows_seen),
        target_rows_written: maintenance.map_or(0, |maintenance| maintenance.target_rows_written),
    }
}

fn validate_migration_state(
    engine: &mut Engine,
    files: &[MigrationFile],
    status: &MigrationStatus,
    expected_schema: &SchemaInfo,
    expected_rows: usize,
) -> AnyResult<()> {
    if status.maintenance.is_some()
        || status.schema != *expected_schema
        || status.applied.len() != files.len()
        || !status.pending.is_empty()
    {
        return Err("migration status has unexpected maintenance, schema, or cardinality".into());
    }
    for (position, (entry, file)) in status.applied.iter().zip(files).enumerate() {
        if entry.id != file.id
            || entry.parent != file.parent
            || entry.checksum != file.checksum
            || entry.schema_revision != u64::try_from(position + 2)?
            || entry.applied_at_unix_ms == 0
        {
            return Err(format!("migration ledger entry {} differs from its file", file.id).into());
        }
    }
    let Some(last) = status.applied.last() else {
        return Err("migration ledger is empty".into());
    };
    if last.schema_revision != expected_schema.revision || last.schema_hash != expected_schema.hash
    {
        return Err("migration ledger head does not match schema identity".into());
    }
    let introspection = engine.introspection();
    if introspection.schema != *expected_schema
        || introspection.migration_count != files.len()
        || introspection.migration_head.as_deref() != Some(last.id.as_str())
        || introspection.maintenance.is_some()
    {
        return Err("migration introspection differs from the ledger".into());
    }

    let epoch_field = if files.len() == 1 {
        "maintenance_epoch"
    } else {
        "epoch"
    };
    validate_default_index(engine, epoch_field, "0", expected_rows)?;
    if files.len() >= 2 {
        let old = engine.execute("from entries\nselect {maintenance_epoch}");
        if old.ok || old.error.as_ref().map(|error| error.code.as_str()) != Some("E_FIELD") {
            return Err("renamed maintenance_epoch field is still queryable".into());
        }
    }
    if files.len() >= 3 {
        validate_default_index(engine, "audit", "None", expected_rows)?;
    }
    Ok(())
}

fn validate_default_index(
    engine: &mut Engine,
    field: &str,
    value: &str,
    expected_rows: usize,
) -> AnyResult<()> {
    let condition = format!("{field} == {value}");
    let explained = engine.execute(&format!("explain from entries\nfilter {condition}"));
    require_ok(&explained, "explain migrated default index")?;
    let plan = explained.plan.ok_or("migrated field explain has no plan")?;
    if plan.access.kind != QueryAccessKind::SecondaryIndexLookup
        || plan.access.index.as_deref() != Some(format!("entries.{field}").as_str())
    {
        return Err(format!("migrated field {field} did not retain its secondary index").into());
    }
    let rows = engine.execute(&format!(
        "from entries\nfilter {condition}\naggregate\n  rows = count"
    ));
    require_ok(&rows, "query migrated default index")?;
    let count = rows
        .rows
        .first()
        .ok_or("migrated default query returned no aggregate")?;
    if value_int(count.get("rows"), "rows")? != i64::try_from(expected_rows)? {
        return Err(format!("migrated default {field} does not cover every row").into());
    }
    Ok(())
}

fn prepare(
    engine: &mut Engine,
    reference: &mut BTreeMap<i64, ExpectedEntry>,
    rows: usize,
    seed: u64,
    batch_rows: usize,
) -> AnyResult<()> {
    for start in (0..rows).step_by(batch_rows) {
        let end = rows.min(start + batch_rows);
        let entries = (start..end)
            .map(|id| Ok(expected_entry(i64::try_from(id)?, 0, seed)))
            .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;
        let source = insert_many_source(&entries);
        require_ok(
            &engine.execute(&source),
            &format!("prepare rows {start}..{end}"),
        )?;
        for entry in entries {
            reference.insert(entry.id, entry);
        }
    }
    Ok(())
}

fn mutate_round(
    engine: &mut Engine,
    reference: &mut BTreeMap<i64, ExpectedEntry>,
    round: usize,
    initial_rows: usize,
    seed: u64,
) -> AnyResult<RoundOperations> {
    let keys = reference.keys().copied().collect::<Vec<_>>();
    let updated_id = keys[pick(seed, round, 1, keys.len())];
    let upserted_id = keys[pick(seed, round, 2, keys.len())];
    let replaced_id = keys[pick(seed, round, 3, keys.len())];
    let idempotent_id = keys[pick(seed, round, 4, keys.len())];
    let inserted_id = i64::try_from(initial_rows.checked_add(round).ok_or("row id overflow")?)?;

    let updated_revision = reference[&updated_id].revision + 1;
    require_affected(
        &engine.execute(&format!(
            "update entries\nfilter id == {updated_id}\nset revision = {updated_revision}"
        )),
        1,
        "update",
    )?;
    reference.get_mut(&updated_id).unwrap().revision = updated_revision;

    let upsert_revision = i64::try_from(round)? + 100;
    let upsert = expected_entry(upserted_id, upsert_revision, seed ^ 0x51a7);
    require_affected(&engine.execute(&upsert_source(&upsert)), 1, "upsert")?;
    reference.insert(upserted_id, upsert);

    require_affected(
        &engine.execute(&format!("delete entries\nfilter id == {replaced_id}")),
        1,
        "delete",
    )?;
    reference.remove(&replaced_id);
    let replaced = expected_entry(replaced_id, i64::try_from(round)? + 200, seed ^ 0xa22e);
    let inserted = expected_entry(inserted_id, i64::try_from(round)? + 300, seed ^ 0xc33f);
    require_affected(
        &engine.execute(&insert_many_source(&[replaced.clone(), inserted.clone()])),
        2,
        "insert many",
    )?;
    reference.insert(replaced_id, replaced);
    reference.insert(inserted_id, inserted);

    let idempotent_revision = reference[&idempotent_id].revision + 1;
    let idempotent_source = format!(
        "update entries\nfilter id == {idempotent_id}\nset revision = {idempotent_revision}"
    );
    let key = format!("churn-round-{round}");
    let digest = format!("sha256:{:064x}", seed ^ u64::try_from(round)?);
    let first = engine.execute_idempotent_with_params(
        &key,
        &digest,
        &idempotent_source,
        BTreeMap::new(),
        None,
    )?;
    if first.replayed || first.response.affected_rows != Some(1) {
        return Err(
            format!("round {round}: first idempotent mutation was not committed once").into(),
        );
    }
    let replay = engine.execute_idempotent_with_params(
        &key,
        &digest,
        "this replay must not be parsed",
        BTreeMap::new(),
        None,
    )?;
    if !replay.replayed || replay.committed_sequence != first.committed_sequence {
        return Err(format!("round {round}: idempotent replay did not return its receipt").into());
    }
    reference.get_mut(&idempotent_id).unwrap().revision = idempotent_revision;

    let mut pruned_receipts = 0;
    let status = engine.idempotency_status()?;
    if status.count > 1 && round % 2 == 1 {
        let through = status
            .oldest
            .as_ref()
            .ok_or("receipt status has no oldest boundary")?
            .committed_sequence;
        let options = IdempotencyPruneOptions {
            completed_before_unix_ms: None,
            committed_through_sequence: Some(through),
            max_receipts: 1,
        };
        let preview = engine.plan_idempotency_prune(options.clone())?;
        if preview.applied || preview.selected_count != 1 {
            return Err(format!("round {round}: receipt prune preview is not exact").into());
        }
        let applied = engine.prune_idempotency_receipts(options)?;
        if !applied.applied || applied.selected_count != preview.selected_count {
            return Err(format!("round {round}: receipt prune differed from preview").into());
        }
        pruned_receipts = applied.selected_count;
    }

    Ok(RoundOperations {
        updated_id,
        upserted_id,
        replaced_id,
        inserted_id,
        idempotent_id,
        pruned_receipts,
    })
}

struct ValidatedState {
    logical_row_bytes: usize,
    schema_revision: u64,
    schema_hash: String,
    reference_digest: String,
}

fn validate_state(
    engine: &mut Engine,
    reference: &BTreeMap<i64, ExpectedEntry>,
) -> AnyResult<ValidatedState> {
    let mut expected_rows = reference.values();
    let mut row_count = 0usize;
    // This is the exact compact JSON array size without retaining a second full row set.
    let mut logical_row_bytes = 2usize;
    let mut result_schema: Option<SchemaInfo> = None;
    let upper_bounds = reference
        .keys()
        .copied()
        .skip(VALIDATION_CHUNK_ROWS)
        .step_by(VALIDATION_CHUNK_ROWS)
        .collect::<Vec<_>>();
    let mut lower_bound = None;
    for upper_bound in upper_bounds.into_iter().map(Some).chain([None]) {
        let filter = match (lower_bound, upper_bound) {
            (None, None) => String::new(),
            (None, Some(upper)) => format!("\nfilter id < {upper}"),
            (Some(lower), None) => format!("\nfilter id >= {lower}"),
            (Some(lower), Some(upper)) => {
                format!("\nfilter id >= {lower} and id < {upper}")
            }
        };
        let response = engine.execute(&format!("from entries{filter}\nsort id"));
        require_ok(&response, "read entry validation chunk")?;
        let schema = response
            .schema
            .as_ref()
            .ok_or("validation query response has no schema identity")?;
        if let Some(first_schema) = &result_schema {
            if first_schema != schema {
                return Err("schema identity changed during chunked validation".into());
            }
        } else {
            result_schema = Some(schema.clone());
        }
        for row in &response.rows {
            let expected = expected_rows
                .next()
                .ok_or("validation query returned more rows than the reference model")?;
            validate_row(row, expected)?;
            if row_count > 0 {
                logical_row_bytes += 1;
            }
            logical_row_bytes += serde_json::to_vec(row)?.len();
            row_count += 1;
        }
        lower_bound = upper_bound;
    }
    if row_count != reference.len() || expected_rows.next().is_some() {
        return Err(format!("expected {} rows, received {row_count}", reference.len()).into());
    }

    let mut revisions = BTreeMap::<i64, BTreeSet<i64>>::new();
    for entry in reference.values() {
        revisions
            .entry(entry.revision)
            .or_default()
            .insert(entry.id);
    }
    for (revision, expected_ids) in revisions.iter().take(3) {
        let indexed = engine.execute(&format!(
            "from entries\nfilter revision == {revision}\nsort id\nselect {{id}}"
        ));
        require_ok(&indexed, "indexed revision query")?;
        let actual = indexed
            .rows
            .iter()
            .map(|row| value_int(row.get("id"), "id"))
            .collect::<AnyResult<BTreeSet<_>>>()?;
        if &actual != expected_ids {
            return Err(format!("revision index mismatch for {revision}").into());
        }
    }
    let probe = reference
        .values()
        .next()
        .ok_or("reference model is empty")?;
    let expected_ids = reference
        .values()
        .filter(|entry| entry.payload == probe.payload)
        .map(|entry| entry.id)
        .collect::<BTreeSet<_>>();
    let condition = format!("payload == {}", payload_source(&probe.payload));
    let explained = engine.execute(&format!("explain from entries\nfilter {condition}"));
    require_ok(&explained, "explain ADT index query")?;
    let plan = explained.plan.ok_or("ADT index explain has no plan")?;
    if plan.access.kind != QueryAccessKind::SecondaryIndexLookup
        || plan.access.index.as_deref() != Some("entries.payload")
    {
        return Err("ADT equality query did not select entries.payload".into());
    }
    let indexed = engine.execute(&format!(
        "from entries\nfilter {condition}\nsort id\nselect {{id}}"
    ));
    require_ok(&indexed, "ADT index query")?;
    let actual = indexed
        .rows
        .iter()
        .map(|row| value_int(row.get("id"), "id"))
        .collect::<AnyResult<BTreeSet<_>>>()?;
    if actual != expected_ids {
        return Err("ADT equality index differs from the reference model".into());
    }
    let schema = result_schema.ok_or("validation query returned no schema identity")?;
    Ok(ValidatedState {
        logical_row_bytes,
        schema_revision: schema.revision,
        schema_hash: schema.hash,
        reference_digest: reference_digest(reference)?,
    })
}

fn validate_row(row: &BTreeMap<String, Value>, expected: &ExpectedEntry) -> AnyResult<()> {
    let id = value_int(row.get("id"), "id")?;
    let revision = value_int(row.get("revision"), "revision")?;
    if id != expected.id || revision != expected.revision {
        return Err(format!(
            "row {} header mismatch: expected revision {}, received id {id} revision {revision}",
            expected.id, expected.revision
        )
        .into());
    }
    let actual = decode_payload(row.get("payload").ok_or("row has no payload")?)?;
    if serde_json::to_vec(&actual)? != serde_json::to_vec(&expected.payload)? {
        return Err(format!("row {id} payload differs from the reference model").into());
    }
    Ok(())
}

fn decode_payload(value: &Value) -> AnyResult<ExpectedPayload> {
    let Value::Enum(value) = value.unwrapped() else {
        return Err("payload is not a sum value".into());
    };
    let record = value
        .args
        .first()
        .map(Value::unwrapped)
        .and_then(|value| match value {
            Value::Record(fields) => Some(fields),
            _ => None,
        })
        .ok_or("payload variant has no record fields")?;
    match value.variant.as_str() {
        "Narrow" => Ok(ExpectedPayload::Narrow {
            score: value_int(record.get("score"), "payload.score")?,
        }),
        "Wide" => Ok(ExpectedPayload::Wide {
            body: value_text(record.get("body"), "payload.body")?,
            tags: value_list_text(record.get("tags"), "payload.tags")?,
        }),
        "Deep" => {
            let (root, leaf) = decode_tree(record.get("tree").ok_or("payload.tree is missing")?)?;
            Ok(ExpectedPayload::Deep { root, leaf })
        }
        variant => Err(format!("unknown payload variant {variant}").into()),
    }
}

fn decode_tree(value: &Value) -> AnyResult<(String, String)> {
    let Value::Enum(tree) = value.unwrapped() else {
        return Err("tree is not a sum value".into());
    };
    if tree.variant != "Branch" {
        return Err("deep payload root is not Branch".into());
    }
    let Value::Record(fields) = tree
        .args
        .first()
        .map(Value::unwrapped)
        .ok_or("empty Branch")?
    else {
        return Err("Branch has no record payload".into());
    };
    let root = value_text(fields.get("label"), "tree.label")?;
    let Value::List(children) = fields
        .get("children")
        .map(Value::unwrapped)
        .ok_or("tree.children is missing")?
    else {
        return Err("tree.children is not a list".into());
    };
    let Value::Enum(leaf) = children
        .first()
        .map(Value::unwrapped)
        .ok_or("Branch has no leaf")?
    else {
        return Err("first child is not a tree value".into());
    };
    if leaf.variant != "Leaf" {
        return Err("first child is not Leaf".into());
    }
    let leaf = value_text(leaf.args.first(), "tree leaf")?;
    Ok((root, leaf))
}

fn expected_entry(id: i64, revision: i64, seed: u64) -> ExpectedEntry {
    let token = seed ^ (id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ revision as u64;
    let payload = match token % 3 {
        0 => ExpectedPayload::Narrow {
            score: i64::try_from(token % 10_000).unwrap(),
        },
        1 => ExpectedPayload::Wide {
            body: format!("wide-{token:016x}-{}", "x".repeat(192)),
            tags: vec!["wide".into(), format!("seed-{:04x}", seed & 0xffff)],
        },
        _ => ExpectedPayload::Deep {
            root: format!("root-{token:016x}"),
            leaf: format!("leaf-{:016x}", token.rotate_left(17)),
        },
    };
    ExpectedEntry {
        id,
        revision,
        payload,
    }
}

fn entry_source(entry: &ExpectedEntry) -> String {
    format!(
        "{{id = {}, revision = {}, payload = {}}}",
        entry.id,
        entry.revision,
        payload_source(&entry.payload)
    )
}

fn payload_source(payload: &ExpectedPayload) -> String {
    match payload {
        ExpectedPayload::Narrow { score } => format!("Narrow {{score = {score}}}"),
        ExpectedPayload::Wide { body, tags } => format!(
            "Wide {{body = {}, tags = [{}]}}",
            quoted(body),
            tags.iter()
                .map(|tag| quoted(tag))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ExpectedPayload::Deep { root, leaf } => format!(
            "Deep {{tree = Branch {{label = {}, children = [Leaf {}]}}}}",
            quoted(root),
            quoted(leaf)
        ),
    }
}

fn insert_many_source(entries: &[ExpectedEntry]) -> String {
    format!(
        "insert many entries [{}]",
        entries
            .iter()
            .map(entry_source)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn upsert_source(entry: &ExpectedEntry) -> String {
    format!("upsert entries {}", entry_source(entry))
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

fn value_int(value: Option<&Value>, path: &str) -> AnyResult<i64> {
    match value.map(Value::unwrapped) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(format!("{path} is not int").into()),
    }
}

fn value_text(value: Option<&Value>, path: &str) -> AnyResult<String> {
    match value.map(Value::unwrapped) {
        Some(Value::Text(value)) => Ok(value.clone()),
        _ => Err(format!("{path} is not text").into()),
    }
}

fn value_list_text(value: Option<&Value>, path: &str) -> AnyResult<Vec<String>> {
    let Some(Value::List(values)) = value.map(Value::unwrapped) else {
        return Err(format!("{path} is not a list").into());
    };
    values
        .iter()
        .map(|value| value_text(Some(value), path))
        .collect()
}

fn require_ok(response: &unionid::QueryResponse, operation: &str) -> AnyResult<()> {
    if response.ok {
        Ok(())
    } else {
        Err(format!("{operation}: {}", response.message).into())
    }
}

fn require_affected(
    response: &unionid::QueryResponse,
    expected: usize,
    operation: &str,
) -> AnyResult<()> {
    require_ok(response, operation)?;
    if response.affected_rows == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "{operation}: affected {:?} rows, expected {expected}",
            response.affected_rows
        )
        .into())
    }
}

fn reference_digest(reference: &BTreeMap<i64, ExpectedEntry>) -> AnyResult<String> {
    let bytes = serde_json::to_vec(reference)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn pick(seed: u64, round: usize, lane: u64, len: usize) -> usize {
    let value = seed
        .wrapping_add((round as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_add(lane.wrapping_mul(0xbf58_476d_1ce4_e5b9));
    usize::try_from(value % len as u64).unwrap()
}

fn validate_sizes(rows: usize, rounds: usize, batch_rows: usize) -> AnyResult<()> {
    if !(MIN_ROWS..=MAX_ROWS).contains(&rows) {
        return Err(format!("rows must be between {MIN_ROWS} and {MAX_ROWS}").into());
    }
    if rounds == 0 || rounds > MAX_ROUNDS {
        return Err(format!("rounds must be between 1 and {MAX_ROUNDS}").into());
    }
    if batch_rows == 0 || batch_rows > rows {
        return Err("batch-rows must be between 1 and rows".into());
    }
    Ok(())
}

fn suffixed(prefix: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{}", prefix.display(), suffix))
}

fn remove_if_exists(path: &Path) -> AnyResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn environment() -> AnyResult<EnvironmentReport> {
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()?;
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
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
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
