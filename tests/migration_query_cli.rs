mod common;

use common::TempDir;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use unionid::{Engine, backup};

const INITIAL: &str = "migration initial\n  add type State = Pending | Done\n  add type Job =\n    id int\n    state State\n  add table jobs Job key id\n  add index jobs.state\n";
const EXPAND: &str = "migration expand\n  parent initial\n  add variant State.Cancelled\n";
const QUERY: &str =
    "from jobs\nderive done = match state { Pending => false, Done => true }\nselect {id, done}";

fn fixture() -> TempDir {
    let dir = TempDir::new();
    fs::create_dir(dir.0.join("migrations")).unwrap();
    fs::create_dir_all(dir.0.join("queries/nested")).unwrap();
    fs::write(dir.0.join("migrations/001.unid"), INITIAL).unwrap();
    fs::write(dir.0.join("queries/nested/jobs.unid"), QUERY).unwrap();
    let mut engine = Engine::open_redb(dir.0.join("db.redb")).unwrap();
    engine
        .apply_migrations(&[unionid::migration::MigrationFile::parse(INITIAL).unwrap()])
        .unwrap();
    assert!(engine.execute("insert jobs {id: 1, state: Pending}").ok);
    drop(engine);
    fs::write(dir.0.join("migrations/002.unid"), EXPAND).unwrap();
    dir
}

fn command(dir: &Path, action: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_unionid"));
    command
        .args(["migration", action, "--db"])
        .arg(dir.join("db.redb"))
        .arg("--dir")
        .arg(dir.join("migrations"))
        .arg("--queries")
        .arg(dir.join("queries"))
        .args(["--format", "json"]);
    command
}

fn run(mut command: Command, expected: i32) -> Value {
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // from_slice rejects a second object or any non-JSON output on stdout.
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn failed_preflight_returns_one_report_and_preserves_database() {
    let dir = fixture();
    let db = dir.0.join("db.redb");
    let before = dir.0.join("before.json");
    let after = dir.0.join("after.json");
    backup::create(&db, &before).unwrap();
    let bytes = fs::read(&db).unwrap();
    for action in ["plan", "rehearse", "apply"] {
        let report = run(command(&dir.0, action), 3);
        assert_eq!(report["ok"], false);
        assert_eq!(report["exit_code"], 3);
        assert_eq!(report["error"]["code"], "E_MIGRATION");
        let validation = &report["query_validation"];
        assert_eq!(validation["version"], 1);
        assert_eq!(validation["valid"], false);
        assert_eq!(validation["files"][0]["path"], "nested/jobs.unid");
        assert_eq!(
            validation["files"][0]["failures"][0]["migration_id"],
            "expand"
        );
        assert_eq!(
            validation["files"][0]["failures"][0]["error"]["code"],
            "E_MATCH"
        );
        if action != "apply" {
            assert!(
                fs::read(&db).unwrap() == bytes,
                "observational command changed source"
            );
        }
    }
    backup::create(&db, &after).unwrap();
    let before: Value = serde_json::from_slice(&fs::read(before).unwrap()).unwrap();
    let after: Value = serde_json::from_slice(&fs::read(after).unwrap()).unwrap();
    assert_eq!(before["database"], after["database"]);
    assert_eq!(before["receipts"], after["receipts"]);
    assert!(Engine::open_redb(db).unwrap().check_integrity().is_ok());
}

#[test]
fn valid_queries_rehearse_apply_and_check_even_without_pending_migrations() {
    let dir = fixture();
    fs::write(
        dir.0.join("queries/nested/jobs.unid"),
        "from jobs\nselect {id}",
    )
    .unwrap();
    let plan = run(command(&dir.0, "plan"), 0);
    assert_eq!(plan["query_validation"]["valid"], true);
    let copy = dir.0.join("kept.redb");
    let mut cmd = command(&dir.0, "rehearse");
    cmd.arg("--copy").arg(&copy);
    let report = run(cmd, 0);
    assert_eq!(report["retained_copy"], copy.to_str().unwrap());
    assert_eq!(report["checked"], true);
    assert_eq!(report["applied"], json!(["expand"]));
    assert_eq!(
        Engine::open_redb(dir.0.join("db.redb"))
            .unwrap()
            .schema_info()
            .revision,
        1
    );
    let applied = run(command(&dir.0, "apply"), 0);
    assert_eq!(applied["query_validation"]["valid"], true);
    assert_eq!(applied["applied"], json!(["expand"]));
    let no_pending = run(command(&dir.0, "apply"), 0);
    assert_eq!(no_pending["applied"], json!([]));
    assert_eq!(no_pending["query_validation"]["checked_files"], 1);
    fs::write(dir.0.join("queries/nested/jobs.unid"), QUERY).unwrap();
    assert_eq!(
        run(command(&dir.0, "apply"), 3)["query_validation"]["valid"],
        false
    );
}

#[test]
fn invalid_new_database_and_input_errors_do_not_create_database_files() {
    let dir = fixture();
    fs::remove_file(dir.0.join("db.redb")).unwrap();
    let report = run(command(&dir.0, "apply"), 3);
    assert_eq!(report["error"]["code"], "E_MIGRATION");
    assert!(!dir.0.join("db.redb").exists());
    fs::write(dir.0.join("queries/README.md"), "not a query").unwrap();
    assert_eq!(
        run(command(&dir.0, "apply"), 3)["error"]["code"],
        "E_MIGRATION"
    );
    assert!(!dir.0.join("db.redb").exists());
    fs::remove_file(dir.0.join("queries/README.md")).unwrap();
    fs::remove_file(dir.0.join("queries/nested/jobs.unid")).unwrap();
    assert_eq!(
        run(command(&dir.0, "apply"), 3)["error"]["code"],
        "E_MIGRATION"
    );
    fs::remove_dir_all(dir.0.join("queries")).unwrap();
    assert_eq!(run(command(&dir.0, "apply"), 5)["error"]["code"], "E_IO");
    assert!(!dir.0.join("db.redb").exists());
}

#[test]
fn failed_rehearsals_report_retained_copies_and_clean_automatic_copies() {
    let dir = fixture();
    let copy = dir.0.join("rejected.redb");
    let mut cmd = command(&dir.0, "rehearse");
    cmd.arg("--copy").arg(&copy);
    let report = run(cmd, 3);
    assert_eq!(report["retained_copy"], copy.to_str().unwrap());
    let engine = Engine::open_redb(&copy).unwrap();
    assert_eq!(engine.schema_info().revision, 1);
    drop(engine);
    let original = fs::read(&copy).unwrap();
    let mut cmd = command(&dir.0, "rehearse");
    cmd.arg("--copy").arg(&copy);
    let failure = run(cmd, 5);
    assert_eq!(failure["error"]["code"], "E_IO");
    assert!(failure.get("retained_copy").is_none());
    assert!(fs::read(&copy).unwrap() == original);
    let temporary = dir.0.join("temp");
    fs::create_dir(&temporary).unwrap();
    for action in ["plan", "rehearse"] {
        let mut cmd = command(&dir.0, action);
        cmd.env("TMPDIR", &temporary)
            .env("TMP", &temporary)
            .env("TEMP", &temporary);
        run(cmd, 3);
        assert_eq!(fs::read_dir(&temporary).unwrap().count(), 0);
    }
}

#[test]
fn legacy_sources_warn_on_stderr_and_table_output_identifies_checkpoint() {
    let dir = fixture();
    fs::rename(
        dir.0.join("queries/nested/jobs.unid"),
        dir.0.join("queries/nested/jobs.uid"),
    )
    .unwrap();
    let output = command(&dir.0, "plan").output().unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains(".uid"));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["query_validation"]["files"][0]["path"],
        "nested/jobs.uid"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["migration", "plan", "--db"])
        .arg(dir.0.join("db.redb"))
        .arg("--dir")
        .arg(dir.0.join("migrations"))
        .arg("--queries")
        .arg(dir.0.join("queries"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let table = String::from_utf8_lossy(&output.stdout);
    assert!(table.contains("nested/jobs.uid: invalid"));
    assert!(table.contains("expand (schema 2): E_MATCH"));
}

#[test]
fn checkpoint_details_are_opt_in_without_changing_json_or_rejection() {
    let dir = fixture();
    fs::write(
        dir.0.join("migrations/003.unid"),
        "migration note\n  parent expand\n  add field Job.note: text = \"\"\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("migrations/004.unid"),
        "migration final_note\n  parent note\n  add field Job.extra: text = \"\"\n",
    )
    .unwrap();
    let before = dir.0.join("details-before.json");
    let after = dir.0.join("details-after.json");
    backup::create(dir.0.join("db.redb"), &before).unwrap();
    for action in ["plan", "apply", "rehearse"] {
        let bytes = fs::read(dir.0.join("db.redb")).unwrap();
        let plain_json = run(command(&dir.0, action), 3);
        let mut verbose_json = command(&dir.0, action);
        verbose_json.arg("--verbose");
        assert_eq!(run(verbose_json, 3), plain_json);
        assert_eq!(
            plain_json["query_validation"]["files"][0]["failures"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        for verbose in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_unionid"));
            cmd.args(["migration", action, "--db"])
                .arg(dir.0.join("db.redb"))
                .arg("--dir")
                .arg(dir.0.join("migrations"));
            if verbose {
                cmd.arg("--verbose");
            }
            let output = cmd.output().unwrap();
            assert_eq!(output.status.code(), Some(3));
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("nested/jobs.unid: invalid"), "{text}");
            assert!(text.contains("expand (schema 2): E_MATCH"), "{text}");
            assert!(text.contains("final_note (schema 4): E_MATCH"), "{text}");
            assert_eq!(text.contains("note (schema 3): E_MATCH"), verbose);
            assert_eq!(
                text.contains("1 intermediate checkpoint failures omitted"),
                !verbose
            );
        }
        if action != "apply" {
            assert!(fs::read(dir.0.join("db.redb")).unwrap() == bytes);
        }
    }
    backup::create(dir.0.join("db.redb"), &after).unwrap();
    let before: Value = serde_json::from_slice(&fs::read(before).unwrap()).unwrap();
    let after: Value = serde_json::from_slice(&fs::read(after).unwrap()).unwrap();
    assert_eq!(before["database"], after["database"]);
    assert_eq!(before["receipts"], after["receipts"]);
    Engine::open_redb(dir.0.join("db.redb"))
        .unwrap()
        .check_integrity()
        .unwrap();
}

#[test]
fn new_parameterized_and_guarded_queries_are_bound_without_execution() {
    let dir = fixture();
    fs::write(
        dir.0.join("migrations/002.unid"),
        "migration expand\n  parent initial\n  add field Job.note: text = \"\"\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("queries/nested/jobs.unid"),
        "from jobs\nfilter id == $id\nselect {note}",
    )
    .unwrap();
    fs::write(
        dir.0.join("queries/delete.unid"),
        "delete jobs\nexpect affected == 1",
    )
    .unwrap();
    let applied = run(command(&dir.0, "apply"), 0);
    assert_eq!(applied["query_validation"]["checked_files"], 2);
    let new_query = &applied["query_validation"]["files"][1];
    assert_eq!(new_query["current_valid"], false);
    assert_eq!(new_query["valid"], true);
    assert!(new_query["parameters_changed"].is_null());
    let mut engine = Engine::open_redb(dir.0.join("db.redb")).unwrap();
    let result = engine.execute("from jobs");
    assert!(result.ok);
    assert_eq!(result.rows.len(), 1, "preflight executed the saved delete");
}

#[test]
fn later_data_conversion_failure_retains_valid_preflight_and_earlier_commits() {
    let dir = fixture();
    {
        let mut engine = Engine::open_redb(dir.0.join("db.redb")).unwrap();
        assert!(engine.execute("insert jobs {id: 2, state: Pending}").ok);
    }
    fs::write(
        dir.0.join("queries/nested/jobs.unid"),
        "from jobs\nselect {id}",
    )
    .unwrap();
    fs::write(
        dir.0.join("migrations/002.unid"),
        "migration expand\n  parent initial\n  add field Job.note: text = \"\"\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("migrations/003.unid"),
        "migration unique\n  parent expand\n  add unique index jobs.note\n",
    )
    .unwrap();
    let report = run(command(&dir.0, "apply"), 3);
    assert_eq!(report["query_validation"]["valid"], true);
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("earlier migration(s) were committed")
    );
    let engine = Engine::open_redb(dir.0.join("db.redb")).unwrap();
    assert_eq!(engine.migration_history().len(), 2);
    assert_eq!(engine.schema_info().revision, 2);
}

#[test]
fn help_agent_and_bundled_docs_expose_query_preflight() {
    for action in ["plan", "apply", "rehearse"] {
        let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["migration", action, "--help"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("--queries <DIR>"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["agent", "--format", "json"])
        .output()
        .unwrap();
    let agent: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        agent["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "migration"
                && command["usage"].as_str().unwrap().contains("--queries"))
    );
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["docs", "show", "migrations"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("--queries queries"));
}

#[test]
fn recursive_application_queries_report_contract_changes_and_migrate_nested_defaults() {
    let dir = fixture();
    fs::remove_file(dir.0.join("db.redb")).unwrap();
    fs::remove_file(dir.0.join("migrations/002.unid")).unwrap();
    fs::write(dir.0.join("migrations/001.unid"), "migration initial\n  add type Chain =\n    value int\n    next Option<Chain> = None\n  add table chains Chain key value\n").unwrap();
    fs::write(dir.0.join("queries/nested/jobs.unid"), "from chains").unwrap();
    run(command(&dir.0, "apply"), 0);
    {
        let mut engine = Engine::open_redb(dir.0.join("db.redb")).unwrap();
        let inserted = engine.execute("insert chains {value: 1, next: Some({value: 2})}");
        assert!(inserted.ok, "{}", inserted.message);
    }
    fs::write(
        dir.0.join("migrations/002.unid"),
        "migration notes\n  parent initial\n  add field Chain.note: text = \"default\"\n",
    )
    .unwrap();
    fs::write(dir.0.join("queries/nested/jobs.unid"), "update chains | filter value == $id | set next = $next | returning {next}\nexpect affected == 1").unwrap();
    let plan = run(command(&dir.0, "plan"), 0);
    assert_eq!(
        plan["query_validation"]["files"][0]["parameters_changed"],
        true
    );
    assert_eq!(plan["query_validation"]["files"][0]["result_changed"], true);
    assert_eq!(
        plan["query_validation"]["files"][0]["compatibility"],
        "conditional"
    );
    run(command(&dir.0, "rehearse"), 0);
    run(command(&dir.0, "apply"), 0);
    let mut reopened = Engine::open_redb(dir.0.join("db.redb")).unwrap();
    let rows = reopened.execute("from chains");
    assert!(rows.ok);
    assert_eq!(rows.rows.len(), 1);
    assert!(rows.rows[0]["note"].cmp_eq(&unionid::Value::Text("default".into())));
    assert!(
        rows.rows[0]["next"]
            .source_text()
            .contains("note: \"default\"")
    );
    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct Chain {
        value: i64,
        next: Option<Box<Chain>>,
        note: String,
    }
    #[derive(serde::Deserialize)]
    struct Returned {
        next: Option<Chain>,
    }
    let replacement = Some(Chain {
        value: 3,
        next: None,
        note: "application".into(),
    });
    let response = reopened.execute_with_params(
        &fs::read_to_string(dir.0.join("queries/nested/jobs.unid")).unwrap(),
        std::collections::BTreeMap::from([
            ("id".into(), unionid::Value::Int(1)),
            (
                "next".into(),
                unionid::Value::from_serde(&replacement).unwrap(),
            ),
        ]),
    );
    assert!(response.ok, "{}", response.message);
    let returned = response.typed_rows::<Returned>().unwrap();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].next, replacement);
    reopened.check_integrity().unwrap();
}

#[test]
fn automatic_preflight_and_explicit_opt_out_preserve_the_apply_boundary() {
    let dir = fixture();
    fs::write(dir.0.join("queries/README.md"), "Saved query documentation").unwrap();
    for action in ["plan", "rehearse", "apply"] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_unionid"));
        cmd.current_dir(dir.0.join("migrations")).args([
            "migration",
            action,
            "--db",
            "../db.redb",
            "--dir",
            ".",
            "--format",
            "json",
        ]);
        let report = run(cmd, 3);
        assert_eq!(report["query_validation"]["valid"], false);
        assert_eq!(
            report["query_validation"]["files"][0]["failures"][0]["error"]["code"],
            "E_MATCH"
        );
        assert_eq!(
            Engine::open_redb(dir.0.join("db.redb"))
                .unwrap()
                .migration_history()
                .len(),
            1
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .current_dir(&dir.0)
        .args([
            "migration",
            "apply",
            "--db",
            "db.redb",
            "--no-queries",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("preflight explicitly disabled"));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.get("query_validation").is_none());
    let mut engine = Engine::open_redb(dir.0.join("db.redb")).unwrap();
    assert_eq!(engine.migration_history().len(), 2);
    assert_eq!(engine.execute(QUERY).error.unwrap().code, "E_MATCH");
    engine.check_integrity().unwrap();
}

#[test]
fn missing_or_empty_discovered_queries_keep_legacy_execution() {
    let dir = fixture();
    fs::remove_dir_all(dir.0.join("queries")).unwrap();
    for empty in [false, true] {
        if empty {
            fs::create_dir(dir.0.join("queries")).unwrap();
            fs::write(dir.0.join("queries/README.md"), "No query sources yet").unwrap();
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_unionid"));
        cmd.current_dir(&dir.0)
            .args(["migration", "plan", "--db", "db.redb", "--format", "json"]);
        assert!(run(cmd, 0).get("query_validation").is_none());
    }
    let output = command(&dir.0, "apply")
        .arg("--no-queries")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
