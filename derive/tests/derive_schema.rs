#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unionid::scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid};
use unionid::{Engine, SchemaBuilder, Value};
use unionid_derive::UnionidSchema;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
struct Contact {
    email: String,
    nickname: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
enum State {
    Pending,
    Running { worker: String, attempt: i64 },
    Failed(String),
    Done(String, String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
#[unionid(table = "tasks", key = "id")]
struct Task {
    id: i64,
    title: String,
    owner: Contact,
    tags: Vec<String>,
    state: State,
    parent: Option<Box<Task>>,
}

#[derive(UnionidSchema)]
struct Event {
    id: Uuid,
    at: Timestamp,
    on: Date,
    took: Duration,
    #[unionid(decimal = "12 2")]
    amount: Decimal,
    blob: Bytes,
    flags: Vec<bool>,
}

fn schema() -> String {
    SchemaBuilder::new()
        .add::<Contact>()
        .unwrap()
        .add::<State>()
        .unwrap()
        .add::<Task>()
        .unwrap()
        .add::<Event>()
        .unwrap()
        .build()
        .unwrap()
}

#[test]
fn derives_type_and_table_declarations() {
    let source = schema();
    assert!(source.contains("type Contact = {"));
    assert!(source.contains("  nickname option (text),"));
    assert!(source.contains("type State ="));
    assert!(source.contains("  Pending"));
    assert!(source.contains("  | Running {worker text, attempt int}"));
    assert!(source.contains("  | Failed text"));
    assert!(source.contains("  | Done (text, text)"));
    assert!(source.contains("  parent option (Task),"));
    assert!(source.contains("table tasks Task"));
    assert!(source.contains("  key id"));
    assert!(source.contains("type Event = {"));
    assert!(source.contains("  amount decimal 12 2,"));
}

#[test]
fn derived_schema_validates_and_round_trips() {
    let source = schema();
    let checked = Engine::check_schema(&source).unwrap();
    assert!(!checked.normalized.is_empty());
    // The forward generator must accept the derived schema too.
    let rust = unionid::codegen::rust(&source).unwrap();
    assert!(rust.contains("pub struct Task {"));
    assert!(rust.contains("pub enum State {"));
    assert!(rust.contains("pub amount: unionid::scalars::Decimal,"));
}

#[test]
fn schema_executes_in_an_engine() {
    let mut engine = Engine::memory();
    let response = engine.execute(&schema());
    assert!(response.ok, "{}", response.message);
    assert!(engine.execute("from tasks").ok);
}

#[test]
fn add_deduplicates_types() {
    let once = SchemaBuilder::new()
        .add::<Contact>()
        .unwrap()
        .build()
        .unwrap();
    let twice = SchemaBuilder::new()
        .add::<Contact>()
        .unwrap()
        .add::<Contact>()
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(once, twice);
    assert_eq!(twice.matches("type Contact =").count(), 1);
}

mod first {
    use unionid_derive::UnionidSchema;

    #[derive(UnionidSchema)]
    pub struct User {
        pub id: i64,
    }
}

mod second {
    use unionid_derive::UnionidSchema;

    #[derive(UnionidSchema)]
    pub struct User {
        pub id: i64,
        pub label: String,
    }
}

#[test]
fn add_rejects_conflicting_names() {
    let error = SchemaBuilder::new()
        .add::<first::User>()
        .unwrap()
        .add::<second::User>()
        .unwrap_err();
    assert_eq!(error.code, "E_SCHEMA");
    assert!(error.message.contains("conflicting"));
    // Identical duplicates are still accepted.
    assert!(
        SchemaBuilder::new()
            .add::<first::User>()
            .unwrap()
            .add::<first::User>()
            .is_ok()
    );
}

#[test]
fn inserts_and_reads_derived_values() {
    let mut engine = Engine::memory();
    assert!(engine.execute(&schema()).ok, "schema must execute");

    let task = Task {
        id: 1,
        title: "ship".into(),
        owner: Contact {
            email: "a@example.com".into(),
            nickname: Some("A".into()),
        },
        tags: vec!["x".into(), "y".into()],
        state: State::Running {
            worker: "w".into(),
            attempt: 2,
        },
        parent: None,
    };
    let insert = engine.prepare("insert tasks $row\nreturning").unwrap();
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([("row".into(), Value::from_serde(&task).unwrap())]),
    );
    assert!(response.ok, "{}", response.message);
    let rows = response.typed_rows::<Task>().unwrap();
    assert_eq!(rows.as_slice(), std::slice::from_ref(&task));

    let read = engine.prepare("from tasks | filter id == 1").unwrap();
    let response = engine.execute_prepared(&read, BTreeMap::new());
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.typed_rows::<Task>().unwrap(), [task]);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
enum TaggedState {
    Queued,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
#[unionid(table = "tagged_jobs", key = "id")]
struct TaggedJob {
    id: i64,
    state: TaggedState,
}

#[test]
fn builds_dependency_ordered_schema_regardless_of_add_order() {
    // The referencing type sorts before the referenced one; registration order
    // must not matter.
    let source = SchemaBuilder::new()
        .add::<TaggedJob>()
        .unwrap()
        .add::<TaggedState>()
        .unwrap()
        .build()
        .unwrap();
    let state = source.find("type TaggedState").unwrap();
    let job = source.find("type TaggedJob").unwrap();
    assert!(
        state < job,
        "referenced type must be declared first:\n{source}"
    );

    let mut engine = Engine::memory();
    let response = engine.execute(&source);
    assert!(response.ok, "{}", response.message);
    let row = TaggedJob {
        id: 1,
        state: TaggedState::Queued,
    };
    let insert = engine
        .prepare("insert tagged_jobs $row\nreturning")
        .unwrap();
    let response = engine.execute_prepared(
        &insert,
        std::collections::BTreeMap::from([("row".into(), Value::from_serde(&row).unwrap())]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.typed_rows::<TaggedJob>().unwrap(), [row]);
}

#[test]
fn build_reports_missing_dependencies() {
    let error = SchemaBuilder::new()
        .add::<TaggedJob>()
        .unwrap()
        .build()
        .unwrap_err();
    assert_eq!(error.code, "E_SCHEMA");
    assert!(error.message.contains("TaggedState"), "{}", error.message);
}

#[derive(UnionidSchema)]
enum Chain {
    End,
    Link(Box<Chain>),
}

#[test]
fn build_allows_direct_self_recursion() {
    let source = SchemaBuilder::new()
        .add::<Chain>()
        .unwrap()
        .build()
        .unwrap();
    let checked = Engine::check_schema(&source).unwrap();
    assert!(!checked.normalized.is_empty());
}

#[derive(UnionidSchema)]
enum MutualA {
    B(Box<MutualB>),
}

#[derive(UnionidSchema)]
enum MutualB {
    A(Box<MutualA>),
}

#[test]
fn build_rejects_mutually_recursive_types() {
    let error = SchemaBuilder::new()
        .add::<MutualA>()
        .unwrap()
        .add::<MutualB>()
        .unwrap()
        .build()
        .unwrap_err();
    assert_eq!(error.code, "E_SCHEMA");
    assert!(error.message.contains("recursive"), "{}", error.message);
}
