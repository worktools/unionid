mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TempDir;
use unionid::migration::MigrationEntry;
use unionid::{BackupInfo, Engine, MigrationFile, PageSpec, SchemaInfo, Value, backup};

const COMPACT_PATH_ENV: &str = "UNIONID_TEST_COMPACT_PATH";
const COMPACT_READY_ENV: &str = "UNIONID_TEST_COMPACT_READY";
const COMPACT_RESULT_ENV: &str = "UNIONID_TEST_COMPACT_RESULT";
const COMPACT_CHILD_EXIT: i32 = 95;
const ROWS: usize = 3_000;
const TEST_IDEMPOTENCY_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Reference {
    schema: SchemaInfo,
    migrations: Vec<MigrationEntry>,
    receipts: usize,
    rows: String,
}

fn insert_batches() -> Vec<String> {
    let payload = "p".repeat(256);
    let mut batches = Vec::new();
    for start in (0..ROWS).step_by(500) {
        let end = ROWS.min(start + 500);
        let mut source = String::from("insert many items [");
        for id in start..end {
            if id > start {
                source.push_str(", ");
            }
            source.push_str(&format!(
                "{{ id = {id}, label = {} }}",
                serde_json::to_string(&format!("label-{id:06}-{payload}")).unwrap()
            ));
        }
        source.push(']');
        batches.push(source);
    }
    batches
}

fn note_migration() -> MigrationFile {
    MigrationFile::parse(
        "migration m0001_note\n  add field Item.note text = \"\"\n  add index items.note\n",
    )
    .unwrap()
}

fn prepare(path: &Path) {
    let mut engine = Engine::open_redb(path).unwrap();
    let schema = engine.execute(
        "type Item =\n  id int\n  label text\ntable items Item\n  key id\ncreate index items (label)",
    );
    assert!(schema.ok, "{}", schema.message);
    for batch in insert_batches() {
        let inserted = engine.execute(&batch);
        assert!(inserted.ok, "{}", inserted.message);
    }
    engine
        .apply_migrations(std::slice::from_ref(&note_migration()))
        .unwrap();
    let updated = engine.execute_idempotent_with_params(
        "compact-preserve",
        TEST_IDEMPOTENCY_DIGEST,
        "update items | filter id == 0 | set note = \"seeded\"",
        BTreeMap::new(),
        None,
    );
    assert!(updated.unwrap().response.ok);
    assert!(engine.check_integrity().unwrap().backend_clean);
}

fn snapshot_state(path: &Path) -> Reference {
    let mut engine = Engine::open_redb(path).unwrap();
    capture(&mut engine)
}

fn capture(engine: &mut Engine) -> Reference {
    let schema = engine.schema_info();
    let migrations = engine.migration_history().to_vec();
    let receipts = engine.idempotency_status().unwrap().count;
    let rows = engine.execute("from items | sort id");
    assert!(rows.ok, "{}", rows.message);
    let rows = serde_json::to_string(&rows.rows).unwrap();
    Reference {
        schema,
        migrations,
        receipts,
        rows,
    }
}

fn first_cursor(path: &Path) -> String {
    let mut engine = Engine::open_redb(path).unwrap();
    let page = engine.execute_page("from items | sort id", PageSpec::forward(1));
    assert!(page.ok, "{}", page.message);
    page.page.unwrap().next_cursor.unwrap()
}

#[test]
fn redb_compaction_preserves_state_cursor_receipts_and_backup() {
    let dir = TempDir::new();
    let path = dir.0.join("preserve.redb");
    prepare(&path);
    let before = snapshot_state(&path);
    let cursor = first_cursor(&path);
    let before_versions = {
        let engine = Engine::open_redb(&path).unwrap();
        engine.introspection().storage_versions.unwrap()
    };
    let backup_path = dir.0.join("before-compact.backup.json");
    let backup: BackupInfo = backup::create(&path, &backup_path).unwrap();
    assert!(backup_path.exists());
    assert_eq!(backup.schema, before.schema);
    assert_eq!(backup.receipt_count, before.receipts);
    assert_eq!(backup.migration_count, before.migrations.len());

    let report = {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.compact_storage().unwrap()
    };
    assert!(report.identity_preserved);
    assert_eq!(report.schema, before.schema);
    assert_eq!(report.storage, before_versions);
    assert_eq!(engine_schema(&path), before.schema);

    let after = snapshot_state(&path);
    assert_eq!(after.schema, before.schema);
    assert_eq!(after.migrations, before.migrations);
    assert_eq!(after.receipts, before.receipts);
    assert_eq!(after.rows, before.rows);

    let mut engine = Engine::open_redb(&path).unwrap();
    assert!(engine.check_integrity().unwrap().backend_clean);
    assert_eq!(
        engine.introspection().storage_versions.unwrap(),
        before_versions
    );
    let continued = engine.execute_page("from items | sort id", PageSpec::after(1, cursor));
    assert!(continued.ok, "{}", continued.message);
    assert!(continued.rows[0]["id"].cmp_eq(&Value::Int(1)));
    let replay = engine
        .execute_idempotent_with_params(
            "compact-preserve",
            TEST_IDEMPOTENCY_DIGEST,
            "source is deliberately not parsed on replay",
            BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(
        engine
            .execute("from items | filter note == \"seeded\"")
            .rows
            .len(),
        1
    );

    let stable = engine.compact_storage().unwrap();
    assert!(stable.identity_preserved);
    assert_eq!(stable.schema, before.schema);
    drop(engine);

    let restored = dir.0.join("restored.redb");
    backup::restore(&backup_path, &restored).unwrap();
    let restored_state = snapshot_state(&restored);
    assert_eq!(restored_state.schema, before.schema);
    assert_eq!(restored_state.migrations, before.migrations);
    assert_eq!(restored_state.rows, before.rows);
}

#[test]
fn redb_compaction_reaches_a_stable_no_op_across_opens() {
    let dir = TempDir::new();
    let path = dir.0.join("noop.redb");
    prepare(&path);
    let mut changed = true;
    for _ in 0..4 {
        let mut engine = Engine::open_redb(&path).unwrap();
        let report = engine.compact_storage().unwrap();
        assert!(report.identity_preserved);
        assert_eq!(engine.execute("from items").rows.len(), ROWS);
        assert!(engine.check_integrity().unwrap().backend_clean);
        drop(engine);
        if !report.changed {
            assert_eq!(report.reclaimed_bytes, 0);
            changed = false;
            break;
        }
    }
    assert!(!changed, "compaction never reached a stable no-op");
}

fn engine_schema(path: &Path) -> SchemaInfo {
    Engine::open_redb(path).unwrap().schema_info()
}

#[test]
fn redb_compaction_child() {
    let Ok(path) = std::env::var(COMPACT_PATH_ENV) else {
        return;
    };
    let ready = std::env::var(COMPACT_READY_ENV).unwrap();
    let result = std::env::var(COMPACT_RESULT_ENV).unwrap();
    let mut engine = Engine::open_redb(PathBuf::from(&path)).unwrap();
    std::fs::write(&ready, b"ready").unwrap();
    let outcome = match engine.compact_storage() {
        Ok(report) => format!("ok changed={}", report.changed),
        Err(error) => format!("error {}", error.code),
    };
    std::fs::write(&result, outcome).unwrap();
    std::process::exit(COMPACT_CHILD_EXIT);
}

#[test]
fn redb_compaction_survives_child_process_exit_and_reopens_equivalently() {
    let dir = TempDir::new();
    let source = dir.0.join("interrupted.redb");
    prepare(&source);
    let before = snapshot_state(&source);
    let cursor = first_cursor(&source);

    for (index, delay_ms) in [0_u64, 2, 8].into_iter().enumerate() {
        let path = dir.0.join(format!("interrupted-{index}.redb"));
        std::fs::copy(&source, &path).unwrap();
        let ready = dir.0.join(format!("compact-ready-{index}"));
        let result = dir.0.join(format!("compact-result-{index}"));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "redb_compaction_child", "--nocapture"])
            .env(COMPACT_PATH_ENV, &path)
            .env(COMPACT_READY_ENV, &ready)
            .env(COMPACT_RESULT_ENV, &result)
            .spawn()
            .unwrap();
        for _ in 0..2_000 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            ready.exists(),
            "compact child did not reach the compact boundary"
        );
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let _ = child.kill();
        child.wait().unwrap();

        let mut reopened = Engine::open_redb(&path).unwrap();
        assert!(reopened.check_integrity().unwrap().backend_clean);
        let after = capture(&mut reopened);
        assert_eq!(after.schema, before.schema, "delay {delay_ms}ms");
        assert_eq!(after.migrations, before.migrations, "delay {delay_ms}ms");
        assert_eq!(after.receipts, before.receipts, "delay {delay_ms}ms");
        assert_eq!(after.rows, before.rows, "delay {delay_ms}ms");
        let continued =
            reopened.execute_page("from items | sort id", PageSpec::after(1, cursor.clone()));
        assert!(continued.ok, "delay {delay_ms}ms: {}", continued.message);
        assert!(continued.rows[0]["id"].cmp_eq(&Value::Int(1)));
        drop(reopened);
        assert_eq!(engine_schema(&path), before.schema, "delay {delay_ms}ms");
    }
}
