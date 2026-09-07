mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::TempDir;
use unionid::protocol::{Request, Response, VERSION, WireValue};
use unionid::server::execute_protocol_request;
use unionid::{Engine, IntrospectionKind, QueryAccessKind, StorageMode, Value};

fn setup() -> Engine {
    let mut engine = Engine::memory();
    let response = engine.execute(
        r#"type State =
  Pending
  | Running {worker text, attempt int}
type Task =
  id int
  title text
  state State
table tasks Task
  key id
insert tasks
  id = 9007199254740993
  title = "quoted \"text\"\nwith | pipe"
  state = Running {worker = "local", attempt = 2}"#,
    );
    assert!(response.ok, "{}", response.message);
    engine
}

#[derive(Debug, serde::Deserialize, PartialEq)]
struct TaskView {
    id: i64,
    title: String,
}

#[test]
fn transport_neutral_protocol_uses_serde_params_and_typed_rows() {
    let mut engine = setup();
    let request = Request::query(
        "http-task",
        "from tasks | filter id == $id | select {id, title}",
    )
    .with_serde_param("id", &9_007_199_254_740_993_i64)
    .unwrap();
    assert_eq!(
        request.params["id"],
        WireValue::Int {
            value: "9007199254740993".into()
        }
    );

    let response = execute_protocol_request(&mut engine, request);
    assert_eq!(response.request_id, "http-task");
    assert_eq!(
        response.typed_rows::<TaskView>().unwrap(),
        vec![TaskView {
            id: 9_007_199_254_740_993,
            title: "quoted \"text\"\nwith | pipe".into(),
        }]
    );
}

#[test]
fn typed_parameters_bind_without_changing_query_syntax() {
    let mut engine = setup();
    let source = "from tasks\nfilter id == $id\nselect {id, title}";
    let mut params = BTreeMap::new();
    params.insert("id".into(), Value::Int(9_007_199_254_740_993));
    let response = engine.execute_with_params(source, params);
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
    assert!(matches!(
        response.rows[0].get("title"),
        Some(Value::Text(value)) if value == "quoted \"text\"\nwith | pipe"
    ));
}

#[test]
fn missing_extra_and_wrong_typed_parameters_have_stable_codes() {
    let mut engine = setup();
    let source = "from tasks | filter id == $id";
    assert_eq!(
        engine
            .execute_with_params(source, BTreeMap::new())
            .error
            .unwrap()
            .code,
        "E_PARAM_MISSING"
    );
    let mut extra = BTreeMap::new();
    extra.insert("id".into(), Value::Int(1));
    extra.insert("other".into(), Value::Int(2));
    assert_eq!(
        engine
            .execute_with_params(source, extra)
            .error
            .unwrap()
            .code,
        "E_PARAM_EXTRA"
    );
    let mut wrong = BTreeMap::new();
    wrong.insert("id".into(), Value::Text("1".into()));
    assert_eq!(
        engine
            .execute_with_params(source, wrong)
            .error
            .unwrap()
            .code,
        "E_TYPE"
    );
}

#[test]
fn prepared_queries_reject_schema_changes_and_unsupported_mutations() {
    let mut engine = setup();
    let prepared = engine.prepare("from tasks | filter id == $id").unwrap();
    assert_eq!(prepared.parameters(), &["id"]);
    assert_eq!(prepared.parameter_types()["id"], "int");
    assert_eq!(
        engine
            .prepare("from tasks | filter absent == $id")
            .unwrap_err()
            .code,
        "E_FIELD"
    );
    assert_eq!(
        engine
            .prepare("from tasks | filter id == $value and title == $value")
            .unwrap_err()
            .code,
        "E_TYPE"
    );
    let mut params = BTreeMap::new();
    params.insert("id".into(), Value::Int(9_007_199_254_740_993));
    assert!(engine.query(&prepared, params.clone()).ok);
    let prepared_explain = engine
        .prepare("explain from tasks | filter id == $id")
        .unwrap();
    assert_eq!(prepared_explain.parameter_types()["id"], "int");
    let explained = engine.query(&prepared_explain, params.clone());
    assert_eq!(
        explained.plan.as_ref().unwrap().access.kind,
        QueryAccessKind::PrimaryKeyLookup
    );
    let wire = Response::from_query("explain-1", explained);
    assert_eq!(
        wire.plan.as_ref().unwrap().access.kind,
        QueryAccessKind::PrimaryKeyLookup
    );
    assert_eq!(
        engine
            .prepare("create table forbidden (id int)")
            .unwrap_err()
            .code,
        "E_PREPARE"
    );
    assert!(engine.execute("create table metadata (id int)").ok);
    assert_eq!(
        engine.query(&prepared, params).error.unwrap().code,
        "E_SCHEMA_CHANGED"
    );
}

#[test]
fn prepared_bulk_insert_infers_row_lists_and_honors_deadlines() {
    let mut engine = setup();
    let prepared = engine
        .prepare("insert many tasks $rows\nreturning id, state")
        .unwrap();
    assert_eq!(prepared.parameters(), &["rows"]);
    assert_eq!(prepared.parameter_types()["rows"], "list Task");

    let task = |id, title: &str| {
        Value::Record(BTreeMap::from([
            ("id".into(), Value::Int(id)),
            ("title".into(), Value::Text(title.into())),
            (
                "state".into(),
                Value::Enum(unionid::model::EnumValue {
                    id: 0,
                    variant: "Pending".into(),
                    args: Vec::new(),
                }),
            ),
        ]))
    };
    let inserted = engine.execute_prepared(
        &prepared,
        BTreeMap::from([(
            "rows".into(),
            Value::List(vec![task(2, "second"), task(1, "first")]),
        )]),
    );
    assert!(inserted.ok, "{}", inserted.message);
    assert_eq!(inserted.affected_rows, Some(2));
    assert!(inserted.rows[0]["id"].cmp_eq(&Value::Int(2)));
    assert!(inserted.rows[1]["id"].cmp_eq(&Value::Int(1)));

    let expired = engine.execute_prepared_until(
        &prepared,
        BTreeMap::from([("rows".into(), Value::List(vec![task(3, "late")]))]),
        Instant::now() - Duration::from_millis(1),
    );
    assert!(!expired.ok);
    assert_eq!(expired.error.unwrap().code, "E_TIMEOUT");
    assert!(
        engine
            .execute("from tasks | filter id == 3")
            .rows
            .is_empty()
    );
}

#[test]
fn prepared_dml_infers_types_and_executes_atomically() {
    let mut engine = setup();
    let state = |variant: &str, args: Vec<Value>| {
        Value::Enum(unionid::model::EnumValue {
            id: 0,
            variant: variant.into(),
            args,
        })
    };
    let row = |id, title: &str, state: Value| {
        Value::Record(BTreeMap::from([
            ("id".into(), Value::Int(id)),
            ("title".into(), Value::Text(title.into())),
            ("state".into(), state),
        ]))
    };

    let insert = engine
        .prepare("insert tasks $row\nreturning id, state")
        .unwrap();
    assert_eq!(insert.parameter_types()["row"], "Task");
    let inserted = engine.execute_prepared(
        &insert,
        BTreeMap::from([("row".into(), row(1, "one", state("Pending", vec![])))]),
    );
    assert!(inserted.ok, "{}", inserted.message);
    assert!(inserted.rows[0]["id"].cmp_eq(&Value::Int(1)));

    let upsert = engine
        .prepare("upsert tasks $row\nreturning id, title")
        .unwrap();
    assert_eq!(upsert.parameter_types()["row"], "Task");
    let updated = engine.execute_prepared(
        &upsert,
        BTreeMap::from([("row".into(), row(1, "replaced", state("Pending", vec![])))]),
    );
    assert!(updated.ok, "{}", updated.message);
    assert_eq!(
        updated.upsert_action,
        Some(unionid::db::UpsertAction::Updated)
    );
    assert!(updated.rows[0]["title"].cmp_eq(&Value::Text("replaced".into())));

    let update = engine
        .prepare(
            "update tasks\nfilter id == $id\nset state = match state\n  Pending => $state\n  current => current\nreturning id, state",
        )
        .unwrap();
    assert_eq!(update.parameter_types()["id"], "int");
    assert_eq!(update.parameter_types()["state"], "State");
    let running = state(
        "Running",
        vec![Value::Record(BTreeMap::from([
            ("attempt".into(), Value::Int(3)),
            ("worker".into(), Value::Text("prepared".into())),
        ]))],
    );
    let changed = engine.execute_prepared(
        &update,
        BTreeMap::from([("id".into(), Value::Int(1)), ("state".into(), running)]),
    );
    assert!(changed.ok, "{}", changed.message);
    assert_eq!(changed.affected_rows, Some(1));
    assert_eq!(
        changed.rows[0]["state"].source_text(),
        "Running {attempt = 3, worker = \"prepared\"}"
    );

    let expired = engine.execute_prepared_until(
        &update,
        BTreeMap::from([
            ("id".into(), Value::Int(1)),
            ("state".into(), state("Pending", vec![])),
        ]),
        Instant::now() - Duration::from_millis(1),
    );
    assert!(!expired.ok);
    assert_eq!(expired.error.unwrap().code, "E_TIMEOUT");
    assert_eq!(
        engine.execute("from tasks | filter id == 1").rows[0]["state"].source_text(),
        "Running {attempt = 3, worker = \"prepared\"}"
    );

    let delete = engine
        .prepare("delete tasks\nfilter id == $id\nreturning id")
        .unwrap();
    assert_eq!(delete.parameter_types()["id"], "int");
    let deleted = engine.execute_prepared(&delete, BTreeMap::from([("id".into(), Value::Int(1))]));
    assert!(deleted.ok, "{}", deleted.message);
    assert_eq!(deleted.affected_rows, Some(1));
    assert!(deleted.rows[0]["id"].cmp_eq(&Value::Int(1)));

    assert!(engine.execute("create table metadata (id int)").ok);
    let stale = engine.execute_prepared(
        &insert,
        BTreeMap::from([("row".into(), row(2, "stale", state("Pending", vec![])))]),
    );
    assert!(!stale.ok);
    assert_eq!(stale.error.unwrap().code, "E_SCHEMA_CHANGED");
}

#[test]
fn prepared_dml_rejects_invalid_empty_table_operations_before_execution() {
    let mut engine = setup();
    assert!(engine.execute("delete tasks").ok);

    assert_eq!(
        engine
            .prepare("update tasks\nset missing = $value")
            .unwrap_err()
            .code,
        "E_FIELD"
    );
    assert_eq!(
        engine
            .prepare("delete tasks\nfilter missing == $value")
            .unwrap_err()
            .code,
        "E_FIELD"
    );
    assert_eq!(
        engine
            .prepare("update tasks\nfilter id == $value\nset title = $value")
            .unwrap_err()
            .code,
        "E_TYPE"
    );
    assert_eq!(
        engine
            .prepare("update tasks\nset state = match state\n  Pending => $state")
            .unwrap_err()
            .code,
        "E_MATCH"
    );
    assert_eq!(
        engine
            .prepare("delete tasks\nreturning missing")
            .unwrap_err()
            .code,
        "E_FIELD"
    );

    let mut no_key = Engine::memory();
    assert!(no_key.execute("create table items (id int)").ok);
    assert_eq!(
        no_key.prepare("upsert items $row").unwrap_err().code,
        "E_CONSTRAINT"
    );
    assert_eq!(
        no_key.prepare("insert items {id = 1}").unwrap_err().code,
        "E_PREPARE"
    );
}

#[test]
fn prepared_bulk_insert_rejects_the_transitional_wal() {
    let temp = TempDir::new();
    let wal = temp.0.join("prepared-bulk.wal");
    let mut engine = Engine::open(Some(wal), None, 0).unwrap();
    assert!(
        engine
            .execute("type Item =\n  id int\ntable items Item\n  key id")
            .ok
    );
    let prepared = engine.prepare("insert many items $rows").unwrap();
    let response = engine.execute_prepared(
        &prepared,
        BTreeMap::from([(
            "rows".into(),
            Value::List(vec![Value::Record(BTreeMap::from([(
                "id".into(),
                Value::Int(1),
            )]))]),
        )]),
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_CONFIG");
    assert!(engine.execute("from items").rows.is_empty());

    let prepared = engine.prepare("insert items $row").unwrap();
    let response = engine.execute_prepared(
        &prepared,
        BTreeMap::from([(
            "row".into(),
            Value::Record(BTreeMap::from([("id".into(), Value::Int(1))])),
        )]),
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_CONFIG");
    assert!(engine.execute("from items").rows.is_empty());
}

#[test]
fn prepared_queries_infer_parameters_through_local_functions() {
    let mut engine = setup();
    let prepared = engine
        .prepare(
            "from tasks\nlet after = (value int) -> value >= $minimum\nfilter after id\nselect {id}",
        )
        .unwrap();
    assert_eq!(prepared.parameters(), &["minimum"]);
    assert_eq!(prepared.parameter_types()["minimum"], "int");
    let response = engine.query(
        &prepared,
        BTreeMap::from([("minimum".into(), Value::Int(9_007_199_254_740_993))]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn expired_execution_deadline_discards_an_atomic_batch() {
    let mut engine = Engine::memory();
    let response = engine.execute_with_params_until(
        "create table discarded (id int)",
        BTreeMap::new(),
        None,
        std::time::Instant::now(),
    );
    assert_eq!(response.error.unwrap().code, "E_TIMEOUT");
    assert_eq!(
        engine.execute("from discarded").error.unwrap().code,
        "E_TABLE"
    );
}

#[test]
fn row_parameters_support_atomic_insert_and_upsert_batches() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                r#"type Item =
  id int
  note text
table items Item
  key id"#,
            )
            .ok
    );
    let row = Value::Record(BTreeMap::from([
        ("id".into(), Value::Int(1)),
        ("note".into(), Value::Text("first".into())),
    ]));
    let response = engine.execute_with_params(
        "insert items $row\nfrom items | filter id == $id",
        BTreeMap::from([("row".into(), row), ("id".into(), Value::Int(1))]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);

    let replacement = Value::Record(BTreeMap::from([
        ("id".into(), Value::Int(1)),
        ("note".into(), Value::Text("replaced".into())),
    ]));
    let response = engine.execute_with_params(
        "upsert items $row",
        BTreeMap::from([("row".into(), replacement)]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.upsert_action, Some(unionid::UpsertAction::Updated));
}

#[test]
fn prepared_bulk_upsert_infers_row_lists_actions_and_deadlines() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "type Item =\n  id int\n  note text\ntable items Item\n  key id\ninsert items {id = 1, note = \"old\"}"
            )
            .ok
    );
    let row = |id, note: &str| {
        Value::Record(BTreeMap::from([
            ("id".into(), Value::Int(id)),
            ("note".into(), Value::Text(note.into())),
        ]))
    };
    let prepared = engine
        .prepare("upsert many items $rows\nreturning id, note")
        .unwrap();
    assert_eq!(prepared.parameter_types()["rows"], "list Item");

    let values = Value::List(vec![row(1, "updated"), row(2, "inserted")]);
    let response =
        engine.execute_prepared(&prepared, BTreeMap::from([("rows".into(), values.clone())]));
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.upsert_actions,
        [
            unionid::UpsertAction::Updated,
            unionid::UpsertAction::Inserted
        ]
    );
    assert_eq!(response.rows.len(), 2);

    let wire = Response::from_query("bulk", response);
    let json = serde_json::to_value(&wire).unwrap();
    assert_eq!(
        json["upsert_actions"],
        serde_json::json!(["updated", "inserted"])
    );
    assert!(json.get("upsert_action").is_none());
    assert_eq!(
        serde_json::from_value::<Response>(json)
            .unwrap()
            .upsert_actions,
        [
            unionid::UpsertAction::Updated,
            unionid::UpsertAction::Inserted
        ]
    );
    let legacy: Response = serde_json::from_value(serde_json::json!({
        "version": VERSION,
        "request_id": "legacy",
        "ok": true,
        "message": "updated one row",
        "columns": [],
        "rows": [],
        "upsert_action": "updated"
    }))
    .unwrap();
    assert!(legacy.upsert_actions.is_empty());

    let expired = engine.execute_prepared_until(
        &prepared,
        BTreeMap::from([("rows".into(), values)]),
        Instant::now() - Duration::from_millis(1),
    );
    assert!(!expired.ok);
    assert_eq!(expired.error.unwrap().code, "E_TIMEOUT");
    assert_eq!(engine.execute("from items").rows.len(), 2);
}

#[test]
fn parameterized_rows_commit_and_reopen_through_redb() {
    let temp = TempDir::new();
    let path = temp.0.join("data.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(
            engine
                .execute("type Item =\n  id int\ntable items Item\n  key id")
                .ok
        );
        let row = Value::Record(BTreeMap::from([("id".into(), Value::Int(i64::MAX))]));
        let response =
            engine.execute_with_params("insert items $row", BTreeMap::from([("row".into(), row)]));
        assert!(response.ok, "{}", response.message);
        let prepared = engine
            .prepare("insert many items $rows\nreturning id")
            .unwrap();
        let response = engine.execute_prepared(
            &prepared,
            BTreeMap::from([(
                "rows".into(),
                Value::List(vec![
                    Value::Record(BTreeMap::from([("id".into(), Value::Int(2))])),
                    Value::Record(BTreeMap::from([("id".into(), Value::Int(1))])),
                ]),
            )]),
        );
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.affected_rows, Some(2));
        assert!(response.rows[0]["id"].cmp_eq(&Value::Int(2)));
        assert!(response.rows[1]["id"].cmp_eq(&Value::Int(1)));

        let delete = engine
            .prepare("delete items\nfilter id == $id\nreturning id")
            .unwrap();
        let response =
            engine.execute_prepared(&delete, BTreeMap::from([("id".into(), Value::Int(1))]));
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.affected_rows, Some(1));
        assert!(response.rows[0]["id"].cmp_eq(&Value::Int(1)));
    }
    let mut reopened = Engine::open_redb(&path).unwrap();
    let response = reopened.execute("from items | sort id");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 2);
    assert!(response.rows[0]["id"].cmp_eq(&Value::Int(2)));
    assert!(response.rows[1]["id"].cmp_eq(&Value::Int(i64::MAX)));
    assert!(reopened.check_integrity().unwrap().backend_clean);
}

#[test]
fn wire_values_round_trip_losslessly_and_keep_null_distinct() {
    let value = Value::Named {
        type_id: u64::MAX,
        value: Box::new(Value::Record(BTreeMap::from([
            ("large".into(), Value::Int(i64::MIN)),
            ("null".into(), Value::Null),
            ("none".into(), Value::Option(None)),
            (
                "some".into(),
                Value::Option(Some(Box::new(Value::Tuple(vec![
                    Value::Float(1.25),
                    Value::List(vec![Value::Bool(true)]),
                ])))),
            ),
        ]))),
    };
    let wire = WireValue::from(&value);
    let json = serde_json::to_value(&wire).unwrap();
    assert_eq!(json["type_id"], u64::MAX.to_string());
    assert!(Value::try_from(wire).unwrap().cmp_eq(&value));

    let first = Value::Named {
        type_id: 1,
        value: Box::new(Value::Enum(unionid::model::EnumValue {
            variant: "Same".into(),
            args: Vec::new(),
            id: 10,
        })),
    };
    let second = Value::Named {
        type_id: 2,
        value: Box::new(Value::Enum(unionid::model::EnumValue {
            variant: "Same".into(),
            args: Vec::new(),
            id: 20,
        })),
    };
    assert_ne!(WireValue::from(&first), WireValue::from(&second));
}

#[test]
fn protocol_response_uses_wire_values_and_echoes_request_id() {
    let mut engine = setup();
    let request = Request {
        version: VERSION,
        request_id: "req-42".into(),
        query: "from tasks | filter id == $id".into(),
        introspect: None,
        params: BTreeMap::from([(
            "id".into(),
            WireValue::Int {
                value: "9007199254740993".into(),
            },
        )]),
        schema: Some(engine.schema_info()),
    };
    let response = engine.execute_with_params_at_schema(
        &request.query,
        request.decode_params().unwrap(),
        request.schema.as_ref(),
    );
    let response = Response::from_query(request.request_id, response);
    assert!(response.ok);
    assert_eq!(response.request_id, "req-42");
    assert!(matches!(
        response.rows[0].get("id"),
        Some(WireValue::Int { value }) if value == "9007199254740993"
    ));
}

#[test]
fn version_one_introspection_request_and_response_round_trip() {
    let engine = setup();
    let request = Request::introspection("inspect-1", IntrospectionKind::Types);
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["version"], VERSION);
    assert_eq!(encoded["query"], "");
    assert_eq!(encoded["introspect"], "types");
    assert!(encoded.get("params").is_some());

    let introspection = engine.introspection();
    assert_eq!(introspection.storage, StorageMode::Memory);
    assert_eq!(introspection.tables, ["tasks"]);
    assert_eq!(introspection.types, ["State", "Task"]);
    assert!(introspection.fields.contains(&"title".into()));
    assert_eq!(introspection.migration_count, 0);
    assert_eq!(introspection.migration_head, None);

    let response = Response::from_introspection(request.request_id, introspection.clone());
    let decoded: Response =
        serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
    assert!(decoded.ok);
    assert_eq!(decoded.introspection, Some(introspection));

    let legacy_v1: Request = serde_json::from_str(
        r#"{"version":1,"request_id":"old-v1","query":"from tasks","params":{}}"#,
    )
    .unwrap();
    assert_eq!(legacy_v1.introspect, None);
}
