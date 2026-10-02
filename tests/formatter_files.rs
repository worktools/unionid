mod common;
use common::TempDir;
use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unionid"))
        .current_dir(dir)
        .arg("fmt")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn writes_multiple_files_then_check_succeeds_and_stdout_remains_empty() {
    let dir = TempDir::new();
    for file in ["first.unid", "second.unid"] {
        std::fs::write(dir.0.join(file), "struct Task { id: int, title: text }").unwrap();
    }
    let checked = run(&dir.0, &["--check", "first.unid", "second.unid"]);
    assert!(!checked.status.success());
    let errors = String::from_utf8(checked.stderr).unwrap();
    assert!(errors.contains("first.unid") && errors.contains("second.unid"));
    let written = run(
        &dir.0,
        &[
            "--write",
            "-f",
            "first.unid",
            "-f",
            "second.unid",
            "first.unid",
        ],
    );
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(written.stdout.is_empty());
    assert!(
        run(&dir.0, &["--check", "first.unid", "second.unid"])
            .status
            .success()
    );
    let first = std::fs::read_to_string(dir.0.join("first.unid")).unwrap();
    assert_eq!(first, unionid::format_source(&first).unwrap());
    let stdout = run(&dir.0, &["-f", "first.unid"]);
    assert!(stdout.status.success());
    assert_eq!(stdout.stdout, first.as_bytes());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
}

#[test]
fn invalid_batch_and_ambiguous_modes_leave_all_inputs_untouched() {
    let dir = TempDir::new();
    let source = "struct Task { id: int }";
    std::fs::write(dir.0.join("good.unid"), source).unwrap();
    std::fs::write(dir.0.join("bad.unid"), "struct {").unwrap();
    for args in [
        vec!["--write", "good.unid", "bad.unid"],
        vec!["good.unid", "bad.unid"],
        vec!["--check", "--write", "good.unid"],
        vec!["--write"],
    ] {
        assert!(!run(&dir.0, &args).status.success());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("good.unid")).unwrap(),
            source
        );
    }
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn preserves_permissions_and_refuses_symlink_replacement() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = TempDir::new();
    let path = dir.0.join("source.unid");
    let source = "struct Task { id: int }";
    std::fs::write(&path, source).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&path, dir.0.join("link.unid")).unwrap();
    assert!(!run(&dir.0, &["--write", "link.unid"]).status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    assert!(run(&dir.0, &["--write", "source.unid"]).status.success());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(
        std::fs::symlink_metadata(dir.0.join("link.unid"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
