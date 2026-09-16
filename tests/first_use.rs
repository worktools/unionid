mod common;

use common::TempDir;
use std::process::Command;

#[test]
fn user_docs_keep_one_versioned_first_use_command_contract() {
    let required = [
        "cargo install unionid --locked",
        "unionid init tasks",
        "unionid project check --dir .",
        "unionid migration apply --db data/tasks.redb --dir migrations",
        "unionid run --db data/tasks.redb --file seed.unid",
        "unionid run --db data/tasks.redb --file queries/list_running.unid",
        "unionid doctor --db data/tasks.redb",
        "unionid check --db data/tasks.redb",
    ];
    for (name, document) in [
        ("README.md", include_str!("../README.md")),
        ("docs/CLI.md", include_str!("../docs/CLI.md")),
        (
            "docs/GETTING_STARTED.md",
            include_str!("../docs/GETTING_STARTED.md"),
        ),
    ] {
        assert!(document.contains("v0.6.0"), "missing v0.6.0 in {name}");
        for command in required {
            assert!(document.contains(command), "missing {command:?} in {name}");
        }
    }
    let guide = include_str!("../docs/GETTING_STARTED.md");
    assert_eq!(guide.matches("`README.md`").count(), 2);
    assert!(guide.contains("--output data/tasks.backup.json"));
    assert!(guide.contains("--db data/restored.redb"));
}

#[test]
fn generated_starter_completes_the_documented_first_use_journey() {
    let temporary = TempDir::new();
    let work = temporary.0.join("empty");
    let output = Command::new("python3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "scripts/validate-first-use.py",
            "--binary",
            env!("CARGO_BIN_EXE_unionid"),
            "--work-dir",
            work.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["project_files"], 6);
    assert_eq!(report["rows"], 2);
    assert_eq!(report["query_rows"], 1);
    assert_eq!(report["restored_rows"], 1);
    assert_eq!(report["schema"]["revision"], 1);

    let refused = Command::new("python3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "scripts/validate-first-use.py",
            "--binary",
            env!("CARGO_BIN_EXE_unionid"),
            "--work-dir",
            work.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("work directory must be an empty directory")
    );
}
