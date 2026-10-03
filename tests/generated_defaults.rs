use std::collections::BTreeMap;
use unionid::{Engine, QueryResponse, Value};

const SETUP: &str = r#"sequence ids {start 1}
type Id = int
struct Item {id: Id, owner: text, public_id: uuid, created_at: timestamp}
table items: Item {
  key id
  default id = next(ids)
  default public_id = uuid_v7()
  default created_at = now()
}
create unique index items (owner)
"#;

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{}: {:?}", source, response.error);
    response
}

fn id(response: &QueryResponse, row: usize) -> i64 {
    let Value::Int(id) = response.rows[row]["id"].unwrapped() else {
        panic!("expected int ID")
    };
    *id
}

#[test]
fn candidate_allocations_share_time_and_follow_row_order_without_schema_drift() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let schema = engine.schema_info();
    let batch = ok(
        &mut engine,
        r#"insert many items [{owner: "a"}, {owner: "b"}] | returning {id, public_id, created_at}"#,
    );
    assert_eq!((id(&batch, 0), id(&batch, 1)), (1, 2));
    assert!(batch.rows[0]["created_at"].cmp_eq(&batch.rows[1]["created_at"]));
    assert!(!batch.rows[0]["public_id"].cmp_eq(&batch.rows[1]["public_id"]));
    let script = ok(
        &mut engine,
        r#"insert items {owner: "c"}
insert items {owner: "d"} | returning id"#,
    );
    assert_eq!(id(&script, 0), 4);
    assert_eq!(engine.schema_info(), schema);
    let source = engine.schema();
    assert!(source.contains("next(ids)"));
    assert_eq!(
        unionid::schema::check(&source).unwrap().schema.hash,
        schema.hash
    );
}

#[test]
fn failed_constraints_and_expectations_roll_back_all_allocations() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    for source in [
        r#"insert many items [{owner: "same"}, {owner: "same"}]"#,
        r#"insert items {owner: "a"}
expect affected == 2"#,
        r#"insert items {owner: "a"}
insert items {owner: "a"}"#,
        r#"insert items {owner: "a"}
insert items {owner: 1}"#,
    ] {
        assert!(
            !engine.execute(source).ok,
            "unexpectedly succeeded: {source}"
        );
        assert!(ok(&mut engine, "from items").rows.is_empty());
    }
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "valid"} | returning id"#
            ),
            0
        ),
        1
    );
}

#[test]
fn explicit_values_do_not_allocate_and_upsert_requires_its_key() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    ok(&mut engine, r#"insert items {id: 90, owner: "explicit"}"#);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "generated"} | returning id"#
            ),
            0
        ),
        1
    );
    let error = engine
        .execute(r#"upsert items {owner: "missing-key"}"#)
        .error
        .unwrap();
    assert_eq!(error.code, "E_FIELD");
    let replaced = ok(
        &mut engine,
        r#"upsert items {id: 90, owner: "replaced"} | returning id"#,
    );
    assert_eq!(id(&replaced, 0), 90);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "next"} | returning id"#
            ),
            0
        ),
        2
    );
}

#[test]
fn exhaust_once_and_replay_original_generated_values() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        &SETUP.replace("start 1", &format!("start {}", i64::MAX)),
    );
    let source = r#"insert items {owner: "last"} | returning {id, public_id, created_at}"#;
    let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let first = engine
        .execute_idempotent_with_params("last", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert_eq!(id(&first.response, 0), i64::MAX);
    let replay = engine
        .execute_idempotent_with_params("last", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(
        serde_json::to_value(first.response.rows).unwrap(),
        serde_json::to_value(replay.response.rows).unwrap()
    );
    let failed = engine.execute(r#"insert items {owner: "exhausted"}"#);
    assert_eq!(failed.error.unwrap().code, "E_ARITH");
    assert_eq!(ok(&mut engine, "from items").rows.len(), 1);
}

#[test]
fn binding_rejects_wrong_fields_types_and_sequences_before_publication() {
    for declaration in [
        "default id = next(missing)",
        "default owner = next(ids)",
        "default id = uuid_v7()",
        "default created_at = next(ids)",
        "default missing = now()",
    ] {
        let mut engine = Engine::memory();
        let source = format!(
            "sequence ids {{start 1}}\nstruct Item {{id: int, owner: text, created_at: timestamp}}\ntable items: Item {{{declaration}}}"
        );
        assert!(!engine.execute(&source).ok);
        assert!(engine.schema().is_empty());
    }
}

#[test]
fn prepared_omitted_fields_explain_and_read_only_do_not_consume_sequences() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let prepared = engine
        .prepare("insert items $row | returning {id, public_id, created_at}")
        .unwrap();
    ok(&mut engine, r#"explain insert items {owner: "planned"}"#);
    let parameters = BTreeMap::from([(
        "row".into(),
        Value::Record(BTreeMap::from([(
            "owner".into(),
            Value::Text("first".into()),
        )])),
    )]);
    let mut engine = engine.with_read_only(true);
    let rejected = engine.execute_prepared(&prepared, parameters.clone());
    assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
    let mut engine = engine.with_read_only(false);
    let inserted = engine.execute_prepared(&prepared, parameters);
    assert!(inserted.ok, "{:?}", inserted.error);
    assert_eq!(id(&inserted, 0), 1);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "second"} | returning id"#
            ),
            0
        ),
        2
    );
}
