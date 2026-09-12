#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unionid::scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid};
use unionid::{Engine, QueryAccessKind, SchemaBuilder, Value};
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
#[unionid(table = "priced_items", key = "id")]
struct PricedItem {
    id: i64,
    #[unionid(decimal = "5 2")]
    amount: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
enum QueuePayload {
    Sync { source: String, target: String },
    Webhook { url: String, body: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
enum QueueState {
    Queued {
        #[unionid(default = "0")]
        attempt: i64,
    },
    Running {
        worker: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
#[unionid(table = "queue_jobs", key = "id")]
struct QueueJob {
    id: String,
    #[unionid(unique)]
    external_id: String,
    #[unionid(default = "0")]
    priority: i64,
    #[unionid(default = "[]")]
    tags: Vec<String>,
    payload: QueuePayload,
    #[unionid(index)]
    state: QueueState,
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
fn decimal_precision_is_enforced_by_the_schema_boundary() {
    let source = SchemaBuilder::new()
        .add::<PricedItem>()
        .unwrap()
        .build()
        .unwrap();
    let mut engine = Engine::memory();
    assert!(engine.execute(&source).ok);
    let insert = engine
        .prepare("insert priced_items $item\nreturning")
        .unwrap();

    let accepted = PricedItem {
        id: 1,
        amount: Decimal::parse("999.99", 5, 2).unwrap(),
    };
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([("item".into(), Value::from_serde(&accepted).unwrap())]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.typed_rows::<PricedItem>().unwrap(), [accepted]);

    let too_wide = PricedItem {
        id: 2,
        amount: Decimal::parse("1000.00", 6, 2).unwrap(),
    };
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([("item".into(), Value::from_serde(&too_wide).unwrap())]),
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_DECIMAL_RANGE");
}

#[test]
fn real_job_queue_model_preserves_defaults_indexes_and_typed_values() {
    let source = SchemaBuilder::new()
        // Deliberately register the row first: dependency ordering still makes
        // this source executable.
        .add::<QueueJob>()
        .unwrap()
        .add::<QueueState>()
        .unwrap()
        .add::<QueuePayload>()
        .unwrap()
        .build()
        .unwrap();
    assert!(source.contains("attempt int = 0"), "{source}");
    assert!(source.contains("priority int = 0"), "{source}");
    assert!(source.contains("tags list (text) = []"), "{source}");
    assert!(
        source.contains("create unique index queue_jobs (external_id)"),
        "{source}"
    );
    assert!(
        source.contains("create index queue_jobs (state)"),
        "{source}"
    );

    let mut engine = Engine::memory();
    let created = engine.execute(&source);
    assert!(created.ok, "{}", created.message);

    // Schema defaults belong to insert input semantics. The full Rust row still
    // contains every field after the database fills omitted values.
    let inserted = engine.execute(
        r#"insert queue_jobs {
  id = "job-1",
  external_id = "incoming-1",
  payload = Sync {source = "inbox", target = "archive"},
  state = Queued {},
}
returning"#,
    );
    assert!(inserted.ok, "{}", inserted.message);
    assert_eq!(
        inserted.typed_rows::<QueueJob>().unwrap(),
        [QueueJob {
            id: "job-1".into(),
            external_id: "incoming-1".into(),
            priority: 0,
            tags: Vec::new(),
            payload: QueuePayload::Sync {
                source: "inbox".into(),
                target: "archive".into(),
            },
            state: QueueState::Queued { attempt: 0 },
        }]
    );

    let job = QueueJob {
        id: "job-2".into(),
        external_id: "incoming-2".into(),
        priority: 20,
        tags: vec!["webhook".into(), "urgent".into()],
        payload: QueuePayload::Webhook {
            url: "https://example.test/hook".into(),
            body: "{}".into(),
        },
        state: QueueState::Running {
            worker: "worker-1".into(),
        },
    };
    let insert = engine.prepare("insert queue_jobs $job\nreturning").unwrap();
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([("job".into(), Value::from_serde(&job).unwrap())]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.typed_rows::<QueueJob>().unwrap(), [job]);

    let plan = engine
        .execute(r#"explain from queue_jobs | filter external_id == "incoming-2""#)
        .plan
        .unwrap();
    assert_eq!(plan.access.kind, QueryAccessKind::SecondaryIndexLookup);

    let duplicate = engine.execute(
        r#"insert queue_jobs {
  id = "job-3",
  external_id = "incoming-2",
  payload = Sync {source = "a", target = "b"},
  state = Failed {reason = "duplicate"},
}"#,
    );
    assert!(!duplicate.ok);
    assert_eq!(duplicate.error.unwrap().code, "E_CONSTRAINT");
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

#[derive(UnionidSchema)]
struct InvalidDefault {
    #[unionid(default = "Some(")]
    value: Option<i64>,
}

#[derive(UnionidSchema)]
struct InvalidTypedDefault {
    #[unionid(default = "\"not an int\"")]
    value: i64,
}

#[test]
fn build_validates_generated_default_expressions() {
    let error = SchemaBuilder::new()
        .add::<InvalidDefault>()
        .unwrap()
        .build()
        .unwrap_err();
    assert_eq!(error.code, "E_SYNTAX");

    let error = SchemaBuilder::new()
        .add::<InvalidTypedDefault>()
        .unwrap()
        .build()
        .unwrap_err();
    assert_eq!(error.code, "E_TYPE");
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", rename_all_fields = "camelCase")]
enum RenamedState {
    InProgress {
        attempt_count: i64,
    },
    #[serde(rename = "FINISHED")]
    Done {
        result_text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, UnionidSchema)]
#[serde(rename_all = "camelCase")]
#[unionid(table = "renamed_jobs", key = "job_id")]
struct RenamedJob {
    job_id: i64,
    display_name: String,
    #[serde(rename = "externalCode")]
    external_code: String,
    #[unionid(index)]
    current_state: RenamedState,
}

#[test]
fn serde_renames_define_the_schema_and_primary_key_names() {
    let source = SchemaBuilder::new()
        .add::<RenamedState>()
        .unwrap()
        .add::<RenamedJob>()
        .unwrap()
        .build()
        .unwrap();
    assert!(
        source.contains("IN_PROGRESS {attemptCount int}"),
        "{source}"
    );
    assert!(source.contains("FINISHED {resultText text}"), "{source}");
    assert!(source.contains("jobId int"), "{source}");
    assert!(source.contains("displayName text"), "{source}");
    assert!(source.contains("externalCode text"), "{source}");
    assert!(source.contains("currentState RenamedState"), "{source}");
    assert!(source.contains("key jobId"), "{source}");
    assert!(
        source.contains("create index renamed_jobs (currentState)"),
        "{source}"
    );

    let row = RenamedJob {
        job_id: 7,
        display_name: "serde-aligned".into(),
        external_code: "ext-7".into(),
        current_state: RenamedState::InProgress { attempt_count: 2 },
    };
    let mut engine = Engine::memory();
    let response = engine.execute(&source);
    assert!(response.ok, "{}\n{source}", response.message);
    let insert = engine
        .prepare("insert renamed_jobs $row\nreturning")
        .unwrap();
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([("row".into(), Value::from_serde(&row).unwrap())]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.typed_rows::<RenamedJob>().unwrap(), [row]);
}
