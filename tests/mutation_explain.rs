mod common;

use std::collections::BTreeMap;
use unionid::{Engine, QueryOperation, QueryReferenceCheckKind, QueryResponse, Value};

const SCHEMA: &str = "struct Node {id: int, parent: Option<int>}\ntable nodes: Node {key id}\ncreate reference nodes (parent) references nodes (id)\ninsert many nodes [{id: 1, parent: Some(1)}, {id: 2, parent: Some(1)}]";

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{source}: {}", response.message);
    response
}

fn state(engine: &mut Engine) -> serde_json::Value {
    serde_json::json!({
        "schema": engine.schema_info(),
        "rows": ok(engine, "from nodes | sort id").rows,
        "ledger": engine.migration_history(),
    })
}

#[test]
fn all_mutations_plan_without_enforcing_data_constraints_or_writing() {
    use QueryOperation::{Delete, Insert, InsertMany, Update, Upsert, UpsertMany};
    use QueryReferenceCheckKind::{Restrict, TargetExists};
    for durable in [false, true] {
        let dir = common::TempDir::new();
        let path = dir.0.join("plans.redb");
        let mut engine = if durable {
            let mut engine = Engine::open_redb(&path).unwrap();
            engine.upgrade_storage(12).unwrap();
            engine
        } else {
            Engine::memory()
        };
        ok(&mut engine, SCHEMA);
        if durable {
            drop(engine);
            engine = Engine::open_redb_read_only(&path).unwrap();
        }
        let before = state(&mut engine);
        let bytes = durable.then(|| std::fs::read(&path).unwrap());
        let read_access = ok(&mut engine, "explain from nodes | filter id == 1")
            .plan
            .unwrap()
            .access;
        for (source, operation, rows, checks) in [
            (
                "explain insert nodes {id: 1, parent: Some(999)} | returning {id}",
                Insert,
                Some(1),
                vec![TargetExists],
            ),
            (
                "explain insert many nodes [{id: 1, parent: None}, {id: 1, parent: None}]",
                InsertMany,
                Some(2),
                vec![TargetExists],
            ),
            (
                "explain upsert nodes {id: 1, parent: Some(999)}",
                Upsert,
                Some(1),
                vec![TargetExists, Restrict],
            ),
            (
                "explain upsert many nodes [{id: 1, parent: None}, {id: 1, parent: None}]",
                UpsertMany,
                Some(2),
                vec![TargetExists, Restrict],
            ),
            (
                "explain update nodes | filter id == 1 | set id = 3 | returning {id}",
                Update,
                None,
                vec![TargetExists, Restrict],
            ),
            (
                "explain delete nodes | filter id == 1 | returning {id}",
                Delete,
                None,
                vec![Restrict],
            ),
        ] {
            let canonical = unionid::format_source(source).unwrap();
            assert_eq!(
                unionid::format_source(&canonical)
                    .unwrap_or_else(|error| panic!("{canonical}: {error}")),
                canonical
            );
            let response = ok(&mut engine, &canonical);
            assert!(response.rows.is_empty());
            assert!(response.columns.is_empty());
            assert!(response.affected_rows.is_none());
            assert!(engine.last_mutation_profile().is_none());
            let metadata = response.mutation_plan.unwrap();
            assert_eq!(metadata.operation, operation);
            assert_eq!(metadata.table, "nodes");
            assert_eq!(metadata.input_rows, rows);
            assert_eq!(
                metadata
                    .reference_checks
                    .iter()
                    .map(|check| check.kind)
                    .collect::<Vec<_>>(),
                checks
            );
            if matches!(operation, Update | Delete) {
                let plan = response.plan.unwrap();
                assert_eq!(plan.access.kind, unionid::QueryAccessKind::PrimaryKeyLookup);
                assert_eq!(plan.access, read_access);
                assert_eq!(plan.result_schema, metadata.returning_schema);
            } else {
                assert!(response.plan.is_none());
            }
            assert_eq!(state(&mut engine), before);
        }
        if let Some(bytes) = bytes {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn prepared_explain_binds_nested_parameters_and_static_contracts() {
    let mut engine = Engine::memory();
    ok(&mut engine, SCHEMA);
    let before = state(&mut engine);
    for source in [
        "explain insert nodes $row",
        "explain upsert nodes $row",
        "explain insert many nodes $rows",
        "explain upsert many nodes $rows",
    ] {
        let prepared = engine.prepare(source).unwrap();
        let row = Value::Record(BTreeMap::from([
            ("id".into(), Value::Int(1)),
            (
                "parent".into(),
                Value::Option(Some(Box::new(Value::Int(999)))),
            ),
        ]));
        let (name, value) = if source.contains("many") {
            ("rows", Value::List(vec![row]))
        } else {
            ("row", row)
        };
        let response = engine.execute_prepared(&prepared, BTreeMap::from([(name.into(), value)]));
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.mutation_plan.unwrap().input_rows, Some(1));
        let description = engine.describe_query(source).unwrap();
        assert_eq!(description.operation, QueryOperation::Explain);
        assert!(!description.result.affected_rows);
        assert!(description.result.fields.is_empty());
        assert!(!description.reference_checks.is_empty());
    }
    let source = "explain update nodes | filter id == $id | set parent = $parent | returning {id}";
    let prepared = engine.prepare(source).unwrap();
    let response = engine.execute_prepared(
        &prepared,
        BTreeMap::from([
            ("id".into(), Value::Int(1)),
            ("parent".into(), Value::Option(None)),
        ]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.mutation_plan.unwrap().updated_fields,
        vec!["parent"]
    );
    assert_eq!(state(&mut engine), before);
}

#[test]
fn explain_rejects_bad_structure_but_remains_a_read_request() {
    let mut engine = Engine::memory();
    ok(&mut engine, SCHEMA);
    let before = state(&mut engine);
    for source in [
        "explain insert nodes {id: \"wrong\", parent: None}",
        "explain update nodes | set missing = 1",
        "explain delete nodes | select {id}",
        "explain analyze delete nodes",
        "explain create table other (id int)",
        "explain delete nodes\nexpect affected == 1",
    ] {
        assert!(!engine.execute(source).ok, "accepted {source}");
        assert_eq!(state(&mut engine), before);
    }
    let concurrent = unionid::ConcurrentEngine::new(engine);
    let response = concurrent.execute("explain delete nodes | filter id == 1");
    assert!(response.ok, "{}", response.message);
    assert!(response.mutation_plan.is_some());
    for version in [1, 2] {
        let mut request = unionid::ProtocolRequest::query("plan", "explain delete nodes");
        request.version = version;
        let response = concurrent.execute_protocol_request(request);
        assert!(response.ok, "{}", response.message);
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["mutation_plan"]["operation"], "delete");
        let decoded: unionid::ProtocolResponse = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.mutation_plan, response.mutation_plan);
    }
    let request = unionid::ProtocolRequest::query("keyed-plan", "explain delete nodes")
        .with_idempotency_key("must-not-allocate")
        .unwrap();
    let response = concurrent.execute_protocol_request(request);
    assert_eq!(response.error.unwrap().code, "E_IDEMPOTENCY_NOT_MUTATION");
}

#[test]
fn a_write_script_retains_its_final_plan_in_durable_receipts_and_backups() {
    let dir = common::TempDir::new();
    let path = dir.0.join("receipt.redb");
    let archive = dir.0.join("receipt.json");
    let restored = dir.0.join("restored.redb");
    let request = unionid::ProtocolRequest::query(
        "write-and-plan",
        "insert nodes {id: 3, parent: None}\nexplain delete nodes | filter id == 1",
    )
    .with_idempotency_key("write-and-plan")
    .unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(12).unwrap();
    ok(&mut engine, SCHEMA);
    let first = unionid::server::execute_protocol_request(&mut engine, request.clone());
    assert!(first.ok, "{}", first.message);
    assert!(first.mutation_plan.is_some());
    assert!(!first.idempotency.as_ref().unwrap().replayed);
    let expected = state(&mut engine);
    drop(engine);
    unionid::backup::create(&path, &archive).unwrap();
    unionid::backup::restore(&archive, &restored).unwrap();
    for database in [&path, &restored] {
        let mut engine = Engine::open_redb(database).unwrap();
        let replay = unionid::server::execute_protocol_request(&mut engine, request.clone());
        assert!(replay.ok, "{}", replay.message);
        assert!(replay.idempotency.unwrap().replayed);
        assert_eq!(replay.mutation_plan, first.mutation_plan);
        assert_eq!(
            serde_json::to_value(&replay.plan).unwrap(),
            serde_json::to_value(&first.plan).unwrap()
        );
        assert_eq!(replay.statements, first.statements);
        assert_eq!(state(&mut engine), expected);
        engine.check_integrity().unwrap();
    }
}
