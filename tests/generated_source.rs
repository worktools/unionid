mod common;
use common::TempDir;
use std::process::Command;
use unionid::{Engine, MigrationFile, format_source};

#[test]
fn executable_migration_diffs_are_canonical_before_applying() {
    let mut engine = Engine::memory();
    let mut migrations = Vec::new();
    for (id, parent, schema) in [
        (
            "initial",
            None,
            "enum State { Pending, Running {worker: text, attempt: int} }\nstruct Task {id: int, state: State}\ntable tasks Task\n  key id",
        ),
        (
            "priority",
            Some("initial"),
            "enum State { Pending, Running {worker: text, attempt: int} }\nstruct Task {id: int, state: State, priority: int = 0}\ntable tasks Task\n  key id",
        ),
    ] {
        let diff = engine.diff_schema(schema, id, parent).unwrap();
        assert!(diff.runnable);
        assert_eq!(
            format_source(&diff.migration_source).unwrap(),
            diff.migration_source
        );
        migrations.push(MigrationFile::parse(diff.migration_source).unwrap());
        engine.apply_migrations(&migrations).unwrap();
    }
}

#[test]
fn printed_schema_is_canonical_source_and_json_errors_remain_json() {
    let dir = TempDir::new();
    let db = dir.0.join("schema.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    assert!(engine.execute("enum State { Pending, Running {worker: text, attempt: int} }\nstruct Task {id: int, state: State}\ntable tasks Task\n  key id").ok);
    let info = engine.schema_info();
    drop(engine);
    for mode in ["source", "table", "json"] {
        let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["schema", "print", "--db"])
            .arg(&db)
            .args(["--format", mode])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let source = if mode == "json" {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            value["normalized"].as_str().unwrap().to_owned()
        } else {
            text
        };
        assert_eq!(format_source(&source).unwrap(), source);
        assert_eq!(
            Engine::check_schema(&source).unwrap().schema.hash,
            info.hash
        );
        if mode == "source" {
            assert!(output.stderr.is_empty());
        }
    }
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["schema", "print", "--db"])
        .arg(dir.0.join("missing.redb"))
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["ok"], false);
}

#[test]
fn migration_diff_output_immediately_passes_project_check() {
    let dir = TempDir::new();
    let project = dir.0.join("project");
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .current_dir(&dir.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    run(&["init", project.to_str().unwrap()]);
    let migrations = project.join("migrations");
    let db = project.join("data/tasks.redb");
    run(&[
        "migration",
        "apply",
        "--db",
        db.to_str().unwrap(),
        "--dir",
        migrations.to_str().unwrap(),
    ]);
    let schema = project.join("schema.unid");
    let source = std::fs::read_to_string(&schema)
        .unwrap()
        .replace("  state: State\n", "  state: State\n  priority: int = 0\n");
    std::fs::write(&schema, source).unwrap();
    run(&[
        "migration",
        "diff",
        "--db",
        db.to_str().unwrap(),
        "--schema",
        schema.to_str().unwrap(),
        "--dir",
        migrations.to_str().unwrap(),
        "--name",
        "priority",
    ]);
    let generated = std::fs::read_to_string(migrations.join("0002_priority.unid")).unwrap();
    assert_eq!(format_source(&generated).unwrap(), generated);
    run(&[
        "project",
        "check",
        "--dir",
        project.to_str().unwrap(),
        "--format",
        "json",
    ]);
}
