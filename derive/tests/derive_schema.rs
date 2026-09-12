#![allow(dead_code)]

use unionid::scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid};
use unionid::{Engine, SchemaBuilder};
use unionid_derive::UnionidSchema;

#[derive(UnionidSchema)]
struct Contact {
    email: String,
    nickname: Option<String>,
}

#[derive(UnionidSchema)]
enum State {
    Pending,
    Running { worker: String, attempt: i64 },
    Failed(String),
    Done(String, String),
}

#[derive(UnionidSchema)]
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
        .add::<State>()
        .add::<Task>()
        .add::<Event>()
        .build()
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
    let once = SchemaBuilder::new().add::<Contact>().build();
    let twice = SchemaBuilder::new()
        .add::<Contact>()
        .add::<Contact>()
        .build();
    assert_eq!(once, twice);
    assert_eq!(twice.matches("type Contact =").count(), 1);
}
