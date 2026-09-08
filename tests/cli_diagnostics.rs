mod common;

use std::process::Command;

use common::TempDir;
use serde_json::Value;
use unionid::Engine;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn version_and_doctor_have_stable_machine_readable_shapes() {
    let version = run(&["version", "--format", "json"]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    let version = json(&version);
    assert_eq!(version["schema_version"], 1);
    assert_eq!(version["software_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(version["protocol_versions"], serde_json::json!([1, 2]));
    assert_eq!(version["stream_protocol_versions"], serde_json::json!([1]));
    assert_eq!(version["current_storage"]["format"], 4);
    assert!(version["target"].as_str().unwrap().contains('-'));

    let dir = TempDir::new();
    let doctor = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["doctor", "--format", "json"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(doctor.status.success());
    assert!(doctor.stderr.is_empty());
    let doctor = json(&doctor);
    assert_eq!(doctor["schema_version"], 1);
    assert_eq!(doctor["ok"], true);
    assert!(doctor.get("database").is_none());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn doctor_reads_existing_state_without_changing_the_database() {
    let dir = TempDir::new();
    let database = dir.0.join("doctor.redb");
    {
        let mut engine = Engine::open_redb(&database).unwrap();
        assert!(engine.execute("create table items (id int)").ok);
    }
    let before = std::fs::read(&database).unwrap();
    let output = run(&[
        "doctor",
        "--db",
        database.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report = json(&output);
    assert_eq!(report["database"]["storage"], "redb");
    assert_eq!(report["database"]["read_only"], true);
    assert_eq!(report["database"]["schema"]["revision"], 1);
    assert_eq!(report["database"]["table_count"], 1);
    assert!(report["database"].get("schema_source").is_none());
    assert!(
        std::fs::read(&database).unwrap() == before,
        "doctor changed database bytes"
    );
}

#[test]
fn json_errors_and_exit_classes_are_stable_and_redacted() {
    let argument = run(&["version", "--format=json", "--secret-option"]);
    assert_eq!(argument.status.code(), Some(2));
    assert_eq!(json(&argument)["error"]["code"], "E_ARGUMENT");
    assert!(!String::from_utf8_lossy(&argument.stdout).contains("secret-option"));
    assert!(argument.stderr.is_empty());

    let dir = TempDir::new();
    let schema = dir.0.join("private-schema.uid");
    std::fs::write(&schema, "type Broken = {").unwrap();
    let input = run(&[
        "schema",
        "check",
        "--file",
        schema.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(input.status.code(), Some(3));
    assert_eq!(json(&input)["error"]["code"], "E_SYNTAX");
    assert!(input.stderr.is_empty());

    let database = dir.0.join("busy.redb");
    let engine = Engine::open_redb(&database).unwrap();
    let busy = run(&[
        "receipts",
        "status",
        "--db",
        database.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(busy.status.code(), Some(4));
    assert_eq!(json(&busy)["error"]["code"], "E_BUSY");
    assert!(busy.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&busy.stdout).contains(database.to_str().unwrap()));
    drop(engine);

    let corrupt = dir.0.join("secret-corrupt.redb");
    std::fs::write(&corrupt, b"not a redb database").unwrap();
    let storage = run(&[
        "doctor",
        "--db",
        corrupt.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(storage.status.code(), Some(5));
    assert_eq!(json(&storage)["error"]["code"], "E_STORAGE");
    assert!(!String::from_utf8_lossy(&storage.stdout).contains(corrupt.to_str().unwrap()));

    let integrity = run(&[
        "check",
        "--db",
        corrupt.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(integrity.status.code(), Some(6));
    assert_eq!(json(&integrity)["exit_code"], 6);
}

#[test]
fn query_json_response_remains_compatible_while_using_input_exit_class() {
    let output = run(&["run", "--query", "from absent", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3));
    let response = json(&output);
    assert!(response.get("schema_version").is_none());
    assert_eq!(response["error"]["code"], "E_TABLE");
}
