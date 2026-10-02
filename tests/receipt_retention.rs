mod common;
use common::TempDir;
use std::{collections::BTreeMap, path::Path, process::Command};
use unionid::Engine;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const QUERY: &str = "update counters | set value = value + 1 | returning {value}";

fn run(db: &Path, age: &str, extra: &[&str], code: i32) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["receipts", "retain", "--db"])
        .arg(db)
        .args(["--min-age-seconds", age, "--format", "json"])
        .args(extra)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_previews_without_touching_source_then_requires_confirm_for_one_bounded_pass() {
    let dir = TempDir::new();
    let db = dir.0.join("db.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    assert!(engine.execute("struct Counter {id: int, value: int}\ntable counters Counter\n  key id\ninsert counters {id: 1, value: 0}").ok);
    for key in ["first", "second"] {
        engine
            .execute_idempotent_with_params(key, DIGEST, QUERY, BTreeMap::new(), None)
            .unwrap();
    }
    drop(engine);
    let bytes = std::fs::read(&db).unwrap();
    let huge_window = (u64::MAX / 1_000).to_string();
    let preview = run(&db, &huge_window, &[], 0);
    assert_eq!(preview["schema_version"], 1);
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["selected_count"], 0);
    assert!(preview["cutoff_unix_ms"].is_null());
    assert!(
        preview["key_reuse_warning"]
            .as_str()
            .unwrap()
            .contains("execute again")
    );
    assert!(std::fs::read(&db).unwrap() == bytes);
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let preview = run(&db, "1", &["--max-receipts", "1"], 0);
    assert_eq!(preview["selected_count"], 1);
    assert_eq!(preview["applied"], false);
    assert!(std::fs::read(&db).unwrap() == bytes);
    let applied = run(&db, "1", &["--max-receipts", "1", "--confirm"], 0);
    assert_eq!(applied["selected_count"], 1);
    assert_eq!(applied["remaining_count"], 1);
    assert_eq!(applied["applied"], true);
    let mut engine = Engine::open_redb(db).unwrap();
    assert_eq!(engine.idempotency_status().unwrap().count, 1);
    assert!(
        engine
            .execute_idempotent_with_params("second", DIGEST, QUERY, BTreeMap::new(), None)
            .unwrap()
            .replayed
    );
    assert!(
        !engine
            .execute_idempotent_with_params("first", DIGEST, QUERY, BTreeMap::new(), None)
            .unwrap()
            .replayed
    );
    assert!(engine.execute("from counters").rows[0]["value"].cmp_eq(&unionid::Value::Int(3)));
    engine.check_integrity().unwrap();
}

#[test]
fn cli_rejects_invalid_or_missing_databases_with_one_json_error_and_no_creation() {
    let dir = TempDir::new();
    let db = dir.0.join("missing.redb");
    for (age, extra) in [
        ("0", vec![]),
        ("18446744073709551615", vec![]),
        ("1", vec!["--max-receipts", "0"]),
        ("1", vec!["--max-receipts", "1001"]),
        ("1", vec!["--confirm"]),
    ] {
        let error = run(&db, age, &extra, 2);
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "E_CONFIG");
        assert!(!db.exists());
    }
}

#[test]
fn cli_help_explains_preview_confirmation_and_key_reuse() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["receipts", "retain", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--min-age-seconds"));
    assert!(help.contains("only preview"));
    assert!(help.contains("keys can execute again"));
}
