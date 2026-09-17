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
        r#"enum State {
  Pending
  Running { worker: text }
  Done
}

struct Task {
  id: int
  state: State
  tags: List<text>
}

struct Note {
  id: int
  note: text
}

table active: Task { key id }
table archived: Task { key id }
table notes: Note { key id }

insert many active [
  {id: 1, state: Pending, tags: ["release"]}
  {id: 2, state: Pending, tags: ["release"]}
  {id: 3, state: Running {worker: "local"}, tags: ["release", "urgent"]}
]

insert many archived [
  {id: 10, state: Pending, tags: ["release"]}
  {id: 11, state: Done, tags: ["release"]}
]

insert notes {id: 1, note: "release"}"#,
    );
    engine
}

fn variants(response: &unionid::QueryResponse) -> Vec<String> {
    response
        .rows
        .iter()
        .map(|row| match row["state"].unwrapped() {
            Value::Enum(value) => value.variant.clone(),
            value => panic!("expected enum, got {value:?}"),
        })
        .collect()
}

#[test]
fn typed_set_operations_deduplicate_nested_adt_rows_in_stable_order() {
    let mut engine = setup();
    for (operator, expected) in [
        ("union", vec!["Pending", "Running", "Done"]),
        ("intersect", vec!["Pending"]),
        ("except", vec!["Running"]),
    ] {
        let source = format!(
            r#"from active
select {{state, tags}}
{operator} {{
  from archived
  select {{state, tags}}
}}"#
        );
        let formatted = format_source(&source).unwrap();
        assert_eq!(format_source(&formatted).unwrap(), formatted);
        let response = ok(&mut engine, &formatted);
        assert_eq!(variants(&response), expected);
    }
}

#[test]
fn set_operations_require_an_identical_schema_before_scanning() {
    let mut engine = setup();
    let response = engine.execute(
        r#"from active
select id
union {
  from notes
  select note
}"#,
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TYPE");
    assert!(
        response
            .message
            .contains("identical field names, order, and types")
    );
}

#[test]
fn set_operations_reject_nested_composition_and_cursor_pages() {
    let mut engine = setup();
    let nested = engine.execute(
        r#"from active
union {
  from archived
  union {
    from active
  }
}"#,
    );
    assert!(!nested.ok);
    assert_eq!(nested.error.unwrap().code, "E_QUERY");
    assert!(nested.message.contains("nested set operations"));

    let page = engine.execute(
        r#"from active
union {
  from archived
}
sort id
page 2"#,
    );
    assert!(!page.ok);
    assert_eq!(page.error.unwrap().code, "E_PAGE_SHAPE");
    assert!(page.message.contains("page cannot be combined"));
}

#[test]
fn explain_reports_the_set_boundary_and_right_access_without_rows() {
    let mut engine = setup();
    let response = ok(
        &mut engine,
        r#"explain from active
select {state, tags}
union {
  from archived
  filter id == 10
  select {state, tags}
}"#,
    );
    assert!(response.rows.is_empty());
    let plan = response.plan.unwrap();
    assert_eq!(plan.stages[1].kind, QueryStageKind::SetOperation);
    assert_eq!(plan.set_operations.len(), 1);
    assert_eq!(plan.set_operations[0].table, "archived");
    assert_eq!(plan.set_operations[0].stage, 2);
    assert_eq!(plan.set_operations[0].access.estimated_rows, 1);
}

#[test]
fn set_operation_parameters_bind_inside_the_right_pipeline() {
    let engine = setup();
    let prepared = engine
        .prepare(
            r#"from active
filter id == $left
select {state, tags}
union {
  from archived
  filter id == $right
  select {state, tags}
}"#,
        )
        .unwrap();
    assert_eq!(prepared.parameters().len(), 2);
}

#[test]
fn set_equality_matches_database_float_zero_semantics() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "create table positive (value float)\ncreate table negative (value float)\ninsert positive {value: 0.0}\ninsert negative {value: -0.0}",
    );
    let response = ok(
        &mut engine,
        r#"from positive
union {
  from negative
}"#,
    );
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn structurally_equal_named_adts_remain_nominally_distinct() {
    let mut engine = setup();
    ok(
        &mut engine,
        r#"enum OtherState {
  Pending
  Running { worker: text }
  Done
}

struct OtherTask {
  id: int
  state: OtherState
  tags: List<text>
}

table imported: OtherTask { key id }"#,
    );
    let response = engine.execute(
        r#"from active
select {state, tags}
union {
  from imported
  select {state, tags}
}"#,
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TYPE");
}
