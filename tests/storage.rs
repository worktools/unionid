mod common;
use common::TempDir;
use redb::{
    Database as RedbDatabase, Durability, ReadableDatabase, ReadableTable, TableDefinition,
};
use std::process::Command;
use unionid::{Engine, MigrationFile, QueryAccessKind, UpsertAction, Value};

const REDB_META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const REDB_CATALOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("catalog");
const REDB_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("rows");
const REDB_SECONDARY_INDEX: TableDefinition<&[u8], u8> = TableDefinition::new("secondary_index");
const REDB_MIGRATION_LEDGER: TableDefinition<u64, &[u8]> = TableDefinition::new("migration_ledger");
const CRASH_PATH_ENV: &str = "UNIONID_TEST_REDB_CRASH_PATH";
const CRASH_MODE_ENV: &str = "UNIONID_TEST_REDB_CRASH_MODE";
const DISK_LIMIT_PATH_ENV: &str = "UNIONID_TEST_REDB_DISK_LIMIT_PATH";
const DISK_LIMIT_RESULT_ENV: &str = "UNIONID_TEST_REDB_DISK_LIMIT_RESULT";
const CRASH_BEFORE_COMMIT: i32 = 91;
const CRASH_AFTER_COMMIT: i32 = 92;
const DISK_LIMIT_FAILURE: i32 = 93;

#[test]
fn recursive_adt_rows_indexes_and_schema_survive_redb_reopen() {
    let dir = TempDir::new();
    let path = dir.0.join("recursive.redb");
    let schema;
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let result = engine.execute(include_str!("../examples/recursive_tree.uid"));
        assert!(result.ok, "{}", result.message);
        assert_eq!(result.rows.len(), 2);
        schema = engine.schema_info();
    }

    let mut reopened = Engine::open_redb(&path).unwrap();
    assert_eq!(reopened.schema_info(), schema);
    let exact = r#"Tree.Branch {label = "root", children = [Tree.Leaf "readme", Tree.Branch {label = "src", children = []}]}"#;
    let rows = reopened.execute(&format!("from documents | filter tree == {exact}"));
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 1);
    let plan = reopened.execute(&format!("explain from documents | filter tree == {exact}"));
    assert!(plan.ok, "{}", plan.message);
    assert_eq!(
        plan.plan.unwrap().access.kind,
        QueryAccessKind::SecondaryIndexLookup
    );
    assert!(reopened.check_integrity().unwrap().backend_clean);
}

#[test]
fn adt_match_updates_persist_rows_and_secondary_indexes() {
    let dir = TempDir::new();
    let path = dir.0.join("match-update.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let setup = engine.execute(
            r#"type State =
  Queued {attempt int}
  | Running {worker text, attempt int}
  | Done

type Job =
  id int
  state State

table jobs Job
  key id

create index jobs (state)
insert jobs {id = 1, state = Queued {attempt = 0}}"#,
        );
        assert!(setup.ok, "{}", setup.message);
        let updated = engine.execute(
            r#"update jobs
set state =
  match state
    Queued {attempt} => Running {worker = "disk", attempt = attempt + 1}
    current => current"#,
        );
        assert!(updated.ok, "{}", updated.message);
        assert_eq!(updated.affected_rows, Some(1));
    }

    let mut reopened = Engine::open_redb(&path).unwrap();
    let value = "State.Running {worker = \"disk\", attempt = 1}";
    let rows = reopened.execute(&format!("from jobs | filter state == {value}"));
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 1);
    let plan = reopened.execute(&format!("explain from jobs | filter state == {value}"));
    assert!(plan.ok, "{}", plan.message);
    assert_eq!(
        plan.plan.unwrap().access.kind,
        QueryAccessKind::SecondaryIndexLookup
    );
    assert!(reopened.check_integrity().unwrap().backend_clean);
}

#[test]
fn redb_crash_transaction_child() {
    let Ok(path) = std::env::var(CRASH_PATH_ENV) else {
        return;
    };
    let mode = std::env::var(CRASH_MODE_ENV).unwrap();
    if mode == "before" {
        let database = RedbDatabase::open(path).unwrap();
        let mut transaction = database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        transaction.set_two_phase_commit(true);
        transaction
            .open_table(REDB_META)
            .unwrap()
            .insert("commit_sequence", 999_u64.to_be_bytes().as_slice())
            .unwrap();
        transaction
            .open_table(REDB_CATALOG)
            .unwrap()
            .retain(|_, _| false)
            .unwrap();
        transaction
            .open_table(REDB_ROWS)
            .unwrap()
            .retain(|_, _| false)
            .unwrap();
        transaction
            .open_table(REDB_SECONDARY_INDEX)
            .unwrap()
            .retain(|_, _| false)
            .unwrap();
        transaction
            .open_table(REDB_MIGRATION_LEDGER)
            .unwrap()
            .retain(|_, _| false)
            .unwrap();
        std::process::exit(CRASH_BEFORE_COMMIT);
    }
    if mode == "after" {
        let mut engine = Engine::open_redb(path).unwrap();
        let response = engine.execute("insert entries {id = 2, value = \"committed\"}");
        assert!(response.ok, "{}", response.message);
        std::process::exit(CRASH_AFTER_COMMIT);
    }
    panic!("unknown crash mode '{mode}'");
}

#[test]
fn redb_disk_limit_child() {
    let Ok(path) = std::env::var(DISK_LIMIT_PATH_ENV) else {
        return;
    };
    let result_path = std::env::var(DISK_LIMIT_RESULT_ENV).unwrap();
    let mut engine = Engine::open_redb(path).unwrap();
    let payload = "x".repeat(900_000);
    let failed = engine.execute(&format!(
        "insert entries {{id = 2, value = {}}}",
        serde_json::to_string(&payload).unwrap()
    ));
    assert!(!failed.ok, "the OS file-size limit must reject the write");
    assert_eq!(failed.error.as_ref().unwrap().code, "E_STORAGE");
    let retry = engine.execute("insert entries {id = 3, value = \"retry\"}");
    std::fs::write(
        result_path,
        serde_json::to_vec(&serde_json::json!({
            "failure": failed.message,
            "retry": retry.message,
        }))
        .unwrap(),
    )
    .unwrap();
    std::process::exit(DISK_LIMIT_FAILURE);
}

#[test]
fn redb_real_disk_growth_failure_is_classified_and_recovers_atomically() {
    let dir = TempDir::new();
    let path = dir.0.join("limited.redb");
    let result_path = dir.0.join("result.json");
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        engine
            .apply_migrations(std::slice::from_ref(&crash_migration()))
            .unwrap();
        assert!(
            engine
                .execute("insert entries {id = 1, value = \"baseline\"}")
                .ok
        );
    }

    let blocks = std::fs::metadata(&path).unwrap().len().div_ceil(512);
    let status = Command::new("sh")
        .args([
            "-c",
            "trap '' XFSZ; ulimit -f \"$1\"; exec \"$2\" --exact redb_disk_limit_child --nocapture",
            "unionid-disk-limit",
            &blocks.to_string(),
            std::env::current_exe().unwrap().to_str().unwrap(),
        ])
        .env(DISK_LIMIT_PATH_ENV, &path)
        .env(DISK_LIMIT_RESULT_ENV, &result_path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(DISK_LIMIT_FAILURE));

    let classification: serde_json::Value =
        serde_json::from_slice(&std::fs::read(result_path).unwrap()).unwrap();
    let failure = classification["failure"].as_str().unwrap();
    let retry = classification["retry"].as_str().unwrap();
    if failure.contains("result is uncertain") {
        assert!(retry.contains("writes are disabled"), "{retry}");
    } else {
        assert!(failure.contains("aborted before commit"), "{failure}");
        assert!(!retry.contains("writes are disabled"), "{retry}");
    }

    let mut reopened = Engine::open_redb(path).unwrap();
    assert!(reopened.check_integrity().unwrap().backend_clean);
    let rows = reopened.execute("from entries | sort id");
    assert!(rows.ok, "{}", rows.message);
    assert!(matches!(rows.rows.len(), 1 | 2));
    assert!(rows.rows[0]["id"].cmp_eq(&Value::Int(1)));
    if rows.rows.len() == 2 {
        assert!(rows.rows[1]["id"].cmp_eq(&Value::Int(2)));
        let Value::Text(payload) = rows.rows[1]["value"].unwrapped() else {
            panic!("committed payload must remain typed text");
        };
        assert_eq!(payload.len(), 900_000);
    }
    assert_eq!(
        reopened
            .migration_status(std::slice::from_ref(&crash_migration()))
            .unwrap()
            .applied
            .len(),
        1
    );
}

#[test]
fn redb_recovers_complete_state_across_process_exit_boundaries() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        engine
            .apply_migrations(std::slice::from_ref(&crash_migration()))
            .unwrap();
        assert!(
            engine
                .execute("insert entries {id = 1, value = \"baseline\"}")
                .ok
        );
    }
    run_crash_child(&path, "before", CRASH_BEFORE_COMMIT);
    {
        let mut reopened = Engine::open_redb(path.clone()).unwrap();
        let rows = reopened.execute("from entries");
        assert!(rows.ok, "{}", rows.message);
        assert_eq!(rows.rows.len(), 1);
        assert!(rows.rows[0]["id"].cmp_eq(&Value::Int(1)));
        assert_eq!(
            reopened
                .migration_status(std::slice::from_ref(&crash_migration()))
                .unwrap()
                .applied
                .len(),
            1
        );
    }
    run_crash_child(&path, "after", CRASH_AFTER_COMMIT);
    let mut reopened = Engine::open_redb(path).unwrap();
    let rows = reopened.execute("from entries | sort id");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 2);
    assert!(rows.rows[1]["id"].cmp_eq(&Value::Int(2)));
    assert_eq!(
        reopened
            .migration_status(std::slice::from_ref(&crash_migration()))
            .unwrap()
            .applied
            .len(),
        1
    );
}

fn crash_migration() -> MigrationFile {
    MigrationFile::parse(
        "migration m0001_initial\n  add type Entry =\n    id int\n    value text\n  add table entries Entry key id\n",
    )
    .unwrap()
}

fn run_crash_child(path: &std::path::Path, mode: &str, expected_code: i32) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "redb_crash_transaction_child", "--nocapture"])
        .env(CRASH_PATH_ENV, path)
        .env(CRASH_MODE_ENV, mode)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(expected_code));
}

#[test]
fn redb_atomic_adt_batches_survive_reopen() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    let expected_schema;
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        let response = engine.execute(include_str!("../examples/tasks.uid"));
        assert!(response.ok, "{}", response.message);
        assert!(engine.execute("create index tasks (owner.email)").ok);
        expected_schema = engine.schema_info();
        let failed = engine.execute("insert tasks {id = 3}");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_FIELD");
        assert_eq!(engine.execute("from tasks").rows.len(), 2);
    }
    let mut reopened = Engine::open_redb(path).unwrap();
    assert_eq!(reopened.schema_info(), expected_schema);
    let response = reopened
        .execute("from tasks | filter owner.email == \"alice@example.com\" | select {id, state}");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["id"].cmp_eq(&Value::Int(1)));
}

#[test]
fn redb_schema_migration_persists_catalog_rows_and_rebuilt_indexes() {
    let dir = TempDir::new();
    let path = dir.0.join("migration.redb");
    let migrated_schema;
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        let created = engine.execute(
            r#"type State = Pending | Failed {message text}
type Task =
  id int
  state State
table tasks Task
  key id
create index tasks (state)
insert tasks {id = 1, state = Failed {message = "broken"}}"#,
        );
        assert!(created.ok, "{}", created.message);
        let migrated = engine.execute(
            "migration task_state_v2\n  rename field Task.id to task_id\n  add field Task.priority int = 0\n  rename variant State.Failed to Rejected\n  change variant State.Rejected to {code int, message text}\n    using old -> {code = 500, message = old.message}",
        );
        assert!(migrated.ok, "{}", migrated.message);
        migrated_schema = engine.schema_info();
        assert!(engine.check_integrity().unwrap().backend_clean);
    }

    let mut reopened = Engine::open_redb(path).unwrap();
    assert_eq!(reopened.schema_info(), migrated_schema);
    let response = reopened.execute(
        "from tasks | filter task_id == 1 | filter match state\n  Rejected {code, ..} => code == 500\n  Pending => false",
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["priority"].cmp_eq(&Value::Int(0)));
    assert!(
        response.rows[0]["state"]
            .source_text()
            .contains("code = 500")
    );
}

#[test]
fn redb_incremental_commit_handles_schema_and_multi_table_batches() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        assert!(
            engine
                .execute(
                    "type Entry =\n  id int\n  label text\ntable active Entry\n  key id\ntable archive Entry\n  key id\ncreate index active (label)\ninsert active {id = 1, label = \"old\"}\ninsert archive {id = 2, label = \"remove\"}",
                )
                .ok
        );
    }
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        let changed = engine.execute(
            "type Audit =\n  id int\n  message text\ntable audits Audit\n  key id\ninsert audits {id = 10, message = \"created\"}\nupdate active | filter id == 1 | set label = \"new\"\ndelete archive | filter id == 2",
        );
        assert!(changed.ok, "{}", changed.message);
        let integrity = engine.check_integrity().unwrap();
        assert!(integrity.backend_clean);
    }

    let mut reopened = Engine::open_redb(path.clone()).unwrap();
    assert_eq!(
        reopened
            .execute("from active | filter label == \"new\"")
            .rows
            .len(),
        1
    );
    assert!(reopened.execute("from archive").rows.is_empty());
    assert_eq!(reopened.execute("from audits").rows.len(), 1);
    drop(reopened);
}

#[test]
fn redb_upgrades_and_persists_the_per_table_row_id_cursor() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        assert!(
            engine
                .execute(
                    "create table entries (id int)\ninsert entries {id = 1}\ninsert entries {id = 2}",
                )
                .ok
        );
    }

    // Simulate a catalog written by the first redb release, before the table
    // allocation cursor was persisted.
    let database = RedbDatabase::open(&path).unwrap();
    {
        let mut transaction = database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        transaction.set_two_phase_commit(true);
        let mut catalog = transaction.open_table(REDB_CATALOG).unwrap();
        let mut legacy_entry = None;
        for entry in catalog.iter().unwrap() {
            let (key, value) = entry.unwrap();
            let mut json: serde_json::Value = serde_json::from_slice(&value.value()[6..]).unwrap();
            if json["kind"] == "Table" {
                json["value"].as_object_mut().unwrap().remove("next_row_id");
                let mut encoded = b"UIDC".to_vec();
                encoded.extend_from_slice(&1_u16.to_be_bytes());
                encoded.extend(serde_json::to_vec(&json).unwrap());
                legacy_entry = Some((key.value().to_vec(), encoded));
                break;
            }
        }
        let (key, value) = legacy_entry.unwrap();
        catalog.insert(key.as_slice(), value.as_slice()).unwrap();
        drop(catalog);
        transaction.commit().unwrap();
    }
    drop(database);

    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        assert!(engine.execute("insert entries {id = 3}").ok);
    }

    let database = RedbDatabase::open(&path).unwrap();
    let transaction = database.begin_read().unwrap();
    let catalog = transaction.open_table(REDB_CATALOG).unwrap();
    let cursor = catalog
        .iter()
        .unwrap()
        .filter_map(|entry| {
            let (_, value) = entry.ok()?;
            let json: serde_json::Value = serde_json::from_slice(&value.value()[6..]).ok()?;
            (json["kind"] == "Table").then(|| json["value"]["next_row_id"].as_u64())?
        })
        .next();
    assert_eq!(cursor, Some(3));
    let rows = transaction.open_table(REDB_ROWS).unwrap();
    let row_ids = rows
        .iter()
        .unwrap()
        .map(|entry| {
            let (key, _) = entry.unwrap();
            u64::from_be_bytes(key.value()[8..].try_into().unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(row_ids, vec![0, 1, 2]);
}

#[test]
fn redb_update_delete_preserve_row_ids_constraints_and_indexes() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        assert!(
            engine
                .execute(
                    "type Entry =\n  id int\n  label text\ntable entries Entry\n  key id\ncreate index entries (label)\ninsert entries {id = 1, label = \"one\"}\ninsert entries {id = 2, label = \"two\"}\ninsert entries {id = 3, label = \"three\"}",
                )
                .ok
        );
        let deleted = engine.execute("delete entries | filter id == 2");
        assert!(deleted.ok, "{}", deleted.message);
        assert_eq!(deleted.affected_rows, Some(1));
        let updated =
            engine.execute("update entries\nfilter id == 3\nset id = 30\nset label = \"changed\"");
        assert!(updated.ok, "{}", updated.message);
        assert_eq!(updated.affected_rows, Some(1));
        assert!(
            engine
                .execute("insert entries {id = 4, label = \"four\"}")
                .ok
        );
        let replaced = engine.execute("upsert entries {id = 30, label = \"upserted\"}");
        assert!(replaced.ok, "{}", replaced.message);
        assert_eq!(replaced.upsert_action, Some(UpsertAction::Updated));
        let inserted = engine.execute("upsert entries {id = 5, label = \"five\"}");
        assert!(inserted.ok, "{}", inserted.message);
        assert_eq!(inserted.upsert_action, Some(UpsertAction::Inserted));

        let failed = engine.execute("update entries\nset id = 1");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_CONSTRAINT");
    }

    {
        let mut reopened = Engine::open_redb(path.clone()).unwrap();
        assert!(
            reopened
                .execute("from entries | filter id == 2")
                .rows
                .is_empty()
        );
        assert_eq!(
            reopened
                .execute("from entries | filter id == 30 | filter label == \"upserted\"")
                .rows
                .len(),
            1
        );
        let rows = reopened.execute("from entries | sort id");
        assert!(rows.ok, "{}", rows.message);
        assert_eq!(rows.rows.len(), 4);
        assert!(rows.rows[0]["id"].cmp_eq(&Value::Int(1)));
        assert!(rows.rows[1]["id"].cmp_eq(&Value::Int(4)));
        assert!(rows.rows[2]["id"].cmp_eq(&Value::Int(5)));
        assert!(rows.rows[3]["id"].cmp_eq(&Value::Int(30)));
    }

    let database = RedbDatabase::open(&path).unwrap();
    let transaction = database.begin_read().unwrap();
    let rows = transaction.open_table(REDB_ROWS).unwrap();
    let row_ids = rows
        .iter()
        .unwrap()
        .map(|entry| {
            let (key, _) = entry.unwrap();
            u64::from_be_bytes(key.value()[8..].try_into().unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(row_ids, vec![0, 2, 3, 4]);
    let catalog = transaction.open_table(REDB_CATALOG).unwrap();
    let cursor = catalog
        .iter()
        .unwrap()
        .filter_map(|entry| {
            let (_, value) = entry.ok()?;
            let json: serde_json::Value = serde_json::from_slice(&value.value()[6..]).ok()?;
            (json["kind"] == "Table").then(|| json["value"]["next_row_id"].as_u64())?
        })
        .next();
    assert_eq!(cursor, Some(5));
}

#[test]
fn redb_fixed_tables_are_versioned_and_unknown_formats_fail_closed() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    drop(Engine::open_redb(path.clone()).unwrap());
    let database = RedbDatabase::open(&path).unwrap();
    {
        let transaction = database.begin_read().unwrap();
        let meta = transaction.open_table(REDB_META).unwrap();
        assert_eq!(
            meta.get("storage_format_version").unwrap().unwrap().value(),
            1_u32.to_be_bytes()
        );
        transaction.open_table(REDB_CATALOG).unwrap();
        transaction.open_table(REDB_ROWS).unwrap();
        transaction.open_table(REDB_SECONDARY_INDEX).unwrap();
        transaction.open_table(REDB_MIGRATION_LEDGER).unwrap();
    }
    {
        let mut transaction = database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        transaction.set_two_phase_commit(true);
        transaction
            .open_table(REDB_META)
            .unwrap()
            .insert("storage_format_version", 99_u32.to_be_bytes().as_slice())
            .unwrap();
        transaction.commit().unwrap();
    }
    drop(database);
    let error = Engine::open_redb(path).err().unwrap();
    assert_eq!(error.code, "E_STORAGE");
    assert!(error.message.contains("unsupported storage_format_version"));
}

#[test]
fn redb_unknown_codec_versions_fail_closed() {
    for (key, value) in [
        ("catalog_codec_version", 99_u16.to_be_bytes()),
        ("value_codec_version", 99_u16.to_be_bytes()),
        ("index_key_version", 99_u16.to_be_bytes()),
    ] {
        let dir = TempDir::new();
        let path = dir.0.join("state.redb");
        drop(Engine::open_redb(path.clone()).unwrap());
        let database = RedbDatabase::open(&path).unwrap();
        {
            let mut transaction = database.begin_write().unwrap();
            transaction.set_durability(Durability::Immediate).unwrap();
            transaction.set_two_phase_commit(true);
            transaction
                .open_table(REDB_META)
                .unwrap()
                .insert(key, value.as_slice())
                .unwrap();
            transaction.commit().unwrap();
        }
        drop(database);
        let error = Engine::open_redb(path).err().unwrap();
        assert_eq!(error.code, "E_STORAGE");
        assert!(error.message.contains(&format!("unsupported {key}")));
    }
}

#[test]
fn redb_unknown_migration_codec_versions_fail_closed() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    drop(Engine::open_redb(path.clone()).unwrap());
    let database = RedbDatabase::open(&path).unwrap();
    {
        let mut transaction = database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        transaction.set_two_phase_commit(true);
        let mut ledger = transaction.open_table(REDB_MIGRATION_LEDGER).unwrap();
        ledger.insert(0, b"UIDM\0c{}".as_slice()).unwrap();
        drop(ledger);
        transaction.commit().unwrap();
    }
    drop(database);
    let error = Engine::open_redb(path).err().unwrap();
    assert_eq!(error.code, "E_STORAGE");
    assert!(
        error
            .message
            .contains("unsupported migration ledger codec version")
    );
}

#[test]
fn invalid_redb_files_are_rejected_without_replacement() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    let contents = b"not a redb database";
    std::fs::write(&path, contents).unwrap();
    let error = Engine::open_redb(path.clone()).err().unwrap();
    assert_eq!(error.code, "E_STORAGE");
    assert!(error.message.contains("open redb database"));
    assert_eq!(std::fs::read(path).unwrap(), contents);
}

#[test]
fn redb_enforces_single_database_ownership() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    let first = Engine::open_redb(path.clone()).unwrap();
    let error = Engine::open_redb(path.clone()).err().unwrap();
    assert_eq!(error.code, "E_BUSY");
    drop(first);
    assert!(Engine::open_redb(path).is_ok());
}

#[test]
fn redb_rejects_secondary_indexes_that_do_not_match_rows() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        assert!(
            engine
                .execute("create table entries (id int, label text)\ncreate index entries (label)\ninsert entries {id = 1, label = \"saved\"}")
                .ok
        );
    }
    let database = RedbDatabase::open(&path).unwrap();
    {
        let mut transaction = database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        transaction.set_two_phase_commit(true);
        transaction
            .open_table(REDB_SECONDARY_INDEX)
            .unwrap()
            .retain(|_, _| false)
            .unwrap();
        transaction.commit().unwrap();
    }
    drop(database);
    let error = Engine::open_redb(path).err().unwrap();
    assert_eq!(error.code, "E_STORAGE");
    assert!(error.message.contains("secondary indexes do not match"));
}

#[test]
fn typed_atomic_batches_survive_reopen() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    {
        let mut e = Engine::open(Some(wal.clone()), None, 0).unwrap();
        assert!(e.execute(include_str!("../examples/tasks.uid")).ok);
        assert!(e.execute("create index tasks (owner.email)").ok);
        assert!(!e.execute("insert tasks {id = 3}").ok);
    }
    let mut e = Engine::open(Some(wal.clone()), None, 0).unwrap();
    let r = e.execute("from tasks | filter owner.email == \"alice@example.com\"");
    assert!(r.ok, "{}", r.message);
    assert_eq!(r.rows.len(), 1);
    assert!(r.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert_eq!(std::fs::read_to_string(wal).unwrap().lines().count(), 2);
}

#[test]
fn schema_identity_survives_wal_and_snapshot_recovery() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("db.snapshot");
    let expected;
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 0).unwrap();
        assert!(
            e.execute("type Entry =\n  id int\n  value text\ntable entries Entry\n  key id")
                .ok
        );
        assert!(e.execute("insert entries {id = 1, value = \"one\"}").ok);
        expected = e.schema_info();
    }
    {
        let mut from_wal = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 0).unwrap();
        assert_eq!(from_wal.schema_info(), expected);
        assert_eq!(
            from_wal.execute("from entries").schema,
            Some(expected.clone())
        );
        from_wal.checkpoint().unwrap();
    }
    let mut reopened = Engine::open(Some(wal), Some(snapshot), 0).unwrap();
    assert_eq!(reopened.schema_info(), expected);
    assert_eq!(reopened.execute("from entries").schema, Some(expected));
}

#[test]
fn recovered_schema_keeps_defaults_for_future_inserts() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    {
        let mut e = Engine::open(Some(wal.clone()), None, 0).unwrap();
        assert!(
            e.execute(
                "type Entry =\n  id int\n  label text = \"untitled\"\ntable entries Entry\n  key id"
            )
            .ok
        );
        assert!(e.execute("insert entries {id = 1}").ok);
    }
    let mut reopened = Engine::open(Some(wal), None, 0).unwrap();
    assert!(reopened.execute("insert entries {id = 2}").ok);
    let result = reopened.execute("from entries | filter id == 2 | select {label}");
    assert!(result.ok, "{}", result.message);
    assert!(result.rows[0]["label"].cmp_eq(&Value::Text("untitled".into())));
}

#[test]
fn snapshot_watermark_skips_overlapping_commits_without_duplicate_rows() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("db.snapshot");
    let overlap;
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 0).unwrap();
        assert!(e.execute("create table t (id int)").ok);
        assert!(e.execute("insert t {id:1}").ok);
        overlap = std::fs::read(&wal).unwrap();
        e.checkpoint().unwrap();
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    }
    // Simulate publication of the new snapshot before removal of the old WAL.
    std::fs::write(&wal, overlap).unwrap();
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 0).unwrap();
        assert_eq!(e.execute("from t").rows.len(), 1);
        assert!(e.execute("insert t {id:2}").ok);
    }
    let mut e = Engine::open(Some(wal), Some(snapshot), 0).unwrap();
    assert_eq!(e.execute("from t").rows.len(), 2);
}

#[test]
fn failed_wal_write_does_not_publish_memory_changes() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let backup = dir.0.join("before.wal");
    {
        let mut e = Engine::open(Some(wal.clone()), None, 0).unwrap();
        assert!(e.execute("create table t (id int)").ok);
        std::fs::rename(&wal, &backup).unwrap();
        std::fs::create_dir(&wal).unwrap();
        let failed = e.execute("insert t {id:1}");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_STORAGE");
        let read = e.execute("from t");
        assert!(read.ok);
        assert!(read.rows.is_empty());
        assert!(!e.execute("insert t {id:2}").ok);
    }
    std::fs::remove_dir(&wal).unwrap();
    std::fs::rename(backup, &wal).unwrap();
    let mut e = Engine::open(Some(wal), None, 0).unwrap();
    assert!(e.execute("from t").rows.is_empty());
}

#[test]
fn checkpoint_cannot_erase_an_uncertain_wal_commit() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("db.snapshot");
    let backup = dir.0.join("before.wal");
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 0).unwrap();
        assert!(e.execute("create table t (id int)").ok);
        std::fs::rename(&wal, &backup).unwrap();
        std::fs::create_dir(&wal).unwrap();
        assert!(!e.execute("insert t {id:1}").ok);
        std::fs::remove_dir(&wal).unwrap();
        // Model the on-disk outcome of a complete append followed by a sync
        // error: memory stayed unchanged, but recovery may find the whole write.
        let mut uncertain = std::fs::read_to_string(&backup).unwrap();
        uncertain.push_str(
            &serde_json::json!({
                "format_version": 1, "sequence": 2, "source": "insert t {id:1}"
            })
            .to_string(),
        );
        uncertain.push('\n');
        std::fs::write(&wal, &uncertain).unwrap();
        assert!(e.execute("from t").rows.is_empty());
        assert_eq!(e.checkpoint().unwrap_err().code, "E_STORAGE");
        assert!(!snapshot.exists());
        assert_eq!(std::fs::read_to_string(&wal).unwrap(), uncertain);
    }
    let mut e = Engine::open(Some(wal), Some(snapshot), 0).unwrap();
    assert_eq!(e.execute("from t").rows.len(), 1);
}

#[test]
fn checkpoint_failure_is_reported_as_warning_after_successful_commit() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("snapshot");
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 1).unwrap();
        std::fs::create_dir(&snapshot).unwrap();
        let r = e.execute("create table t (id int)\ninsert t {id:1}");
        assert!(r.ok, "{}", r.message);
        assert_eq!(r.warnings.len(), 1);
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    }
    std::fs::remove_dir(&snapshot).unwrap();
    let mut e = Engine::open(Some(wal), Some(snapshot), 1).unwrap();
    assert_eq!(e.execute("from t").rows.len(), 1);
}

#[test]
fn unreadable_snapshot_never_replaces_recovery_state_or_truncates_wal() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("db.snapshot");
    let mut source = "type N0 = int\n".to_string();
    for n in 1..=60 {
        source.push_str(&format!("type N{n} = N{}\n", n - 1));
    }
    source.push_str("type Deep = {value N60}\ntable deep Deep\ninsert deep {value = 1}");
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 1).unwrap();
        assert!(e.execute("create table baseline (id int)").ok);
        let previous = std::fs::read(&snapshot).unwrap();
        let response = e.execute(&source);
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.warnings.len(), 1, "{response:?}");
        assert_eq!(std::fs::read(&snapshot).unwrap(), previous);
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    }
    let mut e = Engine::open(Some(wal), Some(snapshot), 1).unwrap();
    assert_eq!(e.execute("from deep").rows.len(), 1);
}

#[test]
fn automatic_checkpoints_and_indexes_survive_reopen() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("snapshot");
    {
        let mut e = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 2).unwrap();
        assert!(e.execute("create table t (f float)").ok);
        assert!(e.execute("insert t {f:-0.0}").ok);
        assert!(snapshot.exists());
        assert!(e.execute("create index t (f)").ok);
        assert!(e.execute("insert t {f:0.0}").ok);
    }
    let mut e = Engine::open(Some(wal), Some(snapshot), 2).unwrap();
    assert_eq!(e.execute("from t | filter f == 0.0").rows.len(), 2);
}

#[test]
fn database_ownership_locks_are_released_on_drop() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let first = Engine::open(Some(wal.clone()), None, 0).unwrap();
    let error = Engine::open(Some(wal.clone()), None, 0)
        .err()
        .expect("second writer should fail");
    assert_eq!(error.code, "E_BUSY");
    drop(first);
    assert!(Engine::open(Some(wal), None, 0).is_ok());
}

#[test]
fn malformed_unknown_or_noncontiguous_logs_fail_closed() {
    for contents in [
        "{\"format_version\":1,\"sequence\":0,\"source\":\"create table t (id int)\"}\n",
        "{\"format_version\":99,\"sequence\":1,\"source\":\"create table t (id int)\"}\n",
        "{\"format_version\":1,\"sequence\":2,\"source\":\"create table t (id int)\"}\n",
        "{\"format_version\":1",
        "{broken}\n",
        "from t\n",
        "{\"format_version\":1,\"sequence\":1,\"source\":\"create table t (id int)\"}\n{\"format_version\":1,\"sequence\":1,\"source\":\"insert t {id:1}\"}\n",
    ] {
        let dir = TempDir::new();
        let wal = dir.0.join("db.wal");
        std::fs::write(&wal, contents).unwrap();
        assert!(
            Engine::open(Some(wal.clone()), None, 0).is_err(),
            "{contents}"
        );
        assert_eq!(std::fs::read_to_string(wal).unwrap(), contents);
    }
}

#[test]
fn oversized_wal_records_fail_closed_without_changing_the_file() {
    for (first, terminated) in [(b' ', false), (b' ', true), (b'{', true)] {
        let dir = TempDir::new();
        let wal = dir.0.join("db.wal");
        let mut contents = vec![b' '; unionid::wal::MAX_RECORD_BYTES + 1];
        contents[0] = first;
        if terminated {
            contents.push(b'\n');
        }
        std::fs::write(&wal, &contents).unwrap();
        let error = Engine::open(Some(wal.clone()), None, 0)
            .err()
            .expect("oversized WAL must fail");
        assert_eq!(error.code, "E_STORAGE");
        assert!(
            error.message.contains("WAL line 1: record exceeds"),
            "{error}"
        );
        assert_eq!(std::fs::read(&wal).unwrap(), contents);
    }
}

#[test]
fn maximally_escaped_source_round_trips_within_the_wal_record_limit() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let mut source = "create table t (id int)\n#".to_string();
    source.extend(std::iter::repeat_n(
        '\u{0001}',
        unionid::syntax::MAX_SOURCE_BYTES - source.len(),
    ));
    {
        let mut e = Engine::open(Some(wal.clone()), None, 0).unwrap();
        let response = e.execute(&source);
        assert!(response.ok, "{}", response.message);
        let length = std::fs::metadata(&wal).unwrap().len() as usize;
        assert!(length > unionid::wal::MAX_RECORD_BYTES - 512);
        assert!(length <= unionid::wal::MAX_RECORD_BYTES);
    }
    let mut e = Engine::open(Some(wal), None, 0).unwrap();
    assert!(e.execute("from t").ok);
}

#[test]
fn supported_original_prototype_wal_can_be_read() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    std::fs::write(&wal, "create table events (id int, kind enum(Login, Purchase(int,float)))\ninsert events {id:1,kind:Purchase(42,19.9)}\ncreate index events (id)\n").unwrap();
    let mut e = Engine::open(Some(wal), None, 0).unwrap();
    let r = e.execute("from events | filter id = 1");
    assert!(r.ok, "{}", r.message);
    assert_eq!(r.rows.len(), 1);
}

#[cfg(unix)]
#[test]
fn symlink_alias_cannot_open_a_second_writer() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let alias = dir.0.join("alias.wal");
    let first = Engine::open(Some(wal.clone()), None, 0).unwrap();
    std::os::unix::fs::symlink(&wal, &alias).unwrap();
    assert_eq!(
        Engine::open(Some(alias), None, 0).err().unwrap().code,
        "E_BUSY"
    );
    drop(first);
}
