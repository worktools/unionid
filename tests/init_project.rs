mod common;

use common::TempDir;
use std::path::Path;
use std::process::{Command, Output};
use unionid::project::{ProjectCheckPhase, ProjectCheckReport, ProjectCheckStatus};
use unionid::{QueryResponse, Value};

fn run(current_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unionid"))
        .current_dir(current_dir)
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn init_project_runs_the_persistent_adt_journey() {
    let temp = TempDir::new();
    let project = temp.0.join("tasks");
    let project_arg = project.to_str().unwrap();

    let initialized = run(&temp.0, &["init", project_arg]);
    assert_success(&initialized);
    let instructions = String::from_utf8(initialized.stdout).unwrap();
    assert!(instructions.contains("migration apply"));
    assert!(instructions.contains("project check"));
    assert!(instructions.contains("queries/list_running.unid"));

    let help = run(&temp.0, &["init", "--help"]);
    assert_success(&help);
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("runnable starter project"));
    assert!(help.contains("可运行的入门项目"));

    for relative in [
        ".gitignore",
        "README.md",
        "schema.unid",
        "seed.unid",
        "migrations/0001_initial.unid",
        "queries/list_running.unid",
    ] {
        assert!(project.join(relative).is_file(), "missing {relative}");
    }
    assert!(project.join("data").is_dir());

    let checked = run(
        &project,
        &["project", "check", "--dir", ".", "--format", "json"],
    );
    assert_success(&checked);
    let report: ProjectCheckReport = serde_json::from_slice(&checked.stdout).unwrap();
    assert!(report.ok);
    assert_eq!(report.schema_version, 1);
    assert!(
        report
            .stages
            .iter()
            .all(|stage| stage.status == ProjectCheckStatus::Passed)
    );
    assert_eq!(std::fs::read_dir(project.join("data")).unwrap().count(), 0);
    let repeated = run(
        &project,
        &["project", "check", "--dir", ".", "--format", "json"],
    );
    assert_success(&repeated);
    assert_eq!(checked.stdout, repeated.stdout);
    let plain = run(&project, &["project", "check", "--dir", "."]);
    assert_success(&plain);
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.find("schema: passed").unwrap() < plain.find("migrations: passed").unwrap());
    assert!(plain.find("migrations: passed").unwrap() < plain.find("queries: passed").unwrap());
    assert!(plain.contains("project check ok"));

    for source in [
        "schema.unid",
        "seed.unid",
        "migrations/0001_initial.unid",
        "queries/list_running.unid",
    ] {
        assert_success(&run(&project, &["fmt", "--file", source, "--check"]));
    }

    assert_success(&run(
        &project,
        &["schema", "check", "--file", "schema.unid"],
    ));
    assert_success(&run(
        &project,
        &[
            "query",
            "describe",
            "--schema",
            "schema.unid",
            "--file",
            "queries/list_running.unid",
        ],
    ));
    assert_success(&run(
        &project,
        &[
            "migration",
            "apply",
            "--db",
            "data/tasks.redb",
            "--dir",
            "migrations",
        ],
    ));

    let seeded = run(
        &project,
        &[
            "run",
            "--db",
            "data/tasks.redb",
            "--file",
            "seed.unid",
            "--format",
            "json",
        ],
    );
    assert_success(&seeded);
    let seeded: QueryResponse = serde_json::from_slice(&seeded.stdout).unwrap();
    assert_eq!(seeded.affected_rows, Some(2));

    let queried = run(
        &project,
        &[
            "run",
            "--db",
            "data/tasks.redb",
            "--file",
            "queries/list_running.unid",
            "--format",
            "json",
        ],
    );
    assert_success(&queried);
    let queried: QueryResponse = serde_json::from_slice(&queried.stdout).unwrap();
    assert_eq!(queried.rows.len(), 1);
    assert!(matches!(
        &queried.rows[0]["title"],
        Value::Text(title) if title == "learn ADTs"
    ));

    assert_success(&run(&project, &["doctor", "--db", "data/tasks.redb"]));
    assert_success(&run(&project, &["check", "--db", "data/tasks.redb"]));
}

#[test]
fn project_check_reports_query_errors_without_absolute_paths() {
    let temp = TempDir::new();
    let project = temp.0.join("tasks");
    assert_success(&run(&temp.0, &["init", project.to_str().unwrap()]));
    let invalid_query = "from tasks\nselect missing\n";
    std::fs::write(project.join("queries/list_running.unid"), invalid_query).unwrap();

    let checked = run(
        &project,
        &["project", "check", "--dir", ".", "--format", "json"],
    );
    assert_eq!(checked.status.code(), Some(3));
    assert!(checked.stderr.is_empty());
    let report: ProjectCheckReport = serde_json::from_slice(&checked.stdout).unwrap();
    assert!(!report.ok);
    let error = report.error.unwrap();
    assert_eq!(error.phase, ProjectCheckPhase::Queries);
    assert_eq!(error.path, "queries/list_running.unid");
    assert_eq!(error.code, "E_FIELD");
    assert!(error.span.is_some());
    assert!(!String::from_utf8_lossy(&checked.stdout).contains(temp.0.to_str().unwrap()));
    assert_eq!(
        std::fs::read_to_string(project.join("queries/list_running.unid")).unwrap(),
        invalid_query
    );
    assert_eq!(std::fs::read_dir(project.join("data")).unwrap().count(), 0);
}

#[test]
fn project_check_detects_schema_and_migration_drift() {
    let temp = TempDir::new();
    let project = temp.0.join("tasks");
    assert_success(&run(&temp.0, &["init", project.to_str().unwrap()]));
    let schema_path = project.join("schema.unid");
    let schema = std::fs::read_to_string(&schema_path).unwrap();
    std::fs::write(
        &schema_path,
        schema.replace("  Done {", "  Failed\n  Done {"),
    )
    .unwrap();
    assert_success(&run(&project, &["fmt", "--file", "schema.unid", "--check"]));

    let checked = run(
        &project,
        &["project", "check", "--dir", ".", "--format", "json"],
    );
    assert_eq!(checked.status.code(), Some(3));
    let report: ProjectCheckReport = serde_json::from_slice(&checked.stdout).unwrap();
    let error = report.error.unwrap();
    assert_eq!(error.phase, ProjectCheckPhase::Migrations);
    assert_eq!(error.path, "schema.unid");
    assert_eq!(error.code, "E_SCHEMA");
}

#[cfg(unix)]
#[test]
fn project_check_rejects_symlinked_sources() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let project = temp.0.join("tasks");
    assert_success(&run(&temp.0, &["init", project.to_str().unwrap()]));
    let query = project.join("queries/list_running.unid");
    let outside = temp.0.join("outside.unid");
    std::fs::write(&outside, "from tasks\n").unwrap();
    std::fs::remove_file(&query).unwrap();
    symlink(&outside, &query).unwrap();

    let checked = run(
        &project,
        &["project", "check", "--dir", ".", "--format", "json"],
    );
    assert_eq!(checked.status.code(), Some(2));
    let report: ProjectCheckReport = serde_json::from_slice(&checked.stdout).unwrap();
    let error = report.error.unwrap();
    assert_eq!(error.phase, ProjectCheckPhase::Queries);
    assert_eq!(error.path, "queries/list_running.unid");
    assert_eq!(error.code, "E_CONFIG");
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "from tasks\n");
}

#[test]
fn project_check_bounds_the_number_of_source_files() {
    let temp = TempDir::new();
    let project = temp.0.join("tasks");
    assert_success(&run(&temp.0, &["init", project.to_str().unwrap()]));
    for index in 0..254 {
        std::fs::write(
            project.join(format!("queries/query_{index:03}.unid")),
            "from tasks\ntake 1\n",
        )
        .unwrap();
    }

    let checked = run(
        &project,
        &["project", "check", "--dir", ".", "--format", "json"],
    );
    assert_eq!(checked.status.code(), Some(3));
    let report: ProjectCheckReport = serde_json::from_slice(&checked.stdout).unwrap();
    let error = report.error.unwrap();
    assert_eq!(error.phase, ProjectCheckPhase::Queries);
    assert_eq!(error.code, "E_LIMIT");
}

#[test]
fn init_accepts_an_empty_directory_and_refuses_a_nonempty_one() {
    let temp = TempDir::new();
    let empty = temp.0.join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert_success(&run(&temp.0, &["init", empty.to_str().unwrap()]));

    let occupied = temp.0.join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    let sentinel = occupied.join("keep.txt");
    std::fs::write(&sentinel, "keep me").unwrap();
    let refused = run(&temp.0, &["init", occupied.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep me");
    assert_eq!(std::fs::read_dir(&occupied).unwrap().count(), 1);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is not empty; no files were changed")
    );
}
