mod common;
use common::TempDir;
use std::{fs, process::Command};
use unionid::Engine;

#[test]
fn rehearsal_refuses_an_active_writer_before_creating_the_copy() {
    let dir = TempDir::new();
    let database = dir.0.join("source.redb");
    let engine = Engine::open_redb(&database).unwrap();
    let migrations = dir.0.join("migrations");
    fs::create_dir(&migrations).unwrap();
    let copy = dir.0.join("copy.redb");
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["migration", "rehearse", "--db"])
        .arg(&database)
        .arg("--dir")
        .arg(&migrations)
        .arg("--copy")
        .arg(&copy)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "E_BUSY");
    assert!(!copy.exists());
    drop(engine);
}

#[test]
fn rehearsal_never_overwrites_existing_files_or_source_aliases() {
    let dir = TempDir::new();
    let database = dir.0.join("source.redb");
    drop(Engine::open_redb(&database).unwrap());
    let migrations = dir.0.join("migrations");
    fs::create_dir(&migrations).unwrap();
    let unrelated = dir.0.join("keep.txt");
    fs::write(&unrelated, "keep me").unwrap();
    let mut destinations = vec![database.clone(), unrelated.clone()];
    let hardlink = dir.0.join("hardlink.redb");
    fs::hard_link(&database, &hardlink).unwrap();
    destinations.push(hardlink);
    #[cfg(unix)]
    {
        let symlink = dir.0.join("symlink.redb");
        std::os::unix::fs::symlink(&database, &symlink).unwrap();
        destinations.push(symlink);
    }
    let before = fs::read(&database).unwrap();
    for destination in destinations {
        let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["migration", "rehearse", "--db"])
            .arg(&database)
            .arg("--dir")
            .arg(&migrations)
            .arg("--copy")
            .arg(destination)
            .args(["--format", "json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["code"], "E_IO");
        assert!(
            fs::read(&database).unwrap() == before,
            "source bytes changed"
        );
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep me");
    }
    assert!(
        Engine::open_redb(&database)
            .unwrap()
            .check_integrity()
            .is_ok()
    );
}
