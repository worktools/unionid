mod common;

use std::process::Command;

use common::TempDir;
use unionid::{Engine, codegen};

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

type Event = {
  id uuid,
  at timestamp,
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
    assert!(generated.contains("pub type Ids = (i64, String);"));
    for scalar in [
        "unionid::scalars::Uuid",
        "unionid::scalars::Timestamp",
        "unionid::scalars::Duration",
        "unionid::scalars::Decimal",
        "unionid::scalars::Bytes",
    ] {
        assert!(generated.contains(scalar), "missing {scalar}");
    }
}

#[test]
fn escapes_rust_keyword_field_names() {
    let source = "\
type T = {
  async int,
  trait text,
  match bool,
}

table t T
  key async
";
    let generated = codegen::rust(source).unwrap();
    assert!(generated.contains("pub async_: i64,"));
    assert!(generated.contains("pub trait_: String,"));
    assert!(generated.contains("pub match_: bool,"));
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
