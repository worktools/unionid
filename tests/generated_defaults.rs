mod common;
use std::collections::BTreeMap;
use unionid::{Engine, QueryResponse, Value};

const SETUP: &str = r#"sequence ids {start 1}
type Id = int
struct Item {id: Id, owner: text, public_id: uuid, created_at: timestamp}
table items: Item {
  key id
  default id = next(ids)
  default public_id = uuid_v7()
  default created_at = now()
}
create unique index items (owner)
"#;

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{}: {:?}", source, response.error);
    response
}

fn id(response: &QueryResponse, row: usize) -> i64 {
    let Value::Int(id) = response.rows[row]["id"].unwrapped() else {
        panic!("expected int ID")
    };
    *id
}

#[test]
fn candidate_allocations_share_time_and_follow_row_order_without_schema_drift() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let schema = engine.schema_info();
    let batch = ok(
        &mut engine,
        r#"insert many items [{owner: "a"}, {owner: "b"}] | returning {id, public_id, created_at}"#,
    );
    assert_eq!((id(&batch, 0), id(&batch, 1)), (1, 2));
    assert!(batch.rows[0]["created_at"].cmp_eq(&batch.rows[1]["created_at"]));
    assert!(!batch.rows[0]["public_id"].cmp_eq(&batch.rows[1]["public_id"]));
    let script = ok(
        &mut engine,
        r#"insert items {owner: "c"}
insert items {owner: "d"} | returning id"#,
    );
    assert_eq!(id(&script, 0), 4);
    assert_eq!(engine.schema_info(), schema);
    let source = engine.schema();
    assert!(source.contains("next(ids)"));
    assert_eq!(
        unionid::schema::check(&source).unwrap().schema.hash,
        schema.hash
    );
}

#[test]
fn failed_constraints_and_expectations_roll_back_all_allocations() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    for source in [
        r#"insert many items [{owner: "same"}, {owner: "same"}]"#,
        r#"insert items {owner: "a"}
expect affected == 2"#,
        r#"insert items {owner: "a"}
insert items {owner: "a"}"#,
        r#"insert items {owner: "a"}
insert items {owner: 1}"#,
    ] {
        assert!(
            !engine.execute(source).ok,
            "unexpectedly succeeded: {source}"
        );
        assert!(ok(&mut engine, "from items").rows.is_empty());
    }
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "valid"} | returning id"#
            ),
            0
        ),
        1
    );
}

#[test]
fn explicit_values_do_not_allocate_and_upsert_requires_its_key() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    ok(&mut engine, r#"insert items {id: 90, owner: "explicit"}"#);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "generated"} | returning id"#
            ),
            0
        ),
        1
    );
    let error = engine
        .execute(r#"upsert items {owner: "missing-key"}"#)
        .error
        .unwrap();
    assert_eq!(error.code, "E_FIELD");
    let replaced = ok(
        &mut engine,
        r#"upsert items {id: 90, owner: "replaced"} | returning id"#,
    );
    assert_eq!(id(&replaced, 0), 90);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "next"} | returning id"#
            ),
            0
        ),
        2
    );
}

#[test]
fn exhaust_once_and_replay_original_generated_values() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        &SETUP.replace("start 1", &format!("start {}", i64::MAX)),
    );
    let source = r#"insert items {owner: "last"} | returning {id, public_id, created_at}"#;
    let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let first = engine
        .execute_idempotent_with_params("last", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert_eq!(id(&first.response, 0), i64::MAX);
    let replay = engine
        .execute_idempotent_with_params("last", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(
        serde_json::to_value(first.response.rows).unwrap(),
        serde_json::to_value(replay.response.rows).unwrap()
    );
    let failed = engine.execute(r#"insert items {owner: "exhausted"}"#);
    assert_eq!(failed.error.unwrap().code, "E_ARITH");
    assert_eq!(ok(&mut engine, "from items").rows.len(), 1);
}

#[test]
fn binding_rejects_wrong_fields_types_and_sequences_before_publication() {
    for declaration in [
        "default id = next(missing)",
        "default owner = next(ids)",
        "default id = uuid_v7()",
        "default created_at = next(ids)",
        "default missing = now()",
    ] {
        let mut engine = Engine::memory();
        let source = format!(
            "sequence ids {{start 1}}\nstruct Item {{id: int, owner: text, created_at: timestamp}}\ntable items: Item {{{declaration}}}"
        );
        assert!(!engine.execute(&source).ok);
        assert!(engine.schema().is_empty());
    }
}

#[test]
fn prepared_omitted_fields_explain_and_read_only_do_not_consume_sequences() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let prepared = engine
        .prepare("insert items $row | returning {id, public_id, created_at}")
        .unwrap();
    ok(&mut engine, r#"explain insert items {owner: "planned"}"#);
    let parameters = BTreeMap::from([(
        "row".into(),
        Value::Record(BTreeMap::from([(
            "owner".into(),
            Value::Text("first".into()),
        )])),
    )]);
    let mut engine = engine.with_read_only(true);
    let rejected = engine.execute_prepared(&prepared, parameters.clone());
    assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
    let mut engine = engine.with_read_only(false);
    let inserted = engine.execute_prepared(&prepared, parameters);
    assert!(inserted.ok, "{:?}", inserted.error);
    assert_eq!(id(&inserted, 0), 1);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "second"} | returning id"#
            ),
            0
        ),
        2
    );
}

#[test]
fn native_storage_requires_explicit_install_and_preserves_counters_through_restart_and_backup() {
    let temp = common::TempDir::new();
    let path = temp.0.join("source.redb");
    let backup = temp.0.join("logical.json");
    let target = temp.0.join("restored.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        engine.execute(SETUP).error.unwrap().code,
        "E_STORAGE_UPGRADE_REQUIRED"
    );
    engine.upgrade_storage(14).unwrap();
    assert_eq!(
        engine.execute(SETUP).error.unwrap().code,
        "E_STORAGE_UPGRADE_REQUIRED"
    );
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    ok(&mut engine, SETUP);
    let first = ok(
        &mut engine,
        r#"insert items {owner: "first"} | returning {id, public_id, created_at}"#,
    );
    assert_eq!(id(&first, 0), 1);
    let schema = engine.schema_info();
    let profile = engine.last_mutation_profile().unwrap();
    assert_eq!(
        profile.durable.unwrap().mode,
        unionid::profile::DurableCommitMode::Incremental
    );
    engine.check_integrity().unwrap();
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        id(
            &ok(
                &mut engine,
                r#"insert items {owner: "second"} | returning id"#
            ),
            0
        ),
        2
    );
    assert_eq!(engine.schema_info(), schema);
    let expected = serde_json::to_value(ok(&mut engine, "from items | sort id").rows).unwrap();
    drop(engine);
    let info = unionid::backup::create(&path, &backup).unwrap();
    assert_eq!(info.format_version, 8);
    unionid::backup::restore(&backup, &target).unwrap();
    let mut restored = Engine::open_redb(&target).unwrap();
    restored.check_integrity().unwrap();
    assert_eq!(restored.schema_info(), schema);
    assert_eq!(
        serde_json::to_value(ok(&mut restored, "from items | sort id").rows).unwrap(),
        expected
    );
    assert_eq!(
        id(
            &ok(
                &mut restored,
                r#"insert items {owner: "third"} | returning id"#
            ),
            0
        ),
        3
    );
}

#[test]
fn journal_restore_uses_materialized_values_and_the_selected_counter_boundary() {
    use unionid::backup::incremental::{export, init, restore, verify};
    let temp = common::TempDir::new();
    let path = temp.0.join("journal-source.redb");
    let repo = temp.0.join("archive");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    ok(&mut engine, SETUP);
    drop(engine);
    init(&path, &repo, Default::default()).unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    let baseline = engine.backup_journal_status().unwrap().head_sequence;
    let mut boundaries = vec![(baseline, serde_json::json!([]), 1)];
    for (owner, next_id) in [("first", 2), ("second", 3)] {
        ok(
            &mut engine,
            &format!(
                "insert items {{owner: \"{owner}\"}} | returning {{id, public_id, created_at}}"
            ),
        );
        let sequence = engine.backup_journal_status().unwrap().head_sequence;
        let rows = serde_json::to_value(ok(&mut engine, "from items | sort id").rows).unwrap();
        boundaries.push((sequence, rows, next_id));
    }
    drop(engine);
    export(&path, &repo, Default::default()).unwrap();
    verify(&repo, Default::default()).unwrap();
    for (sequence, expected, next_id) in boundaries {
        let target = temp.0.join(format!("at-{sequence}.redb"));
        restore(&repo, &target, sequence, Default::default()).unwrap();
        let mut engine = Engine::open_redb(&target).unwrap();
        engine.check_integrity().unwrap();
        assert_eq!(
            serde_json::to_value(ok(&mut engine, "from items | sort id").rows).unwrap(),
            expected
        );
        assert_eq!(
            id(
                &ok(
                    &mut engine,
                    r#"insert items {owner: "continued"} | returning id"#
                ),
                0
            ),
            next_id
        );
    }
}

#[test]
fn legacy_recovery_rejects_generators_without_publishing_or_replaying_them() {
    use unionid::{db::Database, snapshot::SnapshotStore, wal::Wal};
    let temp = common::TempDir::new();
    let wal = Wal::new(temp.0.join("legacy.wal")).unwrap();
    wal.append(1, "sequence ids {start 1}").unwrap();
    let mut db = Database::default();
    let before = serde_json::to_value(&db).unwrap();
    let error = wal.replay_into(&mut db).unwrap_err();
    assert!(error.contains("generated defaults"), "{error}");
    assert_eq!(serde_json::to_value(&db).unwrap(), before);

    for statement in unionid::syntax::parse("sequence ids {start 1}").unwrap() {
        db.execute(statement.statement).unwrap();
    }
    let snapshot_path = temp.0.join("legacy.snapshot");
    let snapshot = SnapshotStore::new(&snapshot_path).unwrap();
    assert!(
        snapshot
            .save(&db)
            .unwrap_err()
            .contains("E_STORAGE_UPGRADE_REQUIRED")
    );
    assert!(!snapshot_path.exists());
    std::fs::write(&snapshot_path, serde_json::to_vec(&db).unwrap()).unwrap();
    assert!(
        snapshot
            .load()
            .unwrap_err()
            .contains("E_STORAGE_UPGRADE_REQUIRED")
    );
    let before = serde_json::to_value(&db).unwrap();
    assert!(
        wal.replay_into(&mut db)
            .unwrap_err()
            .contains("E_STORAGE_UPGRADE_REQUIRED")
    );
    assert_eq!(serde_json::to_value(&db).unwrap(), before);
}

#[test]
fn sequence_and_policy_migrations_preserve_history_and_counter_identity() {
    let temp = common::TempDir::new();
    let path = temp.0.join("migrations.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    ok(
        &mut engine,
        "migration initial {\nadd sequence ids {start 7}\nadd struct Item {id: int, owner: text}\nadd table items: Item {key id, default id = next(ids)}\n}",
    );
    let first = ok(
        &mut engine,
        "insert items {owner: \"first\"} | returning id",
    );
    assert_eq!(id(&first, 0), 7);
    let before = engine.schema_info();
    ok(
        &mut engine,
        "migration rename {\nrename sequence ids to item_ids\nrename field Item.id to key\n}",
    );
    assert_ne!(engine.schema_info().hash, before.hash);
    assert!(engine.schema().contains("next(item_ids)"));
    let next = ok(
        &mut engine,
        "insert items {owner: \"second\"} | returning key",
    );
    assert!(next.rows[0]["key"].cmp_eq(&Value::Int(8)));
    let rows = ok(&mut engine, "from items | sort key").rows;
    let schema = engine.schema_info();
    for source in [
        "migration bad {drop sequence item_ids}",
        "migration bad {change default items.key to now()}",
        "migration bad {add sequence items {start 1}}",
        "migration bad {rename table items to item_ids}",
        "migration bad {\ndrop key items\nchange field Item.key to text using old -> \"x\"\n}",
    ] {
        assert!(!engine.execute(source).ok, "{source}");
        assert_eq!(engine.schema_info(), schema);
        assert_eq!(
            serde_json::to_value(ok(&mut engine, "from items | sort key").rows).unwrap(),
            serde_json::to_value(&rows).unwrap()
        );
    }
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    let third = ok(
        &mut engine,
        "insert items {owner: \"third\"} | returning key",
    );
    assert!(third.rows[0]["key"].cmp_eq(&Value::Int(9)));
    ok(
        &mut engine,
        "migration switch {\nadd sequence replacement {start 100}\nchange default items.key to next(replacement)\ndrop sequence item_ids\n}",
    );
    let replacement = ok(
        &mut engine,
        "insert items {owner: \"replacement\"} | returning key",
    );
    assert!(replacement.rows[0]["key"].cmp_eq(&Value::Int(100)));
    ok(
        &mut engine,
        "migration remove {\ndrop default items.key\ndrop sequence replacement\n}",
    );
    assert!(!engine.execute("insert items {owner: \"missing\"}").ok);
    ok(&mut engine, "insert items {key: 101, owner: \"explicit\"}");
    drop(engine);
    Engine::open_redb(&path).unwrap().check_integrity().unwrap();
}

#[test]
fn shadow_abort_and_reopen_resume_preserve_source_and_target_sequences() {
    use unionid::migration::MigrationFile;
    let temp = common::TempDir::new();
    let path = temp.0.join("shadow.redb");
    let initial = MigrationFile::parse("migration initial {\nadd sequence ids {start 1}\nadd struct Item {id: int, owner: text}\nadd table items: Item {key id, default id = next(ids)}\n}").unwrap();
    let expanded = MigrationFile::parse("migration expanded {\nparent initial\nrename sequence ids to item_ids\nadd sequence public_ids {start 50}\nadd field Item.public_id: int = 0\nadd field Item.created_at: timestamp = @2020-01-01T00:00:00Z\nchange default items.public_id to next(public_ids)\nchange default items.created_at to now()\n}").unwrap();
    let files = [initial.clone(), expanded];
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    engine.apply_migrations(&[initial]).unwrap();
    ok(
        &mut engine,
        "insert many items [{owner: \"a\"}, {owner: \"b\"}, {owner: \"c\"}]",
    );
    let schema = engine.schema_info();
    let rows = serde_json::to_value(ok(&mut engine, "from items | sort id").rows).unwrap();
    engine.advance_migrations(&files, 1).unwrap();
    assert!(
        engine
            .migration_status(&files)
            .unwrap()
            .maintenance
            .is_some()
    );
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.abort_migration().unwrap();
    assert_eq!(engine.schema_info(), schema);
    assert_eq!(
        serde_json::to_value(ok(&mut engine, "from items | sort id").rows).unwrap(),
        rows
    );
    assert!(!engine.schema().contains("public_ids"));
    let mut completed = false;
    for _ in 0..32 {
        engine.advance_migrations(&files, 1).unwrap();
        drop(engine);
        engine = Engine::open_redb(&path).unwrap();
        engine.check_integrity().unwrap();
        let status = engine.migration_status(&files).unwrap();
        if status.maintenance.is_none() && engine.migration_history().len() == 2 {
            completed = true;
            break;
        }
    }
    assert!(
        completed,
        "migration did not finish within 32 bounded steps"
    );
    let historical = ok(&mut engine, "from items | sort id");
    assert_eq!(historical.rows.len(), 3);
    for row in &historical.rows {
        assert!(row["public_id"].cmp_eq(&Value::Int(0)));
        let Value::Timestamp(time) = row["created_at"].unwrapped() else {
            panic!("expected timestamp")
        };
        assert_eq!(time.epoch_microseconds(), 1_577_836_800_000_000);
    }
    let fresh = ok(
        &mut engine,
        "insert items {owner: \"fresh\"} | returning {id, public_id, created_at}",
    );
    assert_eq!(id(&fresh, 0), 4);
    assert!(fresh.rows[0]["public_id"].cmp_eq(&Value::Int(50)));
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    let next = ok(
        &mut engine,
        "insert items {owner: \"next\"} | returning {id, public_id}",
    );
    assert_eq!(id(&next, 0), 5);
    assert!(next.rows[0]["public_id"].cmp_eq(&Value::Int(51)));
    engine.check_integrity().unwrap();
}

#[test]
fn counter_only_effects_receipt_pruning_compaction_and_journal_restore_do_not_rewind() {
    use unionid::IdempotencyPruneOptions;
    use unionid::backup::incremental::{export, init, restore, verify};
    let temp = common::TempDir::new();
    let path = temp.0.join("counter-only.redb");
    let archive = temp.0.join("archive");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    ok(&mut engine, SETUP);
    drop(engine);
    init(&path, &archive, Default::default()).unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    let baseline = engine.backup_journal_status().unwrap().head_sequence;
    let source = "insert items {owner: \"transient\"}\ndelete items | filter owner == \"transient\" | returning id";
    let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let first = engine
        .execute_idempotent_with_params("net-empty", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert_eq!(id(&first.response, 0), 1);
    let durable = engine.last_mutation_profile().unwrap().durable.unwrap();
    assert_eq!(
        durable.mode,
        unionid::profile::DurableCommitMode::Incremental
    );
    assert_eq!(durable.row_changes, 0);
    assert!(durable.catalog_changes > 0);
    assert_eq!(durable.receipt_changes, 1);
    assert!(ok(&mut engine, "from items").rows.is_empty());
    let boundary = engine.backup_journal_status().unwrap().head_sequence;
    drop(engine);
    export(&path, &archive, Default::default()).unwrap();
    verify(&archive, Default::default()).unwrap();
    for (sequence, expected_id) in [(baseline, 1), (boundary, 2)] {
        let target = temp.0.join(format!("restore-{sequence}.redb"));
        restore(&archive, &target, sequence, Default::default()).unwrap();
        let mut restored = Engine::open_redb(&target).unwrap();
        assert!(ok(&mut restored, "from items").rows.is_empty());
        if sequence == boundary {
            let replay = restored
                .execute_idempotent_with_params("net-empty", digest, source, BTreeMap::new(), None)
                .unwrap();
            assert!(replay.replayed);
            assert_eq!(id(&replay.response, 0), 1);
        }
        assert_eq!(
            id(
                &ok(
                    &mut restored,
                    "insert items {owner: \"continued\"} | returning id"
                ),
                0
            ),
            expected_id
        );
        restored.check_integrity().unwrap();
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        id(
            &ok(&mut engine, "insert items {owner: \"kept\"} | returning id"),
            0
        ),
        2
    );
    let pruned = engine
        .prune_idempotency_receipts(IdempotencyPruneOptions {
            completed_before_unix_ms: None,
            committed_through_sequence: Some(boundary),
            max_receipts: 1,
        })
        .unwrap();
    assert_eq!(pruned.selected_count, 1);
    let reused = engine
        .execute_idempotent_with_params("net-empty", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert!(!reused.replayed);
    assert_eq!(id(&reused.response, 0), 3);
    engine.compact_storage().unwrap();
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.check_integrity().unwrap();
    let replay = engine
        .execute_idempotent_with_params("net-empty", digest, source, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(id(&replay.response, 0), 3);
    assert_eq!(
        id(
            &ok(
                &mut engine,
                "insert items {owner: \"after compact\"} | returning id"
            ),
            0
        ),
        4
    );
}
