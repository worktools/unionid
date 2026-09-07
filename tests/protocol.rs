mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::TempDir;
use unionid::protocol::{Request, Response, VERSION, WireValue};
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
        engine.prepare("delete tasks").unwrap_err().code,
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
    }
    let mut reopened = Engine::open_redb(&path).unwrap();
    let response = reopened.execute("from items | sort id");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 3);
    assert!(response.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(response.rows[1]["id"].cmp_eq(&Value::Int(2)));
    assert!(response.rows[2]["id"].cmp_eq(&Value::Int(i64::MAX)));
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
