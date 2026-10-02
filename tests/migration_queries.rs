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
    let error = engine
        .apply_migrations_with_queries(&files, &queries)
        .unwrap_err();
    assert!(error.query_validation.unwrap().valid);
    assert!(
        error
            .error
            .message
            .contains("earlier migration(s) were committed")
    );
    assert_eq!(engine.migration_history().len(), 2);
    assert!(engine.schema().contains("note: text"));
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
