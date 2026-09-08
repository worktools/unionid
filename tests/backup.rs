mod common;

use common::TempDir;
use sha2::{Digest, Sha256};
use unionid::backup;
use unionid::migration::load_directory;
use unionid::{Engine, MigrationFile, Value};

#[test]
fn backup_v4_preserves_idempotency_receipts_and_replay_identity() {
    const DIGEST: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let dir = TempDir::new();
    let source = dir.0.join("idempotency-source.redb");
    let archive = dir.0.join("idempotency.backup.json");
    let restored = dir.0.join("idempotency-restored.redb");
    let committed_sequence;
    {
        let mut engine = Engine::open_redb(&source).unwrap();
        assert!(engine.execute("create table entries (id int)").ok);
        committed_sequence = engine
            .execute_idempotent_with_params(
                "entry-1",
                DIGEST,
                "insert entries {id = 1}\nreturning id",
                std::collections::BTreeMap::new(),
                None,
            )
            .unwrap()
            .committed_sequence;
    }

    let created = backup::create(&source, &archive).unwrap();
    assert_eq!(created.format_version, 4);
    assert_eq!(created.receipt_count, 1);
    let recovered = backup::restore(&archive, &restored).unwrap();
    assert_eq!(created, recovered);

    let mut engine = Engine::open_redb(&restored).unwrap();
    let replay = engine
        .execute_idempotent_with_params(
            "entry-1",
            DIGEST,
            "not parsed while replaying",
            std::collections::BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.committed_sequence, committed_sequence);
    assert_eq!(engine.execute("from entries").rows.len(), 1);
}

#[test]
fn backup_v3_single_column_indexes_restore_through_the_legacy_shape() {
    let dir = TempDir::new();
    let source = dir.0.join("backup3-source.redb");
    let current = dir.0.join("current.backup.json");
    let legacy = dir.0.join("legacy-v3.backup.json");
    let restored = dir.0.join("legacy-v3-restored.redb");
    {
        let mut engine = Engine::open_redb(&source).unwrap();
        assert!(engine
            .execute("type Entry = {id int, label text}\ntable entries Entry\n  key id\ncreate index entries (label)\ninsert entries {id = 1, label = \"saved\"}")
            .ok);
    }
    backup::create(&source, &current).unwrap();
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&current).unwrap()).unwrap();
    envelope["format_version"] = serde_json::json!(3);
    for table in envelope["database"]["index_definitions"]
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        for definition in table.as_object_mut().unwrap().values_mut() {
            let object = definition.as_object_mut().unwrap();
            let first = object["components"][0].clone();
            object.insert("column".into(), first["column"].clone());
            object.insert("field_path".into(), first["field_path"].clone());
            object.remove("components");
        }
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "database": envelope["database"].clone(),
        "receipts": envelope.get("receipts").cloned().unwrap_or_else(|| serde_json::json!({})),
    }))
    .unwrap();
    envelope["checksum"] = serde_json::json!(format!("sha256:{:x}", Sha256::digest(payload)));
    std::fs::write(&legacy, serde_json::to_vec(&envelope).unwrap()).unwrap();

    let info = backup::restore(&legacy, &restored).unwrap();
    assert_eq!(info.format_version, 3);
    let mut engine = Engine::open_redb(restored).unwrap();
    assert!(engine.schema().contains("create index entries (label)"));
    assert_eq!(
        engine
            .execute("from entries | filter label == \"saved\"")
            .rows
            .len(),
        1
    );
}

#[test]
fn backup_v4_round_trips_composite_index_shapes() {
    let dir = TempDir::new();
    let source = dir.0.join("composite-source.redb");
    let archive = dir.0.join("composite.backup.json");
    let restored = dir.0.join("composite-restored.redb");
    {
        let mut engine = Engine::open_redb(&source).unwrap();
        assert!(engine
            .execute("type Entry = {id int, tenant text, priority int}\ntable entries Entry\n  key id\ncreate unique index entries (tenant, -priority)\ninsert entries {id = 1, tenant = \"acme\", priority = 2}")
            .ok);
    }
    let created = backup::create(&source, &archive).unwrap();
    assert_eq!(created.format_version, 4);
    backup::restore(&archive, &restored).unwrap();

    let mut engine = Engine::open_redb(restored).unwrap();
    assert!(
        engine
            .schema()
            .contains("create unique index entries (tenant, -priority)")
    );
    let duplicate = engine.execute("insert entries {id = 2, tenant = \"acme\", priority = 2}");
    assert_eq!(duplicate.error.as_ref().unwrap().code, "E_CONSTRAINT");
    assert!(engine.check_integrity().unwrap().backend_clean);
}

#[test]
fn backup_restore_preserves_typed_data_schema_indexes_and_history() {
    let dir = TempDir::new();
    let source = dir.0.join("source.redb");
    let archive = dir.0.join("backup.json");
    let restored = dir.0.join("restored.redb");
    let mut files = load_directory("examples/migrations").unwrap();
    files.push(
        MigrationFile::parse(
            "migration m0003_unique_title\n  parent m0002_add_priority\n  add unique index tasks.title",
        )
        .unwrap(),
    );
    let expected_schema;
    {
        let mut engine = Engine::open_redb(&source).unwrap();
        engine.apply_migrations(&files).unwrap();
        assert!(
            engine
                .execute("insert tasks {id = 1, title = \"saved\", state = Running}")
                .ok
        );
        expected_schema = engine.schema_info();
    }
    let created = backup::create(&source, &archive).unwrap();
    let recovered = backup::restore(&archive, &restored).unwrap();
    assert_eq!(created, recovered);
    assert_eq!(recovered.schema, expected_schema);
    assert_eq!(recovered.migration_count, 3);

    let mut engine = Engine::open_redb(&restored).unwrap();
    assert_eq!(engine.migration_status(&files).unwrap().applied.len(), 3);
    assert!(
        engine
            .schema()
            .contains("create unique index tasks (title)")
    );
    let rows = engine.execute("from tasks | filter priority == 0");
    assert_eq!(rows.rows.len(), 1);
    assert!(rows.rows[0]["id"].cmp_eq(&Value::Int(1)));
    let duplicate =
        engine.execute("insert tasks {id = 2, title = \"saved\", state = Pending, priority = 1}");
    assert!(!duplicate.ok);
    assert_eq!(duplicate.error.unwrap().code, "E_CONSTRAINT");
}

#[test]
fn backup_restore_preserves_recursive_named_values() {
    let dir = TempDir::new();
    let source = dir.0.join("recursive.redb");
    let archive = dir.0.join("recursive.backup.json");
    let restored = dir.0.join("restored.redb");
    let expected_schema;
    {
        let mut engine = Engine::open_redb(&source).unwrap();
        let response = engine.execute(include_str!("../examples/recursive_tree.uid"));
        assert!(response.ok, "{}", response.message);
        expected_schema = engine.schema_info();
    }
    let created = backup::create(&source, &archive).unwrap();
    let recovered = backup::restore(&archive, &restored).unwrap();
    assert_eq!(created, recovered);
    assert_eq!(recovered.schema, expected_schema);

    let mut engine = Engine::open_redb(&restored).unwrap();
    let rows = engine.execute(
        r#"from documents
filter match tree
  Leaf value => value == "single"
  Branch {children, ..} => contains children (Tree.Leaf "readme")
sort id"#,
    );
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 2);
    assert!(engine.check_integrity().unwrap().backend_clean);
}

#[test]
fn corrupt_or_unknown_backups_do_not_create_or_replace_a_target() {
    let dir = TempDir::new();
    let source = dir.0.join("source.redb");
    let archive = dir.0.join("backup.json");
    let corrupt = dir.0.join("corrupt.json");
    let target = dir.0.join("target.redb");
    {
        let mut engine = Engine::open_redb(&source).unwrap();
        assert!(engine.execute("create table values (id int)").ok);
    }
    backup::create(&source, &archive).unwrap();
    let text = std::fs::read_to_string(&archive).unwrap();
    std::fs::write(
        &corrupt,
        text.replacen("\"format_version\":4", "\"format_version\":99", 1),
    )
    .unwrap();
    assert!(backup::restore(&corrupt, &target).is_err());
    assert!(!target.exists());

    let tampered = dir.0.join("tampered.json");
    let text = std::fs::read_to_string(&archive).unwrap();
    std::fs::write(
        &tampered,
        text.replacen("\"checksum\":\"sha256:", "\"checksum\":\"sha256:0", 1),
    )
    .unwrap();
    assert!(backup::restore(&tampered, &target).is_err());
    assert!(!target.exists());

    let mut existing = Engine::open_redb(&target).unwrap();
    assert!(existing.execute("create table keep (id int)").ok);
    drop(existing);
    assert!(backup::restore(&archive, &target).is_err());
    let mut existing = Engine::open_redb(&target).unwrap();
    assert!(existing.execute("from keep").ok);
}

#[test]
fn explicit_legacy_import_preserves_inputs_and_converts_supported_wal_snapshot() {
    let dir = TempDir::new();
    let snapshot = dir.0.join("legacy.snapshot");
    let wal = dir.0.join("legacy.wal");
    let target = dir.0.join("imported.redb");
    {
        let mut engine = Engine::open(Some(wal.clone()), Some(snapshot.clone()), 1).unwrap();
        assert!(engine
            .execute("create table legacy (id int, label text)\ninsert legacy {id = 1, label = \"old\"}")
            .ok);
    }
    let snapshot_before = std::fs::read(&snapshot).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    backup::import_legacy(Some(snapshot.clone()), Some(wal.clone()), &target).unwrap();
    assert_eq!(std::fs::read(snapshot).unwrap(), snapshot_before);
    assert_eq!(std::fs::read(wal).unwrap(), wal_before);
    let mut imported = Engine::open_redb(target).unwrap();
    assert_eq!(imported.execute("from legacy").rows.len(), 1);
}
