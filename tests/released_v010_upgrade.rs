mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::TempDir;
use serde_json::Value;
use sha2::{Digest, Sha256};
use unionid::Engine;
use unionid::protocol::{PRODUCTION_VERSION, Request, VERSION, WireValue};
use unionid::server::execute_protocol_request;

const FIXTURE_SHA256: &str = "db8cdceec848b977cffea00f12077cce2f97327abf63b01d5eed3910e4979e9d";
const BACKUP_SHA256: &str = "eb0091c3c4e2d5a33faf1d874f3324e3c1cb0ba7a9a6702e0af7ab9d11df49f5";
const SCHEMA_HASH: &str = "sha256:2f7b412f82bdbff4222841fb38f8560b9130a99d54a239581efe703b894ac6a7";
const MIGRATION_CHECKSUM: &str =
    "sha256:5aad0f44ef589706244cf04ec37cebcda0647cb258dced590a76345d9e27ee3f";
const QUERY: &str = "from tasks | sort id | select {id, title, owner, tags, state}";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v0.1.0")
        .join(name)
}

fn digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(args)
        .output()
        .unwrap()
}

fn successful_json(args: &[&str]) -> Value {
    let output = run(args);
    assert!(
        output.status.success(),
        "command failed: {}\nstdout={}\nstderr={}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON from {}: {error}; stdout={}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn published_v010_database_upgrades_to_format6_without_logical_drift() {
    let source = fixture("database.redb");
    let old_backup = fixture("backup.json");
    assert_eq!(digest(&source), FIXTURE_SHA256);
    assert_eq!(digest(&old_backup), BACKUP_SHA256);

    let dir = TempDir::new();
    let database = dir.0.join("upgraded.redb");
    std::fs::copy(&source, &database).unwrap();

    let before_doctor = digest(&database);
    let doctor = successful_json(&[
        "doctor",
        "--db",
        database.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(doctor["database"]["schema"]["hash"], SCHEMA_HASH);
    assert_eq!(doctor["database"]["migration_count"], 1);
    assert_eq!(digest(&database), before_doctor);

    for (target, previous) in [("4", 3), ("5", 4), ("6", 5)] {
        let upgraded = successful_json(&[
            "upgrade",
            "--db",
            database.to_str().unwrap(),
            "--target",
            target,
            "--format",
            "json",
        ]);
        assert_eq!(upgraded["previous_format"], previous);
        assert_eq!(upgraded["format"], target.parse::<u64>().unwrap());
        assert_eq!(upgraded["changed"], true);
    }

    let checked = successful_json(&[
        "check",
        "--db",
        database.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(checked["schema"]["hash"], SCHEMA_HASH);
    assert_eq!(checked["versions"]["format"], 6);
    assert_eq!(checked["profile"]["rows_checked"], 3);
    assert_eq!(checked["profile"]["index_entries_checked"], 6);
    assert_eq!(checked["profile"]["bounded"], true);

    let migrations = fixture("migrations");
    let status = successful_json(&[
        "migration",
        "status",
        "--db",
        database.to_str().unwrap(),
        "--dir",
        migrations.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(status["schema"]["hash"], SCHEMA_HASH);
    assert_eq!(status["applied"][0]["id"], "m0001_fixture");
    assert_eq!(status["applied"][0]["checksum"], MIGRATION_CHECKSUM);
    assert_eq!(status["pending"], serde_json::json!([]));

    let rows = successful_json(&[
        "run",
        "--db",
        database.to_str().unwrap(),
        "--read-only",
        "--query",
        QUERY,
        "--format",
        "json",
    ]);
    assert_eq!(rows["schema"]["hash"], SCHEMA_HASH);
    assert_eq!(rows["rows"].as_array().unwrap().len(), 3);
    assert_eq!(rows["rows"][0]["owner"]["value"]["type_id"], 3);
    assert_eq!(
        rows["rows"][1]["state"]["value"]["value"]["value"]["variant"],
        "Pending"
    );
    assert_eq!(
        rows["rows"][2]["tags"]["value"].as_array().unwrap().len(),
        2
    );

    let updated = successful_json(&[
        "run",
        "--db",
        database.to_str().unwrap(),
        "--query",
        "update tasks\nfilter id == 2\nset state = Running {worker = \"v02\", attempt = 2}\nreturning {id, state}",
        "--format",
        "json",
    ]);
    assert_eq!(updated["affected_rows"], 1);
    assert_eq!(updated["rows"][0]["id"]["value"], 2);

    let mut engine = Engine::open_redb(&database).unwrap();
    for version in [VERSION, PRODUCTION_VERSION] {
        let request = Request::query(format!("upgrade-v{version}"), QUERY)
            .with_version(version)
            .unwrap();
        let response = execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.version, version);
        assert_eq!(response.rows.len(), 3);
        assert_eq!(response.schema.as_ref().unwrap().hash, SCHEMA_HASH);
    }
    let production_scalar = WireValue::Uuid {
        value: "550e8400-e29b-41d4-a716-446655440000".into(),
    };
    let version_one = Request {
        params: BTreeMap::from([("value".into(), production_scalar.clone())]),
        ..Request::query("upgrade-v1-scalar", "from tasks")
    };
    assert_eq!(
        version_one.decode_params().unwrap_err().code,
        "E_PROTOCOL_TYPE"
    );
    let version_two = version_one.with_version(PRODUCTION_VERSION).unwrap();
    assert!(version_two.decode_params().is_ok());
    drop(engine);

    let current_backup = dir.0.join("current.backup.json");
    successful_json(&[
        "backup",
        "--db",
        database.to_str().unwrap(),
        "--output",
        current_backup.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let restored = dir.0.join("restored.redb");
    successful_json(&[
        "restore",
        "--backup",
        current_backup.to_str().unwrap(),
        "--db",
        restored.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let restored_check = successful_json(&[
        "check",
        "--db",
        restored.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(restored_check["schema"]["hash"], SCHEMA_HASH);
    assert_eq!(restored_check["versions"]["format"], 6);

    let restored_old = dir.0.join("restored-old-backup.redb");
    successful_json(&[
        "restore",
        "--backup",
        old_backup.to_str().unwrap(),
        "--db",
        restored_old.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let old_rows = successful_json(&[
        "run",
        "--db",
        restored_old.to_str().unwrap(),
        "--read-only",
        "--query",
        QUERY,
        "--format",
        "json",
    ]);
    assert_eq!(old_rows["rows"].as_array().unwrap().len(), 3);
    assert_eq!(old_rows["schema"]["hash"], SCHEMA_HASH);

    assert_eq!(digest(&source), FIXTURE_SHA256);
    assert_eq!(digest(&old_backup), BACKUP_SHA256);
}
