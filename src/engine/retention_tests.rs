use super::*;
use crate::ReceiptRetentionPolicy;
use std::collections::BTreeMap;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const QUERY: &str = "update counters | set value = value + 1 | returning {value}";

fn seeded() -> Engine {
    let mut engine = Engine::memory();
    assert!(engine.execute("struct Counter {id: int, value: int}\ntable counters Counter\n  key id\ninsert counters {id: 1, value: 0}").ok);
    for key in ["old-a", "old-b", "boundary", "recent", "future"] {
        engine
            .execute_idempotent_with_params(key, DIGEST, QUERY, BTreeMap::new(), None)
            .unwrap();
    }
    let receipts = Arc::make_mut(&mut Arc::make_mut(&mut engine.committed).receipts);
    for (key, time) in [
        ("old-a", 1_000),
        ("old-b", 1_000),
        ("boundary", 2_000),
        ("recent", 2_001),
        ("future", 5_000),
    ] {
        receipts.get_mut(key).unwrap().completed_at_unix_ms = time;
    }
    engine
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "unionid-retention-{:032x}",
            u128::from_le_bytes(nonce)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn retention_strict_window_batches_and_noop_preserve_rows_and_sequence() {
    let mut engine = seeded();
    let policy = ReceiptRetentionPolicy {
        min_age_seconds: 2,
        max_receipts: 1,
    };
    let before = engine.committed.db.sequence;
    let preview = engine.plan_idempotency_retention_at(policy, 4_000).unwrap();
    assert_eq!(preview.cutoff_unix_ms, Some(2_000));
    assert_eq!(preview.pruning.selected_count, 1);
    assert_eq!(preview.pruning.first_selected.unwrap().key, "old-a");
    assert!(!preview.pruning.applied);
    assert_eq!(engine.committed.db.sequence, before);
    for key in ["old-a", "old-b"] {
        let pass = engine
            .apply_idempotency_retention_at(policy, 4_000)
            .unwrap();
        assert_eq!(pass.pruning.selected_count, 1);
        assert_eq!(pass.pruning.first_selected.unwrap().key, key);
    }
    assert_eq!(engine.committed.db.sequence, before + 2);
    let noop = engine
        .apply_idempotency_retention_at(policy, 4_000)
        .unwrap();
    assert!(noop.pruning.applied);
    assert_eq!(noop.pruning.selected_count, 0);
    assert_eq!(engine.committed.db.sequence, before + 2);
    assert!(engine.execute("from counters").rows[0]["value"].cmp_eq(&crate::Value::Int(5)));
    for key in ["boundary", "recent", "future"] {
        assert!(
            engine
                .execute_idempotent_with_params(key, DIGEST, QUERY, BTreeMap::new(), None)
                .unwrap()
                .replayed
        );
    }
    assert!(
        !engine
            .execute_idempotent_with_params("old-a", DIGEST, QUERY, BTreeMap::new(), None)
            .unwrap()
            .replayed
    );
    assert!(engine.execute("from counters").rows[0]["value"].cmp_eq(&crate::Value::Int(6)));
}

#[test]
fn retention_rejects_invalid_policy_and_never_turns_underflow_into_unbounded_pruning() {
    let mut engine = seeded();
    let before = engine.committed.clone();
    for (age, max) in [(0, 1), (u64::MAX, 1), (1, 0), (1, 1_001)] {
        let policy = ReceiptRetentionPolicy {
            min_age_seconds: age,
            max_receipts: max,
        };
        assert_eq!(
            engine
                .plan_idempotency_retention_at(policy, 4_000)
                .unwrap_err()
                .code,
            "E_CONFIG"
        );
        assert_eq!(
            engine
                .apply_idempotency_retention_at(policy, 4_000)
                .unwrap_err()
                .code,
            "E_CONFIG"
        );
        assert!(Arc::ptr_eq(&before, &engine.committed));
    }
    let noop = engine
        .apply_idempotency_retention_at(
            ReceiptRetentionPolicy {
                min_age_seconds: 100,
                max_receipts: 1_000,
            },
            4_000,
        )
        .unwrap();
    assert_eq!(noop.cutoff_unix_ms, None);
    assert_eq!(noop.pruning.selected_count, 0);
    assert!(Arc::ptr_eq(&before, &engine.committed));
    let mut readonly = engine.with_read_only(true);
    let policy = ReceiptRetentionPolicy {
        min_age_seconds: 2,
        max_receipts: 1,
    };
    assert_eq!(
        readonly
            .plan_idempotency_retention_at(policy, 4_000)
            .unwrap()
            .pruning
            .selected_count,
        1
    );
    assert_eq!(
        readonly
            .apply_idempotency_retention_at(policy, 4_000)
            .unwrap_err()
            .code,
        "E_READ_ONLY"
    );
}

#[test]
fn retention_redb_reopen_backup_and_maintenance_keep_receipt_contract() {
    let dir = Directory::new();
    let memory = seeded();
    let path = dir.0.join("db.redb");
    let mut engine = Engine::restore_redb(
        path.clone(),
        (*memory.committed.db).clone(),
        (*memory.committed.receipts).clone(),
    )
    .unwrap();
    let policy = ReceiptRetentionPolicy {
        min_age_seconds: 2,
        max_receipts: 1_000,
    };
    assert_eq!(
        engine
            .apply_idempotency_retention_at(policy, 4_000)
            .unwrap()
            .pruning
            .selected_count,
        2
    );
    drop(engine);
    let archive = dir.0.join("backup.json");
    crate::backup::create(&path, &archive).unwrap();
    let restored = dir.0.join("restored.redb");
    crate::backup::restore(&archive, &restored).unwrap();
    let mut engine = Engine::open_redb(restored).unwrap();
    assert_eq!(engine.idempotency_status().unwrap().count, 3);
    assert!(
        engine
            .execute_idempotent_with_params("boundary", DIGEST, QUERY, BTreeMap::new(), None)
            .unwrap()
            .replayed
    );
    let files = [
        MigrationFile::parse("migration note\n  add field Counter.note: text = \"\"\n").unwrap(),
    ];
    engine.advance_migrations(&files, 1).unwrap();
    let before = engine.migration_status(&files).unwrap();
    assert_eq!(
        engine
            .apply_idempotency_retention_at(policy, 10_000)
            .unwrap_err()
            .code,
        "E_MAINTENANCE_REQUIRED"
    );
    assert_eq!(engine.migration_status(&files).unwrap(), before);
    assert_eq!(engine.idempotency_status().unwrap().count, 3);
    engine.abort_migration().unwrap();
    engine.check_integrity().unwrap();
}

#[test]
fn legacy_wal_does_not_enable_retention() {
    let dir = Directory::new();
    let mut engine = Engine::open(Some(dir.0.join("legacy.wal")), None, 0).unwrap();
    let policy = ReceiptRetentionPolicy {
        min_age_seconds: 1,
        max_receipts: 1,
    };
    assert_eq!(
        engine.plan_idempotency_retention(policy).unwrap_err().code,
        "E_CONFIG"
    );
    assert_eq!(
        engine.apply_idempotency_retention(policy).unwrap_err().code,
        "E_CONFIG"
    );
}

#[test]
fn retention_commit_failures_never_publish_partial_receipt_deletion() {
    for uncertain in [false, true] {
        let mut engine = seeded();
        engine.durable = super::tests::engine_with_failure(uncertain).durable;
        let before = engine.committed.clone();
        let policy = ReceiptRetentionPolicy {
            min_age_seconds: 2,
            max_receipts: 1_000,
        };
        let error = engine
            .apply_idempotency_retention_at(policy, 4_000)
            .unwrap_err();
        assert_eq!(error.code, "E_STORAGE");
        assert!(Arc::ptr_eq(&before, &engine.committed));
        assert_eq!(engine.idempotency_status().unwrap().count, 5);
        if uncertain {
            assert!(engine.write_failed);
            assert!(engine.durable.is_none());
            assert_eq!(
                engine
                    .apply_idempotency_retention_at(policy, 4_000)
                    .unwrap_err()
                    .code,
                "E_STORAGE"
            );
        } else {
            assert_eq!(
                engine
                    .apply_idempotency_retention_at(policy, 4_000)
                    .unwrap()
                    .pruning
                    .selected_count,
                2
            );
            assert_eq!(engine.idempotency_status().unwrap().count, 3);
        }
    }
}
