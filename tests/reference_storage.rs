mod common;

use common::TempDir;
use unionid::{DurableCommitMode, Engine, backup, error::ConstraintKind};

const SCHEMA: &str = "struct Parent {id: int}\nstruct Child {id: int, parent: Option<int>}\ntable parents: Parent {key id}\ntable children: Child {key id}";
const REFERENCE: &str = "create reference children (parent) references parents (id)";

fn ok(engine: &mut Engine, source: &str) {
    let response = engine.execute(source);
    assert!(response.ok, "{source}: {}", response.message);
}

fn rejected(engine: &mut Engine, source: &str, kind: ConstraintKind) {
    let response = engine.execute(source);
    assert!(!response.ok, "{source}");
    assert_eq!(response.error.unwrap().constraint, Some(kind));
}

#[test]
fn explicit_upgrade_preserves_incremental_reference_writes_across_restart_and_restore() {
    let dir = TempDir::new();
    let path = dir.0.join("references.redb");
    let archive = dir.0.join("references.backup.json");
    let restored = dir.0.join("restored.redb");
    let schema;
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        ok(&mut engine, SCHEMA);
        let before = engine.schema_info();
        assert_eq!(
            engine.execute(REFERENCE).error.unwrap().code,
            "E_STORAGE_UPGRADE_REQUIRED"
        );
        assert_eq!(engine.schema_info(), before);
        let upgrade = engine.upgrade_storage(12).unwrap();
        assert!(upgrade.changed);
        assert!(!engine.upgrade_storage(12).unwrap().changed);
        assert_eq!(
            engine.upgrade_storage(10).unwrap_err().code,
            "E_STORAGE_UPGRADE"
        );
        ok(&mut engine, REFERENCE);
        ok(&mut engine, "insert parents {id: 7}");
        ok(&mut engine, "insert children {id: 1, parent: Some(7)}");
        let profile = engine.last_mutation_profile().unwrap();
        assert!(!profile.full_rebuild);
        assert_eq!(profile.row_inserts, 1);
        assert_eq!(profile.index_inserts, 2);
        assert_eq!(
            profile.durable.unwrap().mode,
            DurableCommitMode::Incremental
        );
        rejected(
            &mut engine,
            "insert children {id: 2, parent: Some(99)}",
            ConstraintKind::ReferenceMissing,
        );
        rejected(
            &mut engine,
            "delete parents",
            ConstraintKind::ReferenceRestricted,
        );
        schema = engine.schema_info();
        engine.check_integrity().unwrap();
    }
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert_eq!(engine.schema_info(), schema);
        rejected(
            &mut engine,
            "update parents | set id = 8",
            ConstraintKind::ReferenceRestricted,
        );
        ok(&mut engine, "insert children {id: 2, parent: None}");
        engine.check_integrity().unwrap();
    }
    assert_eq!(backup::create(&path, &archive).unwrap().format_version, 7);
    backup::restore(&archive, &restored).unwrap();
    let mut engine = Engine::open_redb(&restored).unwrap();
    assert_eq!(engine.schema_info(), schema);
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
    ok(&mut engine, "update children | set parent = None");
    assert_eq!(engine.last_mutation_profile().unwrap().index_deletes, 1);
    ok(&mut engine, "delete parents");
    engine.check_integrity().unwrap();
}

#[test]
fn durable_self_reference_batches_validate_final_state_and_rollback_whole_scripts() {
    let dir = TempDir::new();
    let path = dir.0.join("self.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(12).unwrap();
    ok(
        &mut engine,
        "struct Node {id: int, parent: int}\ntable nodes: Node {key id}\ncreate reference nodes (parent) references nodes (id)",
    );
    ok(
        &mut engine,
        "insert many nodes [{id: 1, parent: 2}, {id: 2, parent: 1}]",
    );
    assert!(!engine.last_mutation_profile().unwrap().full_rebuild);
    rejected(
        &mut engine,
        "delete nodes | filter id == 1",
        ConstraintKind::ReferenceRestricted,
    );
    ok(
        &mut engine,
        "update nodes | set {id = id + 10, parent = parent + 10}",
    );
    assert!(!engine.last_mutation_profile().unwrap().full_rebuild);
    rejected(
        &mut engine,
        "delete nodes\ninsert nodes {id: 3, parent: 99}",
        ConstraintKind::ReferenceMissing,
    );
    assert_eq!(engine.execute("from nodes").rows.len(), 2);
    engine.check_integrity().unwrap();
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    ok(&mut engine, "delete nodes");
    assert!(!engine.last_mutation_profile().unwrap().full_rebuild);
    engine.check_integrity().unwrap();
}

#[test]
fn reference_journal_replays_only_complete_constraint_preserving_commits() {
    use backup::incremental::{ArchiveLimits, IncrementalExportOptions, IncrementalInitOptions};
    let dir = TempDir::new();
    let path = dir.0.join("journal.redb");
    let repo = dir.0.join("archive");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        ok(&mut engine, SCHEMA);
        ok(&mut engine, REFERENCE);
        ok(&mut engine, "insert parents {id: 7}");
    }
    let initialized =
        backup::incremental::init(&path, &repo, IncrementalInitOptions::default()).unwrap();
    assert_eq!(initialized.previous_storage_format, 12);
    assert_eq!(initialized.current_storage_format, 13);
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        ok(&mut engine, "insert children {id: 1, parent: Some(7)}");
        rejected(
            &mut engine,
            "delete parents",
            ConstraintKind::ReferenceRestricted,
        );
        ok(&mut engine, "update children | set parent = None");
        ok(&mut engine, "delete parents");
    }
    let exported =
        backup::incremental::export(&path, &repo, IncrementalExportOptions::default()).unwrap();
    assert_eq!(exported.exported_commits, 3);
    for (offset, restricted) in [(1, true), (3, false)] {
        let restored = dir.0.join(format!("restored-{offset}.redb"));
        backup::incremental::restore(
            &repo,
            &restored,
            initialized.baseline_sequence + offset,
            ArchiveLimits::default(),
        )
        .unwrap();
        let mut engine = Engine::open_redb(&restored).unwrap();
        engine.check_integrity().unwrap();
        if restricted {
            rejected(
                &mut engine,
                "delete parents",
                ConstraintKind::ReferenceRestricted,
            );
        } else {
            rejected(
                &mut engine,
                "update children | set parent = Some(7)",
                ConstraintKind::ReferenceMissing,
            );
        }
    }
}

#[test]
fn durable_reference_migration_checks_existing_rows_and_preserves_failed_ledger() {
    let dir = TempDir::new();
    let path = dir.0.join("migration.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(12).unwrap();
    ok(&mut engine, SCHEMA);
    ok(&mut engine, "insert children {id: 1, parent: Some(7)}");
    let before = engine.schema_info();
    let migration = unionid::MigrationFile::parse(
        "migration m0001_refs\n  add reference children (parent) references parents (id)",
    )
    .unwrap();
    let error = engine
        .apply_migrations(std::slice::from_ref(&migration))
        .unwrap_err();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert_eq!(error.constraint, Some(ConstraintKind::ReferenceMissing));
    assert_eq!(engine.schema_info(), before);
    ok(&mut engine, "insert parents {id: 7}");
    engine.apply_migrations(&[migration]).unwrap();
    engine.check_integrity().unwrap();
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
}

#[test]
fn active_format_eleven_journal_can_explicitly_upgrade_to_thirteen() {
    use backup::incremental::{ArchiveLimits, IncrementalExportOptions, IncrementalInitOptions};
    let dir = TempDir::new();
    let path = dir.0.join("upgrade-journal.redb");
    let repo = dir.0.join("chain");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        ok(&mut engine, SCHEMA);
        ok(&mut engine, "insert parents {id: 7}");
    }
    let baseline =
        backup::incremental::init(&path, &repo, IncrementalInitOptions::default()).unwrap();
    assert_eq!(baseline.current_storage_format, 11);
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert_eq!(
            engine.execute(REFERENCE).error.unwrap().code,
            "E_STORAGE_UPGRADE_REQUIRED"
        );
        assert!(engine.upgrade_storage(13).unwrap().changed);
        ok(&mut engine, REFERENCE);
        ok(&mut engine, "insert children {id: 1, parent: Some(7)}");
    }
    let exported =
        backup::incremental::export(&path, &repo, IncrementalExportOptions::default()).unwrap();
    assert_eq!(exported.exported_commits, 3);
    let restored = dir.0.join("restored.redb");
    backup::incremental::restore(
        &repo,
        &restored,
        baseline.baseline_sequence + 3,
        ArchiveLimits::default(),
    )
    .unwrap();
    let mut engine = Engine::open_redb(&restored).unwrap();
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
    engine.check_integrity().unwrap();
}

#[test]
fn reference_migration_resumes_across_child_first_batches_and_reclaims_after_cutover() {
    use unionid::migration::MigrationMaintenancePhase;
    let dir = TempDir::new();
    let path = dir.0.join("maintenance.redb");
    let before;
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        ok(
            &mut engine,
            "struct Parent {id: int}\nstruct Child {id: int, parent: Option<int>}\ntable children: Child {key id}\ntable parents: Parent {key id}\ninsert parents {id: 7}",
        );
        let rows = (0..1500)
            .map(|id| format!("{{id: {id}, parent: Some(7)}}"))
            .collect::<Vec<_>>()
            .join(",");
        ok(&mut engine, &format!("insert many children [{rows}]"));
        before = engine.schema_info();
    }
    let files = [unionid::MigrationFile::parse(
        "migration m0001_refs\n  add reference children (parent) references parents (id)",
    )
    .unwrap()];
    let mut saw_child_batch = false;
    let mut saw_ready = false;
    let mut saw_reclaim = false;
    let mut completed = false;
    for _ in 0..32 {
        let mut engine = Engine::open_redb(&path).unwrap();
        let progress = engine.advance_migrations(&files, 1).unwrap();
        assert!(progress.committed_steps <= 1);
        if let Some(maintenance) = &progress.status.maintenance {
            match maintenance.phase {
                MigrationMaintenancePhase::Building => {
                    assert_eq!(engine.schema_info(), before);
                    if maintenance.source_rows_seen == 1024 {
                        saw_child_batch = true;
                    }
                }
                MigrationMaintenancePhase::Ready => {
                    saw_ready = true;
                    assert_eq!(engine.schema_info(), before);
                }
                MigrationMaintenancePhase::Reclaimable => saw_reclaim = true,
                MigrationMaintenancePhase::Aborting => panic!("valid reference migration aborted"),
            }
        }
        if progress.complete {
            completed = true;
            assert_eq!(engine.execute("from children").rows.len(), 1500);
            rejected(
                &mut engine,
                "delete parents",
                ConstraintKind::ReferenceRestricted,
            );
            engine.check_integrity().unwrap();
            break;
        }
    }
    assert!(completed && saw_child_batch && saw_ready && saw_reclaim);
    let rename = unionid::MigrationFile::parse("migration m0002_rename {\nparent m0001_refs\nrename table parents to owners\nrename field Parent.id to owner_id\nrename field Child.parent to owner\n}").unwrap();
    let mut engine = Engine::open_redb(&path).unwrap();
    engine
        .apply_migrations(&[files[0].clone(), rename])
        .unwrap();
    assert!(engine.schema().contains("references owners (owner_id)"));
    rejected(
        &mut engine,
        "delete owners",
        ConstraintKind::ReferenceRestricted,
    );
    engine.check_integrity().unwrap();
}

#[test]
fn abort_discards_unpublished_orphan_reference_batches() {
    let dir = TempDir::new();
    let path = dir.0.join("abort.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(12).unwrap();
    ok(&mut engine, SCHEMA);
    ok(&mut engine, "insert children {id: 1, parent: Some(7)}");
    let schema = engine.schema_info();
    let files = [unionid::MigrationFile::parse(
        "migration m0001_refs\n  add reference children (parent) references parents (id)",
    )
    .unwrap()];
    engine.advance_migrations(&files, 1).unwrap();
    let progress = engine.advance_migrations(&files, 1).unwrap();
    assert_eq!(progress.status.maintenance.unwrap().source_rows_seen, 1);
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    assert!(engine.abort_migration().unwrap().cleaned);
    assert_eq!(engine.schema_info(), schema);
    assert!(engine.migration_history().is_empty());
    engine.check_integrity().unwrap();
    ok(&mut engine, "insert parents {id: 7}");
    engine.apply_migrations(&files).unwrap();
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
}

#[test]
fn reference_migration_cutover_is_one_restorable_journal_commit() {
    use backup::incremental::{ArchiveLimits, IncrementalExportOptions, IncrementalInitOptions};
    let dir = TempDir::new();
    let path = dir.0.join("migration-journal.redb");
    let repo = dir.0.join("chain");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        ok(&mut engine, SCHEMA);
        ok(
            &mut engine,
            "insert parents {id: 7}\ninsert children {id: 1, parent: Some(7)}",
        );
    }
    let baseline =
        backup::incremental::init(&path, &repo, IncrementalInitOptions::default()).unwrap();
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let file = unionid::MigrationFile::parse(
            "migration m0001_refs\n  add reference children (parent) references parents (id)",
        )
        .unwrap();
        engine.apply_migrations(&[file]).unwrap();
    }
    assert_eq!(
        backup::incremental::export(&path, &repo, IncrementalExportOptions::default())
            .unwrap()
            .exported_commits,
        1
    );
    let restored = dir.0.join("restored.redb");
    backup::incremental::restore(
        &repo,
        &restored,
        baseline.baseline_sequence + 1,
        ArchiveLimits::default(),
    )
    .unwrap();
    let mut engine = Engine::open_redb(restored).unwrap();
    assert_eq!(engine.migration_history().len(), 1);
    assert_eq!(engine.execute("from parents").rows.len(), 1);
    assert_eq!(engine.execute("from children").rows.len(), 1);
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
    engine.check_integrity().unwrap();
}

#[test]
fn reference_open_keeps_bounded_view_and_defers_posting_scan_to_explicit_check() {
    use redb::{Database as RedbDatabase, Durability, TableDefinition};
    const INDEXES: TableDefinition<&[u8], u8> = TableDefinition::new("generation_index");
    let dir = TempDir::new();
    let path = dir.0.join("bounded-reference-open.redb");
    let schema;
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        ok(&mut engine, SCHEMA);
        ok(&mut engine, REFERENCE);
        ok(
            &mut engine,
            "insert parents {id: 7}\ninsert children {id: 1, parent: Some(7)}",
        );
        schema = engine.schema_info();
        engine.check_integrity().unwrap();
    }
    // Missing postings are an at-rest logical corruption. If open scans them,
    // this fixture fails before the explicit check and breaks the open contract.
    {
        let database = RedbDatabase::open(&path).unwrap();
        let mut transaction = database.begin_write().unwrap();
        transaction.set_durability(Durability::Immediate).unwrap();
        transaction.set_two_phase_commit(true);
        transaction
            .open_table(INDEXES)
            .unwrap()
            .retain(|_, _| false)
            .unwrap();
        transaction.commit().unwrap();
    }
    for read_only in [false, true] {
        let mut engine = if read_only {
            Engine::open_redb_read_only(&path).unwrap()
        } else {
            Engine::open_redb(&path).unwrap()
        };
        assert_eq!(engine.schema_info(), schema);
        let profile = engine.open_profile().unwrap();
        assert!(profile.bounded_view);
        assert_eq!(profile.row_entries, 0);
        assert_eq!(profile.index_entries, 0);
        assert_eq!(profile.row_bytes, 0);
        assert_eq!(profile.index_key_bytes, 0);
        assert_eq!(engine.check_integrity().unwrap_err().code, "E_STORAGE");
    }
}
