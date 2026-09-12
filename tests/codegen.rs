mod common;

use std::collections::BTreeMap;
use std::process::Command;

use common::TempDir;
use serde::{Deserialize, Serialize};
use unionid::{Engine, Value, codegen};

#[test]
fn generates_rust_bindings_for_example_schema() {
    let generated = codegen::rust(include_str!("../examples/schema.uid")).unwrap();
    assert!(generated.contains("pub enum State {"));
    assert!(generated.contains("    Pending,"));
    assert!(generated.contains("    Running,"));
    assert!(generated.contains("pub struct Task {"));
    assert!(generated.contains("    pub id: i64,"));
    assert!(generated.contains("    pub title: String,"));
    assert!(generated.contains("    pub state: State,"));
    assert!(generated.contains("    pub priority: i64,"));
    assert!(generated.contains("#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]"));
}

#[test]
fn generates_recursive_and_scalar_types() {
    let source = "\
type Tree =
  Leaf text
  | Branch {
    label text,
    children list Tree,
  }

type Expr =
  Num int
  | Neg Expr
  | Pair(int, int)

type Contact = {
  email text,
  nickname option text,
}

type Ids = (int, text)

type UserId = text

type OrderId = text

type Event = {
  id uuid,
  at timestamp,
  on date,
  took duration,
  amount decimal 12 2,
  blob bytes,
  primary bool,
}

table contacts Contact
  key email
table events Event
  key id
";
    let generated = codegen::rust(source).unwrap();
    // Direct self-recursion must box; recursion through a list must not.
    assert!(generated.contains("Neg(Box<Expr>)"));
    assert!(generated.contains("children: Vec<Tree>"));
    assert!(!generated.contains("Box<Tree>"));
    // Positional payloads stay a Rust tuple variant.
    assert!(generated.contains("Pair(i64, i64)"));
    assert!(generated.contains("pub struct Contact {"));
    assert!(generated.contains("pub nickname: Option<String>,"));
    assert!(generated.contains("pub struct Ids(pub (i64, String));"));
    assert!(generated.contains("pub struct UserId(pub String);"));
    assert!(generated.contains("pub struct OrderId(pub String);"));
    assert!(!generated.contains("pub type Ids"));
    assert!(!generated.contains("pub type UserId"));
    assert_eq!(generated.matches("#[serde(transparent)]").count(), 3);
    for scalar in [
        "unionid::scalars::Uuid",
        "unionid::scalars::Timestamp",
        "unionid::scalars::Date",
        "unionid::scalars::Duration",
        "unionid::scalars::Decimal",
        "unionid::scalars::Bytes",
    ] {
        assert!(generated.contains(scalar), "missing {scalar}");
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct UserId(String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct OrderId(String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct Position((i64, i64));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Assignment {
    id: i64,
    user: UserId,
    order: OrderId,
    position: Position,
}

#[test]
fn generated_newtype_shape_round_trips_nominal_domain_values() {
    let schema = "\
type UserId = text
type OrderId = text
type Position = (int, int)
type Assignment = {
  id int,
  user UserId,
  order OrderId,
  position Position,
}
table assignments Assignment
  key id
";
    let generated = codegen::rust(schema).unwrap();
    assert!(generated.contains("pub struct UserId(pub String);"));
    assert!(generated.contains("pub struct OrderId(pub String);"));
    assert!(generated.contains("pub struct Position(pub (i64, i64));"));

    let assignment = Assignment {
        id: 1,
        user: UserId("user-1".into()),
        order: OrderId("order-1".into()),
        position: Position((4, 9)),
    };
    let mut engine = Engine::memory();
    assert!(engine.execute(schema).ok);
    let insert = engine
        .prepare("insert assignments $assignment\nreturning")
        .unwrap();
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([("assignment".into(), Value::from_serde(&assignment).unwrap())]),
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.typed_rows::<Assignment>().unwrap(), [assignment]);
}

#[test]
fn escapes_rust_keyword_field_names() {
    let source = "\
type T = {
  async int,
  trait text,
  match bool,
  gen int,
}

type State =
  | Waiting {
    match bool,
  }

table t T
  key async
";
    let generated = codegen::rust(source).unwrap();
    assert!(generated.contains("pub async_: i64,"));
    assert!(generated.contains("pub trait_: String,"));
    assert!(generated.contains("pub match_: bool,"));
    assert!(generated.contains("pub gen_: i64,"));
    // Escaped names must keep the original schema name for serde.
    assert!(generated.contains("#[serde(rename = \"async\")]"));
    assert!(generated.contains("#[serde(rename = \"match\")]"));
    assert!(generated.contains("#[serde(rename = \"gen\")]"));
}

#[test]
fn rejects_identifier_collisions_after_escaping() {
    let source = "\
type T = {
  match int,
  match_ int,
}

table t T
  key match
";
    let error = codegen::rust(source).unwrap_err();
    assert_eq!(error.code, "E_SCHEMA");
    assert!(error.message.contains("collides"));
}

#[test]
fn rejects_scripts_that_are_not_schema_files() {
    let error = codegen::rust("type T = {\n  id int,\n}\ntable t T\n  key id\ninsert t { id = 1 }")
        .unwrap_err();
    assert_eq!(error.code, "E_SCHEMA");
}

#[test]
fn cli_generates_from_a_schema_file_and_database() {
    let dir = TempDir::new();
    let schema = dir.0.join("schema.uid");
    std::fs::write(
        &schema,
        "type Item = {\n  id int,\n  label text,\n}\ntable items Item\n  key id\n",
    )
    .unwrap();

    let file_output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["schema", "rust", "--file", schema.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(file_output.status.success());
    let stdout = String::from_utf8(file_output.stdout).unwrap();
    assert!(stdout.contains("pub struct Item {"));
    assert!(stdout.contains("    pub label: String,"));

    let database = dir.0.join("app.redb");
    {
        let mut engine = Engine::open_redb(&database).unwrap();
        assert!(
            engine
                .execute("type Item = {\n  id int,\n  label text,\n}\ntable items Item\n  key id")
                .ok
        );
    }
    let db_output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["schema", "rust", "--db", database.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(db_output.status.success());
    assert!(
        String::from_utf8(db_output.stdout)
            .unwrap()
            .contains("pub struct Item {")
    );

    let destination = dir.0.join("bindings.rs");
    let written = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "schema",
            "rust",
            "--file",
            schema.to_str().unwrap(),
            "--output",
            destination.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(written.status.success());
    assert!(
        std::fs::read_to_string(&destination)
            .unwrap()
            .contains("pub struct Item {")
    );
}
