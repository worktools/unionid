use unionid::{Engine, QueryStageKind, Value, format_source};

fn ok(engine: &mut Engine, source: &str) -> unionid::QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{}\n{source}", response.message);
    response
}

fn setup() -> Engine {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        r#"enum ItemState {
  Open
  Done
}

struct Task {
  id: int
  title: text
}

struct Item {
  id: int
  task_id: int
  note: text
  state: ItemState
}

table tasks: Task { key id }
table items: Item { key id }
create index items (task_id)

insert many tasks [
  {id: 1, title: "ship"}
  {id: 2, title: "done"}
  {id: 3, title: "empty"}
]

insert many items [
  {id: 10, task_id: 1, note: "closed", state: Done}
  {id: 11, task_id: 1, note: "ship", state: Open}
  {id: 12, task_id: 2, note: "done", state: Done}
]"#,
    );
    engine
}

#[test]
fn correlated_exists_filters_tasks_and_formats_canonically() {
    let mut engine = setup();
    let source = r#"from tasks
filter exists {
  from items
  filter task_id == outer.id
  filter state != Done
}
sort id
select {id, title}"#;

    let formatted = format_source(source).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    let response = ok(&mut engine, &formatted);
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(response.rows[0]["title"].cmp_eq(&Value::Text("ship".into())));
}

#[test]
fn explain_reports_the_exists_boundary_without_executing_rows() {
    let mut engine = setup();
    let response = ok(
        &mut engine,
        r#"explain from tasks
filter exists {
  from items
  filter task_id == outer.id
  filter state != Done
}"#,
    );
    assert!(response.rows.is_empty());
    let plan = response.plan.unwrap();
    assert_eq!(plan.stages[0].kind, QueryStageKind::FilterExists);
    assert_eq!(plan.exists.len(), 1);
    assert_eq!(plan.exists[0].table, "items");
    assert_eq!(plan.exists[0].index, "items.task_id");
    assert_eq!(plan.exists[0].correlations[0].target, "task_id");
    assert_eq!(plan.exists[0].correlations[0].outer, "id");
    assert_eq!(plan.exists[0].driver_limit, unionid::MAX_EXISTS_DRIVERS);
}

#[test]
fn invalid_exists_shapes_fail_before_scanning() {
    let mut engine = setup();
    let cases = [
        (
            r#"from tasks
filter exists {
  from items
  filter state != Done
}"#,
            "requires an equality correlation",
        ),
        (
            r#"from tasks
filter exists {
  from items
  filter state == outer.title
}"#,
            "expected ItemState",
        ),
        (
            r#"from tasks
filter exists {
  from items
  filter note == outer.title
}"#,
            "first field of an index",
        ),
        (
            r#"from tasks
filter exists {
  from items
  filter task_id == outer.id
  sort id
}"#,
            "support only filter stages",
        ),
    ];
    for (source, message) in cases {
        let response = engine.execute(source);
        assert!(!response.ok, "unexpected success:\n{source}");
        assert!(response.message.contains(message), "{}", response.message);
    }
}

#[test]
fn exists_is_not_yet_a_mutation_target() {
    let mut engine = setup();
    let response = engine.execute(
        r#"update tasks
filter exists {
  from items
  filter task_id == outer.id
}
set title = "blocked""#,
    );
    assert!(!response.ok);
    assert!(response.message.contains("update and delete targets"));
}

#[test]
fn exists_rejects_more_than_the_bounded_driver_count() {
    let mut engine = Engine::memory();
    let parents = (0..=unionid::MAX_EXISTS_DRIVERS)
        .map(|id| format!("{{id: {id}}}"))
        .collect::<Vec<_>>()
        .join(", ");
    ok(
        &mut engine,
        &format!(
            r#"struct Parent {{ id: int }}
struct Child {{ id: int, parent_id: int }}
table parents: Parent {{ key id }}
table children: Child {{ key id }}
create index children (parent_id)
insert many parents [{parents}]"#
        ),
    );
    let response = engine.execute(
        r#"from parents
filter exists {
  from children
  filter parent_id == outer.id
}"#,
    );
    assert!(!response.ok);
    assert!(response.message.contains("exists has 10001 driver rows"));
    assert!(response.message.contains("limit is 10000"));
}

#[test]
fn exists_inner_filters_bind_prepared_parameters() {
    let mut engine = setup();
    let prepared = engine
        .prepare(
            r#"from tasks
filter exists {
  from items
  filter task_id == outer.id
  filter note == $note
}
select id"#,
        )
        .unwrap();
    assert_eq!(prepared.parameter_types()["note"], "text");
    let response = engine.query(
        &prepared,
        [("note".into(), Value::Text("ship".into()))]
            .into_iter()
            .collect(),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["id"].cmp_eq(&Value::Int(1)));
}

#[test]
fn exists_obeys_the_shared_execution_deadline() {
    let mut engine = setup();
    let response = engine.execute_with_params_until(
        r#"from tasks
filter exists {
  from items
  filter task_id == outer.id
        }"#,
        Default::default(),
        None,
        std::time::Instant::now() - std::time::Duration::from_millis(1),
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TIMEOUT");
}
