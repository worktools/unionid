mod common;

use common::TempDir;
use serde::{Deserialize, Serialize};
use unionid::{Engine, Value};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct User {
    id: i64,
    name: String,
    email: String,
    note: String,
}

const SCHEMA: &str = "\
type User = {
  id int,
  name text,
  email text,
  note text,
}
table users User
  key id
create unique index users (email)
create index users (name)
insert users { id = 1, name = \"alice\", email = \"a@x\", note = \"n1\" }
insert users { id = 2, name = \"bob\", email = \"b@x\", note = \"n2\" }
insert users { id = 3, name = \"alice\", email = \"c@x\", note = \"n3\" }";

fn engine() -> Engine {
    let mut engine = Engine::memory();
    let response = engine.execute(SCHEMA);
    assert!(response.ok, "{}", response.message);
    engine
}

fn id_of(row: &Option<std::collections::BTreeMap<String, Value>>) -> Option<i64> {
    row.as_ref().and_then(|row| match row.get("id") {
        Some(Value::Int(id)) => Some(*id),
        _ => None,
    })
}

#[test]
fn fetch_by_key_preserves_input_order_and_missing_keys() {
    let mut engine = engine();
    let rows = engine
        .fetch_by_key(
            "users",
            "id",
            &[Value::Int(3), Value::Int(99), Value::Int(1)],
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(id_of(&rows[0]), Some(3));
    assert!(rows[1].is_none());
    assert_eq!(id_of(&rows[2]), Some(1));
}

#[test]
fn typed_fetch_by_key_decodes_application_types() {
    let mut engine = engine();
    let rows = engine
        .typed_fetch_by_key::<User>("users", "id", &[Value::Int(2), Value::Int(42)])
        .unwrap();
    assert_eq!(
        rows[0],
        Some(User {
            id: 2,
            name: "bob".into(),
            email: "b@x".into(),
            note: "n2".into(),
        })
    );
    assert_eq!(rows[1], None);
}

#[test]
fn fetch_by_key_uses_a_unique_secondary_index() {
    let mut engine = engine();
    let rows = engine
        .fetch_by_key("users", "email", &[Value::Text("c@x".into())])
        .unwrap();
    assert_eq!(id_of(&rows[0]), Some(3));
}

#[test]
fn fetch_by_key_rejects_non_unique_matches() {
    let mut engine = engine();
    let error = engine
        .fetch_by_key("users", "name", &[Value::Text("alice".into())])
        .unwrap_err();
    assert_eq!(error.code, "E_RELATION_NOT_UNIQUE");
}

#[test]
fn fetch_by_key_requires_an_indexed_key_column() {
    let mut engine = engine();
    let error = engine
        .fetch_by_key("users", "note", &[Value::Text("n1".into())])
        .unwrap_err();
    assert_eq!(error.code, "E_RELATION_KEY");
}

#[test]
fn fetch_by_key_matches_manual_single_key_queries() {
    let mut engine = engine();
    let fetched = engine
        .fetch_by_key("users", "id", &[Value::Int(1), Value::Int(2)])
        .unwrap();
    let first = engine.execute("from users | filter id == 1 | take 1");
    assert!(first.ok, "{}", first.message);
    let second = engine.execute("from users | filter id == 2 | take 1");
    assert!(second.ok, "{}", second.message);
    assert_eq!(
        serde_json::to_value(fetched[0].as_ref()).unwrap(),
        serde_json::to_value(first.rows.first()).unwrap()
    );
    assert_eq!(
        serde_json::to_value(fetched[1].as_ref()).unwrap(),
        serde_json::to_value(second.rows.first()).unwrap()
    );
}

#[test]
fn fetch_by_key_is_bounded() {
    let mut engine = engine();
    let keys = (0..=Engine::MAX_BATCH_KEYS)
        .map(|id| Value::Int(id as i64))
        .collect::<Vec<_>>();
    let error = engine.fetch_by_key("users", "id", &keys).unwrap_err();
    assert_eq!(error.code, "E_LIMIT");
}

#[test]
fn fetch_by_key_works_on_a_durable_database() {
    let dir = TempDir::new();
    let path = dir.0.join("relational.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    assert!(engine.execute(SCHEMA).ok);
    let rows = engine
        .typed_fetch_by_key::<User>("users", "id", &[Value::Int(2)])
        .unwrap();
    assert_eq!(rows[0].as_ref().map(|user| user.id), Some(2));
    drop(engine);
    // Reopen from disk and fetch again over the recovered snapshot.
    let mut engine = Engine::open_redb(&path).unwrap();
    let rows = engine
        .fetch_by_key("users", "id", &[Value::Int(2), Value::Int(7)])
        .unwrap();
    assert_eq!(id_of(&rows[0]), Some(2));
    assert!(rows[1].is_none());
}
