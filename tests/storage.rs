mod common;
use common::TempDir;
use redb::{Database as RedbDatabase, Durability, ReadableDatabase, TableDefinition};
use unionid::{Engine, Value};

const REDB_META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const REDB_CATALOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("catalog");
const REDB_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("rows");
const REDB_SECONDARY_INDEX: TableDefinition<&[u8], u8> = TableDefinition::new("secondary_index");
const REDB_MIGRATION_LEDGER: TableDefinition<u64, &[u8]> = TableDefinition::new("migration_ledger");

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
