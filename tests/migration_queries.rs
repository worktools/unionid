mod common;

use common::TempDir;
use std::collections::BTreeMap;
use unionid::migration::MigrationFile;
use unionid::migration::query_validation::{
    MAX_QUERY_FILES, MAX_QUERY_REPORT_BYTES, MigrationQuery,
};
use unionid::portable::CompatibilityLevel;
use unionid::{Engine, ProtocolRequest, backup};

fn initial() -> MigrationFile {
    MigrationFile::parse("migration initial\n  add type State = Pending | Done\n  add type Job =\n    id int\n    state State\n    score int\n  add table jobs Job key id\n  add index jobs.state\n").unwrap()
}
fn next(name: &str, parent: &str, step: &str) -> MigrationFile {
    MigrationFile::parse(format!("migration {name}\n  parent {parent}\n  {step}\n")).unwrap()
}
fn query(path: &str, source: &str) -> MigrationQuery {
    MigrationQuery {
        path: path.into(),
        source: source.into(),
    }
}
fn exhaustive() -> MigrationQuery {
    query(
        "states/check.unid",
        "from jobs\nderive done = match state {\n Pending => false\n Done => true\n}\nselect {id, done}",
    )
}
fn setup(engine: &mut Engine) {
    engine.apply_migrations(&[initial()]).unwrap();
    let r = engine.execute("insert jobs {id: 1, state: Pending, score: 5}");
    assert!(r.ok, "{}", r.message);
}

#[test]
fn invalid_final_match_rejects_before_any_write_in_memory_and_redb() {
    let tmp = TempDir::new();
    for mut engine in [
        Engine::memory(),
        Engine::open_redb(tmp.0.join("jobs.redb")).unwrap(),
    ] {
        setup(&mut engine);
        let before = engine.execute("from jobs");
        let files = [
            initial(),
            next("cancel", "initial", "add variant State.Cancelled"),
        ];
        let queries = [
            query("z/check.unid", "from jobs | select {id}"),
            exhaustive(),
        ];
        let plan = engine
            .plan_migrations_with_queries(&files, &queries)
            .unwrap();
        let report = &plan.query_validation;
        assert!(!report.valid);
        assert_eq!(report.checked_files, 2);
        assert_eq!(report.files[0].path, "states/check.unid");
        assert!(report.files[0].current_valid);
        assert_eq!(
            report.files[0].failures[0].migration_id.as_deref(),
            Some("cancel")
        );
        assert_eq!(report.files[0].failures[0].error.code, "E_MATCH");
        assert!(report.files[0].failures[0].error.span.is_some());
        assert!(report.files[1].valid);
        let failure = engine
            .apply_migrations_with_queries(&files, &queries)
            .unwrap_err();
        assert_eq!(failure.error.code, "E_MIGRATION");
        assert_eq!(failure.query_validation.as_deref(), Some(report));
        assert_eq!(engine.schema_info(), plan.plan.current_schema);
        assert_eq!(engine.migration_history().len(), 1);
        assert_eq!(
            serde_json::to_value(engine.execute("from jobs").rows).unwrap(),
            serde_json::to_value(before.rows).unwrap()
        );
    }
    let mut reopened = Engine::open_redb(tmp.0.join("jobs.redb")).unwrap();
    assert_eq!(reopened.migration_history().len(), 1);
    reopened.check_integrity().unwrap();
}

#[test]
fn nested_nominal_shapes_and_parameters_require_review_but_unrelated_projection_does_not() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let files = [
        initial(),
        next("cancel", "initial", "add variant State.Cancelled"),
    ];
    let queries = [
        query(
            "state.unid",
            "from jobs | filter state == $state | select {state}",
        ),
        query("id.unid", "from jobs | select {id}"),
    ];
    let report = engine
        .plan_migrations_with_queries(&files, &queries)
        .unwrap()
        .query_validation;
    assert!(report.valid);
    assert_eq!(
        report.files[0].compatibility,
        CompatibilityLevel::Compatible
    );
    assert_eq!(report.files[1].parameters_changed, Some(true));
    assert_eq!(report.files[1].result_changed, Some(true));
    assert_eq!(
        report.files[1].compatibility,
        CompatibilityLevel::Conditional
    );
    let applied = engine
        .apply_migrations_with_queries(&files, &queries)
        .unwrap();
    assert_eq!(applied.applied.applied, ["cancel"]);
    assert_eq!(applied.query_validation, report);
}

#[test]
fn all_files_report_field_and_type_failures_without_executing_mutations() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let files = [
        initial(),
        next(
            "change",
            "initial",
            "rename field Job.score to points\n  change field Job.id to text\n    using old -> \"one\"",
        ),
    ];
    let queries = [
        query("fields.unid", "from jobs | select {score}"),
        query("types.unid", "from jobs | filter id > 0"),
        query("mutation.unid", "delete jobs\nexpect affected == 1"),
    ];
    let report = engine
        .plan_migrations_with_queries(&files, &queries)
        .unwrap()
        .query_validation;
    assert!(!report.valid);
    assert!(!report.files[0].valid);
    assert!(report.files[1].valid);
    assert!(!report.files[2].valid);
    assert_eq!(engine.execute("from jobs").rows.len(), 1);
}

#[test]
fn new_queries_and_repaired_intermediate_failures_use_the_final_target() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let files = [
        initial(),
        next("away", "initial", "rename field Job.score to temporary"),
        next(
            "back",
            "away",
            "rename field Job.temporary to score\n  add field Job.note: text = \"\"",
        ),
    ];
    let queries = [
        query("old.unid", "from jobs | select {score}"),
        query("new.unid", "from jobs | select {note}"),
    ];
    let plan = engine
        .plan_migrations_with_queries(&files, &queries)
        .unwrap();
    assert!(plan.query_validation.valid);
    let new = &plan.query_validation.files[0];
    assert!(!new.current_valid);
    assert_eq!(new.parameters_changed, None);
    assert_eq!(new.compatibility, CompatibilityLevel::Conditional);
    let old = &plan.query_validation.files[1];
    assert_eq!(old.failures.len(), 1);
    assert_eq!(old.failures[0].migration_id.as_deref(), Some("away"));
    engine
        .apply_migrations_with_queries(&files, &queries)
        .unwrap();
}

#[test]
fn no_pending_still_checks_queries_and_rejects_invalid_input_budgets() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let bad = [query("broken.unid", "from missing")];
    let plan = engine
        .plan_migrations_with_queries(&[initial()], &bad)
        .unwrap();
    assert!(plan.plan.pending.is_empty());
    assert!(!plan.query_validation.valid);
    assert_eq!(
        plan.query_validation.files[0].failures[0].migration_id,
        None
    );
    assert!(
        engine
            .apply_migrations_with_queries(&[initial()], &bad)
            .is_err()
    );
    assert_eq!(
        engine
            .plan_migrations_with_queries(&[initial()], &[])
            .unwrap_err()
            .code,
        "E_MIGRATION"
    );
    assert!(
        engine
            .plan_migrations_with_queries(&[initial()], &[bad[0].clone(), bad[0].clone()])
            .is_err()
    );
    let too_many = vec![query("x", "from jobs"); MAX_QUERY_FILES + 1];
    assert_eq!(
        engine
            .plan_migrations_with_queries(&[initial()], &too_many)
            .unwrap_err()
            .code,
        "E_LIMIT"
    );
    let large_path = [query(&"x".repeat(MAX_QUERY_REPORT_BYTES), "from jobs")];
    assert_eq!(
        engine
            .plan_migrations_with_queries(&[initial()], &large_path)
            .unwrap_err()
            .code,
        "E_LIMIT"
    );
}

#[test]
fn rejected_preflight_preserves_backup_receipts_sequence_and_index_state() {
    let tmp = TempDir::new();
    let db = tmp.0.join("receipt.redb");
    let before = tmp.0.join("before.json");
    let after = tmp.0.join("after.json");
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        setup(&mut engine);
        let source = "update jobs | set score = score + 1\nreturning\nexpect affected == 1";
        let digest = ProtocolRequest::query("attempt", source)
            .canonical_digest()
            .unwrap();
        engine
            .execute_idempotent_with_params("once", &digest, source, BTreeMap::new(), None)
            .unwrap();
    }
    backup::create(&db, &before).unwrap();
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        assert!(
            engine
                .apply_migrations_with_queries(
                    &[
                        initial(),
                        next("cancel", "initial", "add variant State.Cancelled")
                    ],
                    &[exhaustive()]
                )
                .is_err()
        );
        engine.check_integrity().unwrap();
    }
    backup::create(&db, &after).unwrap();
    let load = |p| serde_json::from_slice::<serde_json::Value>(&std::fs::read(p).unwrap()).unwrap();
    let before = load(before);
    let after = load(after);
    assert_eq!(before["database"], after["database"]);
    assert_eq!(before["receipts"], after["receipts"]);
}

#[test]
fn building_maintenance_is_unchanged_on_failure_and_can_resume_after_valid_preflight() {
    let tmp = TempDir::new();
    let db = tmp.0.join("building.redb");
    let files = [
        initial(),
        next("cancel", "initial", "add variant State.Cancelled"),
    ];
    let mut engine = Engine::open_redb(&db).unwrap();
    setup(&mut engine);
    engine.advance_migrations(&files, 1).unwrap();
    let before = engine.migration_status(&files).unwrap();
    assert!(before.maintenance.is_some());
    let compact = engine
        .apply_migrations_with_queries_v2(&files, &[exhaustive()])
        .unwrap_err();
    assert_eq!(compact.error.code, "E_MIGRATION");
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    assert!(
        engine
            .apply_migrations_with_queries(&files, &[exhaustive()])
            .is_err()
    );
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    drop(engine);
    let mut engine = Engine::open_redb(&db).unwrap();
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    let report = engine
        .apply_migrations_with_queries(&files, &[query("ids.unid", "from jobs | select {id}")])
        .unwrap();
    assert_eq!(report.applied.applied, ["cancel"]);
    engine.check_integrity().unwrap();
}

#[test]
fn recursive_parameter_and_result_contracts_are_compared_without_expanding_forever() {
    let mut engine = Engine::memory();
    let first = MigrationFile::parse("migration chains\n  add type Chain =\n    value int\n    next Option<Chain> = None\n  add table chains Chain key value\n").unwrap();
    engine
        .apply_migrations(std::slice::from_ref(&first))
        .unwrap();
    let queries = [query(
        "replace.unid",
        "update chains | filter value == $id | set next = $next | returning {next}\nexpect affected == 1",
    )];
    let report = engine
        .plan_migrations_with_queries(
            &[
                first,
                next("notes", "chains", "add field Chain.note: text = \"\""),
            ],
            &queries,
        )
        .unwrap()
        .query_validation;
    assert!(report.valid);
    assert_eq!(report.files[0].parameters_changed, Some(true));
    assert_eq!(report.files[0].result_changed, Some(true));
    assert_eq!(engine.execute("from chains").rows.len(), 0);
}

#[test]
fn source_and_binding_budgets_reject_before_planning_or_applying() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let large = [query(
        "large.unid",
        &" ".repeat(unionid::syntax::MAX_SOURCE_BYTES + 1),
    )];
    assert_eq!(
        engine
            .plan_migrations_with_queries(&[initial()], &large)
            .unwrap_err()
            .code,
        "E_LIMIT"
    );
    let mut files = vec![initial()];
    let mut parent = "initial".to_owned();
    for index in 0..65 {
        let id = format!("step_{index}");
        files.push(next(&id, &parent, "change default Job.score to 0"));
        parent = id;
    }
    let queries = (0..MAX_QUERY_FILES)
        .map(|i| query(&format!("{i}.unid"), "from jobs"))
        .collect::<Vec<_>>();
    let before = engine.schema_info();
    assert_eq!(
        engine
            .apply_migrations_with_queries_v2(&files, &queries)
            .unwrap_err()
            .error
            .code,
        "E_LIMIT"
    );
    assert_eq!(
        engine
            .plan_migrations_with_queries_v2(&[initial()], &large)
            .unwrap_err()
            .code,
        "E_LIMIT"
    );
    let long_path = [query(&"x".repeat(MAX_QUERY_REPORT_BYTES), "from jobs")];
    assert_eq!(
        engine
            .plan_migrations_with_queries_v2(&[initial()], &long_path)
            .unwrap_err()
            .code,
        "E_LIMIT"
    );
    assert_eq!(
        engine
            .apply_migrations_with_queries(&files, &queries)
            .unwrap_err()
            .error
            .code,
        "E_LIMIT"
    );
    assert_eq!(engine.schema_info(), before);
    assert_eq!(engine.migration_history().len(), 1);
}

#[test]
fn valid_queries_do_not_promise_directory_wide_atomic_data_conversion() {
    for compact in [false, true] {
        let mut engine = Engine::memory();
        setup(&mut engine);
        assert!(
            engine
                .execute("insert jobs {id: 2, state: Pending, score: 5}")
                .ok
        );
        let files = [
            initial(),
            next("note", "initial", "add field Job.note: text = \"\""),
            next("unique", "note", "add unique index jobs.score"),
        ];
        let queries = [query("ids.unid", "from jobs | select {id}")];
        assert!(
            engine
                .plan_migrations_with_queries(&files, &queries)
                .unwrap()
                .query_validation
                .valid
        );
        let (valid, error) = if compact {
            let failure = engine
                .apply_migrations_with_queries_v2(&files, &queries)
                .unwrap_err();
            (failure.query_validation.unwrap().valid, failure.error)
        } else {
            let failure = engine
                .apply_migrations_with_queries(&files, &queries)
                .unwrap_err();
            (failure.query_validation.unwrap().valid, failure.error)
        };
        assert!(valid);
        assert!(
            error
                .message
                .contains("earlier migration(s) were committed")
        );
        assert_eq!(engine.migration_history().len(), 2);
        assert!(engine.schema().contains("note: text"));
    }
}

#[test]
fn cutover_catalog_is_checked_before_reclaim_cleanup() {
    use unionid::migration::MigrationMaintenancePhase;
    let tmp = TempDir::new();
    let mut engine = Engine::open_redb(tmp.0.join("reclaim.redb")).unwrap();
    setup(&mut engine);
    let files = [
        initial(),
        next("cancel", "initial", "add variant State.Cancelled"),
    ];
    for _ in 0..16 {
        let progress = engine.advance_migrations(&files, 1).unwrap();
        if progress
            .status
            .maintenance
            .as_ref()
            .is_some_and(|m| m.phase == MigrationMaintenancePhase::Reclaimable)
        {
            break;
        }
    }
    let before = engine.migration_status(&files).unwrap();
    assert_eq!(
        before.maintenance.as_ref().unwrap().phase,
        MigrationMaintenancePhase::Reclaimable
    );
    assert_eq!(engine.migration_history().len(), 2);
    let error = engine
        .apply_migrations_with_queries(&files, &[exhaustive()])
        .unwrap_err();
    let report = error.query_validation.unwrap();
    let compact = engine
        .apply_migrations_with_queries_v2(&files, &[exhaustive()])
        .unwrap_err();
    assert_eq!(compact.error.code, "E_MIGRATION");
    assert_eq!(
        expand_compact(compact.query_validation.as_deref().unwrap()),
        *report
    );
    assert!(!report.files[0].current_valid);
    assert_eq!(report.current_schema, report.target_schema);
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    engine
        .apply_migrations_with_queries(&files, &[query("ids.unid", "from jobs | select {id}")])
        .unwrap();
    assert!(
        engine
            .migration_status(&files)
            .unwrap()
            .maintenance
            .is_none()
    );
    engine.check_integrity().unwrap();
}

#[test]
fn field_removal_rename_and_type_change_reject_before_memory_or_redb_effects() {
    for (name, step, source) in [
        (
            "remove",
            "drop field Job.score",
            "from jobs | select {score}",
        ),
        (
            "rename",
            "rename field Job.score to points",
            "from jobs | select {score}",
        ),
        (
            "retype",
            "change field Job.score to text\n    using old -> \"converted\"",
            "from jobs | filter score > 0",
        ),
    ] {
        let dir = TempDir::new();
        let db = dir.0.join("db.redb");
        for mut engine in [Engine::memory(), Engine::open_redb(&db).unwrap()] {
            setup(&mut engine);
            let schema = engine.schema_info();
            let before = serde_json::to_value(engine.execute("from jobs").rows).unwrap();
            let files = [initial(), next(name, "initial", step)];
            let error = engine
                .apply_migrations_with_queries(&files, &[query("saved.unid", source)])
                .unwrap_err();
            assert_eq!(error.error.code, "E_MIGRATION");
            let report = error.query_validation.unwrap();
            assert!(report.files[0].current_valid);
            assert!(!report.files[0].valid);
            assert_eq!(
                report.files[0].failures[0].migration_id.as_deref(),
                Some(name)
            );
            assert_eq!(engine.schema_info(), schema);
            assert_eq!(engine.migration_history().len(), 1);
            assert_eq!(
                serde_json::to_value(engine.execute("from jobs").rows).unwrap(),
                before
            );
            assert!(
                engine
                    .migration_status(&files)
                    .unwrap()
                    .maintenance
                    .is_none()
            );
        }
        let mut reopened = Engine::open_redb(&db).unwrap();
        assert_eq!(reopened.migration_history().len(), 1);
        assert!(reopened.execute("from jobs | filter score == 5").ok);
        reopened.check_integrity().unwrap();
    }
}

#[test]
fn ready_candidate_cannot_cut_over_or_resume_with_different_migration_sources() {
    use unionid::migration::MigrationMaintenancePhase;
    let dir = TempDir::new();
    let db = dir.0.join("ready.redb");
    let files = [
        initial(),
        next("cancel", "initial", "add variant State.Cancelled"),
    ];
    let mut engine = Engine::open_redb(&db).unwrap();
    setup(&mut engine);
    for _ in 0..16 {
        let progress = engine.advance_migrations(&files, 1).unwrap();
        if progress
            .status
            .maintenance
            .as_ref()
            .is_some_and(|m| m.phase == MigrationMaintenancePhase::Ready)
        {
            break;
        }
    }
    let before = engine.migration_status(&files).unwrap();
    assert_eq!(
        before.maintenance.as_ref().unwrap().phase,
        MigrationMaintenancePhase::Ready
    );
    let compact = engine
        .apply_migrations_with_queries_v2(&files, &[exhaustive()])
        .unwrap_err();
    assert_eq!(compact.error.code, "E_MIGRATION");
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    assert!(
        engine
            .apply_migrations_with_queries(&files, &[exhaustive()])
            .is_err()
    );
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    let changed = [
        initial(),
        next("cancel", "initial", "add variant State.Other"),
    ];
    let valid_query = query("ids.unid", "from jobs | select {id}");
    let error = engine
        .apply_migrations_with_queries(&changed, std::slice::from_ref(&valid_query))
        .unwrap_err();
    assert_eq!(error.error.code, "E_MIGRATION");
    assert!(
        error
            .error
            .hint
            .as_deref()
            .unwrap()
            .contains("original files")
    );
    let compact_error = engine
        .apply_migrations_with_queries_v2(&changed, std::slice::from_ref(&valid_query))
        .unwrap_err();
    assert_eq!(compact_error.error, error.error);
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    drop(engine);
    let mut reopened = Engine::open_redb(&db).unwrap();
    assert_eq!(reopened.migration_status(&files).unwrap(), before);
    reopened
        .apply_migrations_with_queries(&files, &[valid_query])
        .unwrap();
    assert_eq!(reopened.migration_history().len(), 2);
    assert!(
        reopened
            .migration_status(&files)
            .unwrap()
            .maintenance
            .is_none()
    );
    reopened.check_integrity().unwrap();
}

#[test]
fn aborting_candidate_is_not_cleaned_by_rejected_query_preflight() {
    use unionid::migration::MigrationMaintenancePhase;
    let dir = TempDir::new();
    let db = dir.0.join("aborting.redb");
    let files = [
        initial(),
        next(
            "cancel",
            "initial",
            "add variant State.Cancelled\n  add unique index jobs.score",
        ),
    ];
    let mut engine = Engine::open_redb(&db).unwrap();
    setup(&mut engine);
    assert!(
        engine
            .execute("insert jobs {id: 2, state: Done, score: 5}")
            .ok
    );
    engine.advance_migrations(&files, 1).unwrap();
    // One step records the deterministic unique violation as Aborting, but
    // leaves cleanup for another step. This exercises a real durable state.
    assert!(engine.advance_migrations(&files, 1).is_err());
    let before = engine.migration_status(&files).unwrap();
    assert_eq!(
        before.maintenance.as_ref().unwrap().phase,
        MigrationMaintenancePhase::Aborting
    );
    let error = engine
        .apply_migrations_with_queries(&files, &[exhaustive()])
        .unwrap_err();
    assert!(!error.query_validation.unwrap().valid);
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    drop(engine);
    let mut engine = Engine::open_redb(&db).unwrap();
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    engine.abort_migration().unwrap();
    assert_eq!(engine.migration_history().len(), 1);
    assert_eq!(engine.execute("from jobs").rows.len(), 2);
    engine.check_integrity().unwrap();
}

/// Expand only trusted, budget-sized reports to compare the public v1 semantics.
fn expand_compact(
    report: &unionid::migration::query_validation::QueryValidationV2,
) -> unionid::migration::query_validation::QueryValidation {
    use unionid::migration::query_validation::{
        QueryCheckpointFailure, QueryFileValidation, QueryValidation,
    };
    QueryValidation {
        version: 1,
        current_schema: report.current_schema.clone(),
        target_schema: report.target_schema.clone(),
        checked_files: report.checked_files,
        valid: report.valid,
        files: report
            .files
            .iter()
            .map(|file| QueryFileValidation {
                path: file.path.clone(),
                valid: file.valid,
                current_valid: file.current_valid,
                compatibility: file.compatibility,
                parameters_changed: file.parameters_changed,
                result_changed: file.result_changed,
                failures: file
                    .failures
                    .iter()
                    .flat_map(|interval| {
                        (interval.first_checkpoint..=interval.last_checkpoint).map(|index| {
                            let checkpoint = &report.checkpoints[index];
                            QueryCheckpointFailure {
                                migration_id: checkpoint.migration_id.clone(),
                                schema: checkpoint.schema.clone(),
                                error: interval.error.clone(),
                            }
                        })
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[test]
fn compact_reports_preserve_full_checkpoint_errors_and_shape_warnings() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let files = [
        initial(),
        next("cancel", "initial", "add variant State.Cancelled"),
        next("default", "cancel", "change default Job.score to 0"),
        next(
            "repair",
            "default",
            "drop variant State.Cancelled using old -> State.Done",
        ),
        next("again", "repair", "add variant State.Cancelled"),
    ];
    let queries = [
        query("missing.unid", "from missing"),
        query(
            "shape.unid",
            "from jobs | filter state == $state | select {state}",
        ),
        exhaustive(),
        query("new.unid", "from jobs | filter state == Cancelled"),
    ];
    for migrations in [&files[..1], &files[..3], &files[..4], &files[..]] {
        let full = engine
            .plan_migrations_with_queries(migrations, &queries)
            .unwrap();
        let compact = engine
            .plan_migrations_with_queries_v2(migrations, &queries)
            .unwrap();
        assert_eq!(compact.plan, full.plan);
        assert_eq!(
            expand_compact(&compact.query_validation),
            full.query_validation
        );
        let json = serde_json::to_vec(&compact.query_validation).unwrap();
        let decoded: unionid::migration::query_validation::QueryValidationV2 =
            serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, compact.query_validation);
    }
    let compact = engine
        .plan_migrations_with_queries_v2(&files, &queries)
        .unwrap()
        .query_validation;
    let matches = compact
        .files
        .iter()
        .find(|file| file.path == "states/check.unid")
        .unwrap();
    assert_eq!(matches.failures.len(), 2);
    assert_eq!(
        (
            matches.failures[0].first_checkpoint,
            matches.failures[0].last_checkpoint
        ),
        (1, 2)
    );
    assert_eq!(
        (
            matches.failures[1].first_checkpoint,
            matches.failures[1].last_checkpoint
        ),
        (4, 4)
    );
    assert!(!matches.valid);
}

fn repeated_preflight_files() -> Vec<MigrationFile> {
    let mut files = vec![initial()];
    let mut parent = "initial".to_owned();
    for i in 0..64 {
        let name = format!("step_{i:03}");
        files.push(next(&name, &parent, "change default Job.score to 0"));
        parent = name;
    }
    files
}

#[test]
fn compact_64_by_64_preflight_returns_final_errors_without_durable_effects() {
    let temp = TempDir::new();
    let db = temp.0.join("compact.redb");
    let archive = temp.0.join("archive");
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        setup(&mut engine);
        let source = "update jobs | set score = score + 1";
        let digest = ProtocolRequest::query("attempt", source)
            .canonical_digest()
            .unwrap();
        engine
            .execute_idempotent_with_params("once", &digest, source, BTreeMap::new(), None)
            .unwrap();
    }
    backup::incremental::init(&db, &archive, Default::default()).unwrap();
    let before = temp.0.join("before.json");
    backup::create(&db, &before).unwrap();
    let files = repeated_preflight_files();
    let queries = (0..64)
        .map(|i| query(&format!("query_{i:03}.unid"), "from missing"))
        .collect::<Vec<_>>();
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        let introspection = serde_json::to_value(engine.introspection()).unwrap();
        let status = serde_json::to_value(engine.backup_journal_status().unwrap()).unwrap();
        let maintenance = engine.migration_status(&files).unwrap();
        assert_eq!(
            engine
                .plan_migrations_with_queries(&files, &queries)
                .unwrap_err()
                .code,
            "E_LIMIT"
        );
        let planned = engine
            .plan_migrations_with_queries_v2(&files, &queries)
            .unwrap();
        let report = planned.query_validation;
        assert!(!report.valid);
        assert_eq!(report.version, 2);
        assert_eq!(report.checkpoints.len(), 65);
        assert_eq!(
            report.checkpoints.last().unwrap().schema,
            planned.plan.target_schema
        );
        for file in &report.files {
            assert!(!file.valid);
            assert_eq!(file.failures.len(), 1);
            assert_eq!(file.failures[0].first_checkpoint, 0);
            assert_eq!(file.failures[0].last_checkpoint, 64);
        }
        let bytes = serde_json::to_vec(&report).unwrap().len();
        assert!(bytes < MAX_QUERY_REPORT_BYTES);
        eprintln!("compact 64x65: checkpoints=65 intervals=64 encoded_bytes={bytes}");
        let rejected = engine
            .apply_migrations_with_queries_v2(&files, &queries)
            .unwrap_err();
        assert_eq!(rejected.error.code, "E_MIGRATION");
        assert_eq!(rejected.query_validation.as_deref(), Some(&report));
        assert_eq!(
            serde_json::to_value(engine.introspection()).unwrap(),
            introspection
        );
        assert_eq!(
            serde_json::to_value(engine.backup_journal_status().unwrap()).unwrap(),
            status
        );
        assert_eq!(engine.migration_status(&files).unwrap(), maintenance);
        engine.check_integrity().unwrap();
    }
    let after = temp.0.join("after.json");
    backup::create(&db, &after).unwrap();
    let load =
        |path| serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(load(before), load(after));
    let mut reopened = Engine::open_redb(&db).unwrap();
    reopened.check_integrity().unwrap();
    assert_eq!(reopened.migration_history().len(), 1);
    assert_eq!(reopened.idempotency_status().unwrap().count, 1);
    assert_eq!(reopened.execute("from jobs").rows.len(), 1);
}

#[test]
fn compact_large_preflight_can_repair_final_queries_then_apply_normally() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let mut files = repeated_preflight_files();
    files.push(next("repair", "step_063", "add table missing Job key id"));
    let queries = (0..64)
        .map(|i| query(&format!("query_{i:03}.unid"), "from missing"))
        .collect::<Vec<_>>();
    let plan = engine
        .plan_migrations_with_queries_v2(&files, &queries)
        .unwrap();
    assert!(plan.query_validation.valid);
    assert!(plan.query_validation.files.iter().all(|file| {
        !file.current_valid && file.valid && file.failures[0].last_checkpoint == 64
    }));
    let result = engine
        .apply_migrations_with_queries_v2(&files, &queries)
        .unwrap();
    assert_eq!(result.query_validation, plan.query_validation);
    assert_eq!(result.applied.applied.len(), 65);
    assert_eq!(engine.schema_info(), plan.plan.target_schema);
    assert!(engine.execute("from missing").ok);
    assert_eq!(engine.execute("from jobs").rows.len(), 1);
}
