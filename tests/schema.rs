use unionid::{Engine, MigrationFile};

fn ok(engine: &mut Engine, source: &str) {
    let response = engine.execute(source);
    assert!(response.ok, "{}", response.message);
}

#[test]
fn schema_check_normalizes_reusable_declarations_and_indexes() {
    let source = "type Task = {id int, title text}\ntable later Task\ntable tasks Task\n  key id\ncreate index tasks (title)";
    let checked = Engine::check_schema(source).unwrap();
    assert!(checked.normalized.contains("struct Task {"));
    assert!(checked.normalized.contains("create index tasks (title)"));
    assert!(!checked.normalized.contains("create index tasks (id)"));
    let repeated = Engine::check_schema(&checked.normalized).unwrap();
    assert_eq!(repeated, checked);

    let ordering =
        Engine::check_schema("create table raw (id int)\ntype Later = text").unwrap_err();
    assert!(ordering.message.contains("types before tables"));

    let error = Engine::check_schema("type Task = {id int}\ninsert tasks {id = 1}").unwrap_err();
    assert_eq!(error.code, "E_SCHEMA");
}

#[test]
fn bounded_migration_progress_requires_positive_format_six_steps() {
    let mut engine = Engine::memory();
    assert_eq!(
        engine.advance_migrations(&[], 0).unwrap_err().code,
        "E_LIMIT"
    );
    assert_eq!(
        engine.advance_migrations(&[], 1).unwrap_err().code,
        "E_CONFIG"
    );
}

#[test]
fn schema_diff_changes_index_constraint_kind_with_drop_then_add() {
    let base = "type User =\n  id int\n  email text\ntable users User\n  key id\ncreate index users (email)";
    let target = "type User =\n  id int\n  email text\ntable users User\n  key id\ncreate unique index users (email)";
    let checked = Engine::check_schema(target).unwrap();
    assert!(
        checked
            .normalized
            .contains("create unique index users (email)")
    );
    assert_eq!(Engine::check_schema(&checked.normalized).unwrap(), checked);

    let mut engine = Engine::memory();
    ok(&mut engine, base);
    let diff = engine
        .diff_schema(target, "m0001_unique_email", None)
        .unwrap();
    assert!(diff.runnable);
    assert_eq!(
        diff.operations
            .iter()
            .map(|operation| operation.description.as_str())
            .collect::<Vec<_>>(),
        ["drop index users.email", "add unique index users.email"]
    );
    assert!(
        diff.migration_source
            .contains("add unique index users.email")
    );
    let file = MigrationFile::parse(diff.migration_source).unwrap();
    engine.apply_migrations(&[file]).unwrap();
    assert_eq!(engine.schema(), checked.normalized);
}

#[test]
fn empty_database_diff_is_runnable_and_reaches_the_target_schema() {
    let target = "type State = Pending | Complete\ntype Task =\n  id int\n  state State\ntable tasks Task\n  key id\ncreate index tasks (state)";
    let mut engine = Engine::memory();
    let diff = engine.diff_schema(target, "m0001_initial", None).unwrap();
    assert!(diff.runnable);
    assert!(
        diff.operations
            .iter()
            .any(|item| item.description == "add table tasks Task key id")
    );
    let file = MigrationFile::parse(diff.migration_source).unwrap();
    engine.apply_migrations(&[file]).unwrap();
    assert_eq!(
        engine.schema(),
        Engine::check_schema(target).unwrap().normalized
    );
}

#[test]
fn recursive_target_schema_diff_is_runnable_and_stable() {
    let target = r#"type Chain =
  Next Chain
  | End

type Entry =
  id int
  chain Chain

table entries Entry
  key id

create index entries (chain)"#;
    let checked = Engine::check_schema(target).unwrap();
    assert_eq!(Engine::check_schema(&checked.normalized).unwrap(), checked);

    let mut engine = Engine::memory();
    let diff = engine.diff_schema(target, "m0001_recursive", None).unwrap();
    assert!(diff.runnable);
    let file = MigrationFile::parse(diff.migration_source).unwrap();
    engine.apply_migrations(&[file]).unwrap();
    assert_eq!(engine.schema(), checked.normalized);
}

#[test]
fn diff_reports_nested_type_impact_and_exhaustive_match_compatibility() {
    let current = "type State = Pending | Running\ntype Task =\n  id int\n  state State\ntype Boxed =\n  task Task\ntable active Task\n  key id\ntable archive Task\ntable boxes Boxed\ncreate index archive (state)";
    let target = "type State = Pending | Running | Complete\ntype Task =\n  id int\n  state State\n  priority int = 0\ntype Boxed =\n  task Task\ntable active Task\n  key id\ntable archive Task\ntable boxes Boxed\ncreate index archive (state)";
    let mut engine = Engine::memory();
    ok(&mut engine, current);
    ok(
        &mut engine,
        "insert active {id = 1, state = Pending}\ninsert boxes {task = {id = 2, state = Running}}",
    );
    let diff = engine.diff_schema(target, "m0001_evolve", None).unwrap();
    assert!(diff.runnable);
    assert!(
        diff.warnings
            .iter()
            .any(|warning| warning.contains("exhaustive client matches"))
    );
    let state = diff
        .impacts
        .iter()
        .find(|impact| impact.type_name == "State")
        .unwrap();
    assert_eq!(state.tables.len(), 3);
    assert!(
        state
            .tables
            .iter()
            .any(|table| table.table == "boxes" && table.rows == 1)
    );

    let file = MigrationFile::parse(diff.migration_source).unwrap();
    engine.plan_migrations(std::slice::from_ref(&file)).unwrap();
    engine.apply_migrations(&[file]).unwrap();
    assert_eq!(
        engine.schema(),
        Engine::check_schema(target).unwrap().normalized
    );
}

#[test]
fn formatting_only_changes_produce_no_operations() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Task =\n  id int\n  title text\ntable tasks Task\n  key id",
    );
    let diff = engine
        .diff_schema(
            "# same schema\n\ntype Task = { id int, title text }\n\ntable tasks Task\n  key id\n",
            "m0001_none",
            None,
        )
        .unwrap();
    assert!(diff.operations.is_empty());
}

#[test]
fn ambiguous_renames_and_required_backfills_are_non_runnable_todos() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Task =\n  id int\n  title text\ntable tasks Task",
    );
    let target = "type Task =\n  id int\n  label text\n  owner int\ntable tasks Task";
    let diff = engine.diff_schema(target, "m0001_edit", None).unwrap();
    assert!(!diff.runnable);
    assert!(
        diff.migration_source
            .contains("todo confirm rename field Task.title to label")
    );
    assert!(diff.migration_source.contains("required_backfill_value"));
    assert!(!diff.migration_source.contains("drop field Task.title"));
    assert!(MigrationFile::parse(diff.migration_source).is_err());
}

#[test]
fn table_rename_requires_confirmation_and_has_an_explicit_operation() {
    let mut engine = Engine::memory();
    ok(&mut engine, "type Task = {id int}\ntable tasks Task");
    let diff = engine
        .diff_schema(
            "type Task = {id int}\ntable archived_tasks Task",
            "m0001_rename",
            None,
        )
        .unwrap();
    assert!(!diff.runnable);
    assert!(
        diff.migration_source
            .contains("todo confirm rename table tasks to archived_tasks")
    );

    let applied = engine.execute("migration rename\n  rename table tasks to archived_tasks");
    assert!(applied.ok, "{}", applied.message);
    assert_eq!(engine.tables(), ["archived_tasks"]);
}

#[test]
fn runnable_diff_orders_constraint_and_index_changes_safely() {
    let current = "type Task =\n  id int\n  slug text\n  title text\ntable tasks Task\n  key id\ncreate index tasks (title)";
    let target = "type Task =\n  id int\n  slug text\ntable tasks Task\n  key slug";
    let mut engine = Engine::memory();
    ok(&mut engine, current);
    ok(
        &mut engine,
        "insert tasks {id = 1, slug = \"one\", title = \"first\"}",
    );
    let diff = engine
        .diff_schema(target, "m0001_constraints", None)
        .unwrap();
    assert!(diff.runnable);
    let lines = diff.migration_source.lines().collect::<Vec<_>>();
    let drop_key = lines
        .iter()
        .position(|line| line.contains("drop key"))
        .unwrap();
    let drop_id_index = lines
        .iter()
        .position(|line| line.contains("drop index tasks.id"))
        .unwrap();
    let drop_field = lines
        .iter()
        .position(|line| line.contains("drop field Task.title"))
        .unwrap();
    let set_key = lines
        .iter()
        .position(|line| line.contains("set key"))
        .unwrap();
    assert!(drop_key < drop_id_index && drop_id_index < drop_field && drop_field < set_key);
    let file = MigrationFile::parse(diff.migration_source).unwrap();
    engine.apply_migrations(&[file]).unwrap();
    assert_eq!(
        engine.schema(),
        Engine::check_schema(target).unwrap().normalized
    );
}

#[test]
fn diff_drops_tables_and_dependent_types_in_reference_order() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Inner = {value text}\ntype Outer = {inner Inner}\ntable items Outer",
    );
    let target = "type Replacement = {id int}";
    let diff = engine.diff_schema(target, "m0001_replace", None).unwrap();
    assert!(diff.runnable);
    let file = MigrationFile::parse(diff.migration_source).unwrap();
    engine.apply_migrations(&[file]).unwrap();
    assert_eq!(
        engine.schema(),
        Engine::check_schema(target).unwrap().normalized
    );
}

#[test]
fn schema_diff_preserves_composite_order_and_direction() {
    let current = "type Task = {id int, tenant text, priority int}\ntable tasks Task\n  key id\ncreate index tasks (tenant, priority)";
    let target = "type Task = {id int, tenant text, priority int}\ntable tasks Task\n  key id\ncreate unique index tasks (tenant, -priority)";
    let mut engine = Engine::memory();
    ok(&mut engine, current);
    let diff = engine
        .diff_schema(target, "m0001_index_shape", None)
        .unwrap();
    assert!(diff.runnable);
    assert!(
        diff.migration_source
            .contains("drop index tasks (tenant, priority)")
    );
    assert!(
        diff.migration_source
            .contains("add unique index tasks (tenant, -priority)")
    );
    let migration = MigrationFile::parse(diff.migration_source).unwrap();
    engine.apply_migrations(&[migration]).unwrap();
    assert_eq!(
        engine.schema(),
        Engine::check_schema(target).unwrap().normalized
    );
}

const REFERENCE_BASE: &str = r#"
struct Parent {id: int, alternate: int}
struct Child {id: int, parent: Option<int>}
table parents: Parent {key id}
table children: Child {key id}
"#;

#[test]
fn reference_diff_add_drop_and_initial_schema_are_runnable() {
    let target =
        format!("{REFERENCE_BASE}\ncreate reference children (parent) references parents (id)");
    let mut engine = Engine::memory();
    let initial = engine.diff_schema(&target, "initial", None).unwrap();
    assert!(initial.runnable);
    assert!(
        initial
            .operations
            .last()
            .unwrap()
            .description
            .starts_with("add reference ")
    );
    let initial = MigrationFile::parse(initial.migration_source).unwrap();
    engine
        .apply_migrations(std::slice::from_ref(&initial))
        .unwrap();
    ok(
        &mut engine,
        "insert parents {id: 1, alternate: 10}\ninsert children {id: 2, parent: Some(1)}",
    );
    assert!(
        engine
            .diff_schema(&target, "unchanged", None)
            .unwrap()
            .operations
            .is_empty()
    );
    let diff = engine
        .diff_schema(REFERENCE_BASE, "remove_reference", Some("initial"))
        .unwrap();
    assert_eq!(diff.operations.len(), 1);
    assert!(diff.operations[0].destructive);
    engine
        .apply_migrations(&[
            initial,
            MigrationFile::parse(diff.migration_source).unwrap(),
        ])
        .unwrap();
    ok(&mut engine, "delete parents");
    let error = engine.diff_schema(&target, "orphan", None).unwrap_err();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert_eq!(
        engine.schema(),
        Engine::check_schema(REFERENCE_BASE).unwrap().normalized
    );
}

#[test]
fn reference_diff_rebinds_replaced_target_key_and_orders_table_removal() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        &format!("{REFERENCE_BASE}\ncreate reference children (parent) references parents (id)"),
    );
    ok(
        &mut engine,
        "insert parents {id: 1, alternate: 10}\ninsert children {id: 2, parent: Some(1)}",
    );
    let target = format!(
        "{}\ncreate unique index parents (id)\ncreate reference children (parent) references parents (id)",
        REFERENCE_BASE.replace(
            "parents: Parent {key id}",
            "parents: Parent {key alternate}"
        )
    );
    let diff = engine.diff_schema(&target, "replace_key", None).unwrap();
    assert!(
        diff.operations
            .first()
            .unwrap()
            .description
            .starts_with("drop reference ")
    );
    assert!(
        diff.operations
            .last()
            .unwrap()
            .description
            .starts_with("add reference ")
    );
    let initial = MigrationFile::parse(diff.migration_source).unwrap();
    engine
        .apply_migrations(std::slice::from_ref(&initial))
        .unwrap();
    assert_eq!(
        engine.execute("delete parents").error.unwrap().code,
        "E_CONSTRAINT"
    );
    let target = "struct Parent {id: int, alternate: int}\ntable parents: Parent {key alternate}\ncreate unique index parents (id)";
    let diff = engine
        .diff_schema(target, "remove_child", Some("replace_key"))
        .unwrap();
    assert!(
        diff.operations
            .first()
            .unwrap()
            .description
            .starts_with("drop reference ")
    );
    engine
        .apply_migrations(&[
            initial,
            MigrationFile::parse(diff.migration_source).unwrap(),
        ])
        .unwrap();
    ok(&mut engine, "delete parents");
}

#[test]
fn reference_schema_round_trip_preserves_identity_while_diff_ignores_order() {
    let a = "create reference children (parent) references parents (id)";
    let b = "create reference children (parent) references parents (alternate)";
    let base = format!("{REFERENCE_BASE}\ncreate unique index parents (alternate)");
    let left = format!("{base}\n{a}\n{b}");
    let right = format!("{base}\n{b}\n{a}");
    for source in [&left, &right] {
        let checked = Engine::check_schema(source).unwrap();
        assert_eq!(Engine::check_schema(&checked.normalized).unwrap(), checked);
    }
    let mut engine = Engine::memory();
    ok(&mut engine, &left);
    assert!(
        engine
            .diff_schema(&right, "same", None)
            .unwrap()
            .operations
            .is_empty()
    );
}

#[test]
fn reference_diff_rebinds_a_replaced_unique_index() {
    let base = format!(
        "{REFERENCE_BASE}\ncreate unique index parents (alternate)\ncreate reference children (parent) references parents (alternate)"
    );
    let mut engine = Engine::memory();
    ok(&mut engine, &base);
    ok(
        &mut engine,
        "insert parents {id: 1, alternate: 10}\ninsert children {id: 2, parent: Some(10)}",
    );
    let target = base.replace("index parents (alternate)", "index parents (-alternate)");
    let diff = engine.diff_schema(&target, "reverse_index", None).unwrap();
    assert!(diff.runnable);
    assert_eq!(diff.operations.len(), 4);
    assert!(
        diff.operations[0]
            .description
            .starts_with("drop reference ")
    );
    assert!(diff.operations[3].description.starts_with("add reference "));
    engine
        .apply_migrations(&[MigrationFile::parse(diff.migration_source).unwrap()])
        .unwrap();
    assert_eq!(
        engine.schema(),
        Engine::check_schema(&target).unwrap().normalized
    );
    assert_eq!(
        engine.execute("delete parents").error.unwrap().code,
        "E_CONSTRAINT"
    );
}
