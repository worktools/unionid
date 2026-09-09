mod common;

use common::TempDir;
use unionid::migration::{
    ApplyDecision, MigrationEntry, MigrationFile, checksum, load_directory, validate_history,
    validate_next,
};
use unionid::{Engine, Value};

fn ok(engine: &mut Engine, source: &str) -> unionid::QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{}\n{source}", response.message);
    response
}

fn entry(id: &str, parent: Option<&str>, revision: u64) -> MigrationEntry {
    MigrationEntry {
        id: id.into(),
        parent: parent.map(Into::into),
        checksum: format!("checksum-{id}"),
        schema_revision: revision,
        schema_hash: format!("sha256:{id}"),
        applied_at_unix_ms: 0,
    }
}

#[test]
fn linear_history_can_be_loaded_in_any_order() {
    let first = entry("v1", None, 1);
    let second = entry("v2", Some("v1"), 2);
    assert_eq!(
        validate_history(&[second.clone(), first.clone()]).unwrap(),
        Some("v2")
    );
    assert_eq!(
        validate_next(&[first], &second).unwrap(),
        ApplyDecision::Apply
    );
}

#[test]
fn migration_history_rejects_forks_and_cycles() {
    let fork = [
        entry("v1", None, 1),
        entry("left", Some("v1"), 2),
        entry("right", Some("v1"), 2),
    ];
    let error = validate_history(&fork).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("forks"));

    let cycle = [entry("v1", Some("v2"), 1), entry("v2", Some("v1"), 2)];
    let error = validate_history(&cycle).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("cycle"));
}

#[test]
fn applying_a_migration_is_idempotent_but_immutable() {
    let first = entry("v1", None, 1);
    assert_eq!(
        validate_next(std::slice::from_ref(&first), &first).unwrap(),
        ApplyDecision::AlreadyApplied
    );
    let mut changed = first.clone();
    changed.checksum = "edited".into();
    assert!(validate_next(&[first], &changed).is_err());
}

#[test]
fn next_migration_must_extend_the_current_head() {
    let history = [entry("v1", None, 1), entry("v2", Some("v1"), 2)];
    let error = validate_next(&history, &entry("v3", Some("v1"), 3)).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("current head"));
}

fn initial_file() -> MigrationFile {
    MigrationFile::parse(
        "migration m0001_initial\n  add type Task =\n    id int\n    title text\n  add table tasks Task key id\n",
    )
    .unwrap()
}

fn title_file() -> MigrationFile {
    MigrationFile::parse(
        "migration m0002_title_default\n  parent m0001_initial\n  change default Task.title to \"untitled\"\n",
    )
    .unwrap()
}

#[test]
fn migration_files_have_normalized_checksums_and_linear_parents() {
    assert_eq!(checksum("migration x\n"), checksum("migration x\r\n\r\n"));
    let files = [initial_file(), title_file()];
    let mut engine = Engine::memory();
    let plan = engine.plan_migrations(&files).unwrap();
    assert_eq!(plan.applied_count, 0);
    assert_eq!(plan.pending.len(), 2);
    assert_eq!(plan.current_schema.revision, 0);
    assert_eq!(plan.target_schema.revision, 2);
    assert!(
        plan.pending[0]
            .operations
            .contains(&"add table tasks Task key id".into())
    );
    assert_eq!(engine.schema_info().revision, 0, "plan must be read-only");

    let applied = engine.apply_migrations(&files).unwrap();
    assert_eq!(applied.applied, ["m0001_initial", "m0002_title_default"]);
    assert!(applied.skipped.is_empty());
    assert_eq!(
        engine.migration_status(&files).unwrap().pending,
        Vec::<String>::new()
    );
    let repeated = engine.apply_migrations(&files).unwrap();
    assert!(repeated.applied.is_empty());
    assert_eq!(repeated.skipped.len(), 2);
}

#[test]
fn migration_apply_commits_files_individually_and_can_resume() {
    let dir = TempDir::new();
    let path = dir.0.join("resume.redb");
    let first = initial_file();
    let broken = MigrationFile::parse(
        "migration m0002_add_priority\n  parent m0001_initial\n  add field Missing.priority int = 0\n",
    )
    .unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    let error = engine
        .apply_migrations(&[first.clone(), broken])
        .unwrap_err();
    assert!(error.message.contains("m0001_initial"));
    assert_eq!(engine.migration_history().len(), 1);
    drop(engine);

    let fixed = MigrationFile::parse(
        "migration m0002_add_priority\n  parent m0001_initial\n  add field Task.priority int = 0\n",
    )
    .unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    let resumed = engine.apply_migrations(&[first, fixed]).unwrap();
    assert_eq!(resumed.skipped, ["m0001_initial"]);
    assert_eq!(resumed.applied, ["m0002_add_priority"]);
    assert!(engine.schema().contains("priority int = 0"));
}

#[test]
fn applied_migration_files_are_immutable() {
    let mut engine = Engine::memory();
    let first = initial_file();
    engine
        .apply_migrations(std::slice::from_ref(&first))
        .unwrap();
    let changed = MigrationFile::parse(format!("{}\n# edited\n", first.source)).unwrap();
    let error = engine.apply_migrations(&[changed]).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("was changed"));
    let missing = engine.apply_migrations(&[]).unwrap_err();
    assert!(missing.message.contains("is missing"));
}

#[test]
fn redb_persists_schema_and_migration_ledger_together() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    let files = [initial_file(), title_file()];
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.apply_migrations(&files).unwrap();
        assert!(engine.execute("insert tasks {id = 1}").ok);
    }
    let mut reopened = Engine::open_redb(&path).unwrap();
    assert_eq!(reopened.migration_status(&files).unwrap().applied.len(), 2);
    assert_eq!(reopened.execute("from tasks").rows.len(), 1);
    let direct_schema_change = reopened.execute("type Extra = int");
    assert!(!direct_schema_change.ok);
    assert_eq!(direct_schema_change.error.unwrap().code, "E_MIGRATION");
}

#[test]
fn redb_shadow_generation_migrates_multiple_bounded_batches() {
    let dir = TempDir::new();
    let path = dir.0.join("shadow-batches.redb");
    let initial = MigrationFile::parse(
        "migration m0001_items\n  add type Item =\n    id int\n    value int\n  add table items Item key id\n",
    )
    .unwrap();
    let changed = MigrationFile::parse(
        "migration m0002_items\n  parent m0001_items\n  add field Item.enabled bool = true\n  add index items (enabled, id)\n",
    )
    .unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    engine
        .apply_migrations(std::slice::from_ref(&initial))
        .unwrap();
    let mut source = String::from("insert many items [");
    for id in 0..2_500 {
        if id > 0 {
            source.push_str(", ");
        }
        source.push_str(&format!("{{id = {id}, value = {id}}}"));
    }
    source.push(']');
    ok(&mut engine, &source);
    let digest = format!("sha256:{}", "ab".repeat(32));
    let receipt = engine
        .execute_idempotent_with_params(
            "migration-receipt",
            &digest,
            "update items\nfilter id == 0\nset value = 42",
            std::collections::BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(!receipt.replayed);
    let before = engine.schema_info();
    engine
        .apply_migrations(&[initial.clone(), changed.clone()])
        .unwrap();
    let profile = engine
        .last_migration_profile()
        .expect("format-6 migration profile");
    assert_eq!(profile.source_rows_seen, 2_500);
    assert_eq!(profile.target_rows_written, 2_500);
    assert!(profile.index_entries_written >= 2_500);
    assert!(profile.logical_bytes > 0);
    assert!(profile.source_generation < profile.target_generation);
    assert!(profile.reclaim_complete);
    assert!(
        profile
            .prepare_micros
            .saturating_add(profile.build_micros)
            .saturating_add(profile.validate_micros)
            .saturating_add(profile.cutover_micros)
            .saturating_add(profile.reclaim_micros)
            <= profile.total_micros
    );
    assert_eq!(engine.schema_info().revision, before.revision + 1);
    let rows = ok(
        &mut engine,
        "from items | filter enabled == true | filter id == 2499",
    );
    assert_eq!(rows.rows.len(), 1);
    assert!(rows.rows[0]["id"].cmp_eq(&Value::Int(2_499)));
    let replay = engine
        .execute_idempotent_with_params(
            "migration-receipt",
            &digest,
            "the stored receipt bypasses this invalid source",
            std::collections::BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(replay.replayed);
    assert!(
        engine
            .migration_status(&[initial, changed])
            .unwrap()
            .maintenance
            .is_none()
    );
    drop(engine);
    let mut reopened = Engine::open_redb(path).unwrap();
    assert_eq!(
        reopened
            .execute("from items | filter enabled == true")
            .rows
            .len(),
        2_500
    );
    assert!(reopened.check_integrity().unwrap().backend_clean);
}

#[test]
fn redb_shadow_generation_cleans_a_deterministic_constraint_failure() {
    let dir = TempDir::new();
    let path = dir.0.join("shadow-failure.redb");
    let initial = MigrationFile::parse(
        "migration m0001_items\n  add type Item =\n    id int\n    group int\n  add table items Item key id\n",
    )
    .unwrap();
    let invalid = MigrationFile::parse(
        "migration m0002_unique_group\n  parent m0001_items\n  add unique index items (group)\n",
    )
    .unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    engine
        .apply_migrations(std::slice::from_ref(&initial))
        .unwrap();
    ok(
        &mut engine,
        "insert many items [{id = 1, group = 7}, {id = 2, group = 7}]",
    );

    let error = engine
        .apply_migrations(&[initial.clone(), invalid.clone()])
        .unwrap_err();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert!(engine.introspection().maintenance.is_none());
    assert_eq!(ok(&mut engine, "from items").rows.len(), 2);
    ok(&mut engine, "insert items {id = 3, group = 8}");
    assert_eq!(engine.migration_history().len(), 1);

    drop(engine);
    let mut reopened = Engine::open_redb(path).unwrap();
    assert_eq!(reopened.execute("from items").rows.len(), 3);
    assert!(reopened.introspection().maintenance.is_none());
}

#[test]
fn migration_directory_order_and_parent_are_validated() {
    let dir = TempDir::new();
    std::fs::write(dir.0.join("0001_initial.uid"), initial_file().source).unwrap();
    std::fs::write(
        dir.0.join("0002_bad.uid"),
        "migration m0002_bad\n  parent absent\n  add type Extra = text\n",
    )
    .unwrap();
    let error = load_directory(&dir.0).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("file order requires"));
}

#[test]
fn executable_schema_migration_example() {
    let mut engine = Engine::memory();
    let response = ok(
        &mut engine,
        include_str!("../examples/schema_migration.uid"),
    );
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["priority"].cmp_eq(&Value::Int(0)));
    assert!(
        response.rows[0]["state"]
            .source_text()
            .contains("code = 500")
    );
}

#[test]
fn schema_migration_renames_and_backfills_shared_nested_types() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        r#"type State = Pending | Failed {message text}
type Task =
  id int
  state State
type Envelope =
  task Task
table active Task
  key id
table archive Task
table boxed Envelope
create index archive (state)
create index boxed (task.id)
insert active {id = 1, state = Failed {message = "broken"}}
insert archive {id = 2, state = Pending}
insert boxed {task = {id = 3, state = Pending}}"#,
    );
    let before = engine.schema_info();

    let migrated = ok(
        &mut engine,
        "migration task_state_v2\n  rename type State to JobState\n  rename variant JobState.Failed to Rejected\n  rename field Task.id to task_id\n  add field Task.priority int = 0",
    );
    assert_eq!(
        migrated.schema.as_ref().unwrap().revision,
        before.revision + 1
    );
    let schema = engine.schema();
    assert!(schema.contains("type JobState"));
    assert!(schema.contains("Rejected"));
    assert!(schema.contains("task_id int"));
    assert!(schema.contains("priority int = 0"));

    let active = ok(
        &mut engine,
        "from active | filter task_id == 1 | select {task_id, priority, state}",
    );
    assert_eq!(active.rows.len(), 1);
    assert!(active.rows[0]["priority"].cmp_eq(&Value::Int(0)));
    assert_eq!(
        active.rows[0]["state"].source_text(),
        "Rejected {message = \"broken\"}"
    );
    assert_eq!(
        ok(&mut engine, "from archive | filter state == Pending")
            .rows
            .len(),
        1
    );
    assert_eq!(
        ok(
            &mut engine,
            "from boxed | filter task.task_id == 3 | select {task.task_id, task.priority}"
        )
        .rows
        .len(),
        1
    );
    ok(
        &mut engine,
        "insert active {task_id = 4, state = Pending, priority = 2}",
    );
    assert!(
        !engine
            .execute("insert active {task_id = 1, state = Pending, priority = 2}")
            .ok
    );
}

#[test]
fn schema_migration_changes_variant_payload_with_a_typed_transform() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        r#"type State = Pending | Failed {message text}
type Task =
  id int
  state State
table active Task
table archive Task
insert active {id = 1, state = Failed {message = "a"}}
insert archive {id = 2, state = Failed {message = "b"}}"#,
    );
    ok(
        &mut engine,
        "migration failed_payload_v2\n  rename variant State.Failed to Rejected\n  change variant State.Rejected to {code int, message text}\n    using old -> {code = 0, message = old.message}",
    );

    for table in ["active", "archive"] {
        let response = ok(
            &mut engine,
            &format!(
                "from {table} | filter match state\n  Rejected {{code, ..}} => code == 0\n  Pending => false"
            ),
        );
        assert_eq!(response.rows.len(), 1);
        assert!(response.rows[0]["state"].source_text().contains("code = 0"));
    }
}

#[test]
fn dropping_a_live_variant_requires_a_mapping_and_is_atomic() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type State = Pending | Done\ntype Task =\n  id int\n  state State\ntable tasks Task\ninsert tasks {id = 1, state = Pending}",
    );
    let before = engine.schema_info();
    let failed = engine.execute(
        "migration remove_pending\n  rename variant State.Done to Complete\n  drop variant State.Pending",
    );
    assert!(!failed.ok);
    assert_eq!(failed.error.as_ref().unwrap().code, "E_MIGRATION");
    assert!(failed.message.contains("RowId 0"));
    assert_eq!(engine.schema_info(), before);
    assert_eq!(
        ok(&mut engine, "from tasks | filter state == Done")
            .rows
            .len(),
        0,
        "the earlier rename in the failed migration must roll back"
    );

    ok(
        &mut engine,
        "migration remove_pending\n  drop variant State.Pending\n    using old -> State.Done",
    );
    assert_eq!(
        ok(&mut engine, "from tasks | filter state == Done")
            .rows
            .len(),
        1
    );
    assert!(!engine.schema().contains("Pending"));
}

#[test]
fn field_type_changes_use_typed_expressions_and_rebuild_indexes() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Metric =\n  id int\n  score int = 1\ntable metrics Metric\n  key id\ncreate index metrics (score)\ninsert metrics {id = 1, score = 2}",
    );
    ok(
        &mut engine,
        "migration metric_v2\n  change field Metric.score to float\n    using old -> old",
    );
    let response = ok(&mut engine, "from metrics | filter score == 2.0");
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["score"].cmp_eq(&Value::Float(2.0)));
    ok(&mut engine, "insert metrics {id = 2}");
    assert_eq!(
        ok(&mut engine, "from metrics | filter score == 1.0")
            .rows
            .len(),
        1,
        "the field transform also migrates its default"
    );
}

#[test]
fn field_type_changes_can_return_boolean_expressions() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "type Feature =\n  id int\n  score int\ntable features Feature\n  key id\ninsert features {id = 1, score = 2}\ninsert features {id = 2, score = 0}"
            )
            .ok
    );
    let migrated = engine.execute(
        "migration feature_enabled\n  change field Feature.score to bool\n    using old -> old > 0",
    );
    assert!(migrated.ok, "{}", migrated.message);
    let rows = engine.execute("from features | sort id");
    assert!(rows.rows[0]["score"].cmp_eq(&Value::Bool(true)));
    assert!(rows.rows[1]["score"].cmp_eq(&Value::Bool(false)));
}

#[test]
fn migration_transforms_preserve_nested_named_record_identity() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        r#"type Meta =
  attempts int
type OldPayload =
  meta Meta
type NewPayload =
  meta Meta
  label text
type Item =
  id int
  payload OldPayload
table items Item
  key id
insert items {id = 1, payload = {meta = {attempts = 3}}}"#,
    );

    ok(
        &mut engine,
        r#"migration payload_v2
  change field Item.payload to NewPayload
    using old -> {meta = old.meta, label = "preserved"}"#,
    );

    let response = ok(
        &mut engine,
        "from items | filter payload.meta.attempts == 3",
    );
    assert_eq!(response.rows.len(), 1);
    assert!(
        response.rows[0]["payload"]
            .source_text()
            .contains("label = \"preserved\"")
    );
}

#[test]
fn type_add_and_drop_obey_reference_integrity() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "migration temporary_type\n  add type Temporary = text\n  drop type Temporary",
    );
    assert!(!engine.schema().contains("Temporary"));

    ok(
        &mut engine,
        "type State = Ready | Waiting\ntype Task =\n  state State\ntable tasks Task",
    );
    let before = engine.schema_info();
    let failed = engine
        .execute("migration invalid_drop\n  add field Task.note text = \"x\"\n  drop type State");
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().code, "E_MIGRATION");
    assert_eq!(engine.schema_info(), before);
    assert!(!engine.schema().contains("note text"));

    ok(
        &mut engine,
        "migration recursive_type\n  add field Task.child option Task = None",
    );
    assert!(engine.schema().contains("child option Task = None"));
}

#[test]
fn recursive_record_migrations_rewrite_every_finite_occurrence() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        r#"type Chain =
  value int
  next option Chain = None

table chains Chain
  key value

create index chains (next)

insert chains
  value = 1
  next = Some {value = 2}"#,
    );
    ok(
        &mut engine,
        r#"migration chain_v2
  rename field Chain.value to number
  add field Chain.note text = "added""#,
    );
    let rows = ok(
        &mut engine,
        r#"from chains
filter number == 1
filter match next
  None => false
  Some {number, note, ..} => number == 2 and note == "added""#,
    );
    assert_eq!(rows.rows.len(), 1);
    assert!(rows.rows[0]["note"].cmp_eq(&Value::Text("added".into())));
    assert!(engine.schema().contains("key number"));
    assert!(engine.schema().contains("create index chains (next)"));

    let mut sum = Engine::memory();
    ok(
        &mut sum,
        r#"type Link =
  Next Link
  | End

type LinkRow =
  id int
  link Link

table links LinkRow"#,
    );
    let before = sum.schema_info();
    let failed = sum.execute("migration remove_end\n  drop variant Link.End");
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().code, "E_SCHEMA");
    assert_eq!(sum.schema_info(), before);
    assert!(sum.schema().contains("| End"));

    let mut mutual = Engine::memory();
    ok(
        &mut mutual,
        r#"type Parent =
  id int

type Child =
  parent option Parent = None"#,
    );
    let before = mutual.schema_info();
    let failed =
        mutual.execute("migration mutual_cycle\n  add field Parent.child option Child = None");
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().code, "E_SCHEMA");
    assert_eq!(mutual.schema_info(), before);
    assert!(!mutual.schema().contains("child option Child"));
}

#[test]
fn migration_updates_defaults_variants_keys_and_indexes_explicitly() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        r#"type Status = Active | Paused
type Item =
  id int
  name text
  note text = "old"
  status Status
table items Item
  key id
create index items (note)
insert items {id = 1, name = "first", status = Active}"#,
    );
    ok(
        &mut engine,
        "migration item_constraints_v2\n  add variant Status.Cancelled text\n  change default Item.note to \"new\"\n  add index items.status\n  set key items.name",
    );
    ok(
        &mut engine,
        "insert items {id = 2, name = \"second\", status = Cancelled \"obsolete\"}",
    );
    let rows = ok(&mut engine, "from items | filter note == \"new\"");
    assert_eq!(rows.rows.len(), 1);
    assert!(
        !engine
            .execute("insert items {id = 3, name = \"second\", status = Active}")
            .ok
    );

    ok(
        &mut engine,
        "migration require_note\n  drop default Item.note",
    );
    assert!(
        !engine
            .execute("insert items {id = 3, name = \"third\", status = Active}")
            .ok
    );

    let blocked = engine.execute("migration unsafe_drop\n  drop field Item.note");
    assert!(!blocked.ok);
    assert!(blocked.message.contains("drop the key/index first"));
    ok(
        &mut engine,
        "migration remove_note\n  drop index items.note\n  drop field Item.note",
    );
    assert!(!engine.schema().contains("note text"));
    assert!(!engine.execute("from items | filter note == \"new\"").ok);
}

#[test]
fn migrations_add_rename_and_drop_composite_index_shapes() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Entry = {id int, tenant text, priority int}\ntable entries Entry\n  key id\ninsert entries {id = 1, tenant = \"a\", priority = 1}",
    );
    ok(
        &mut engine,
        "migration add_shape\n  add unique index entries (tenant, -priority)",
    );
    assert!(
        engine
            .schema()
            .contains("create unique index entries (tenant, -priority)")
    );

    ok(
        &mut engine,
        "migration rename_component\n  rename field Entry.priority to rank",
    );
    assert!(
        engine
            .schema()
            .contains("create unique index entries (tenant, -rank)")
    );
    let blocked = engine.execute("migration drop_live_field\n  drop field Entry.rank");
    assert_eq!(blocked.error.as_ref().unwrap().code, "E_MIGRATION");

    ok(
        &mut engine,
        "migration remove_shape\n  drop index entries (tenant, -rank)\n  drop field Entry.rank",
    );
    assert!(!engine.schema().contains("-rank"));
}
