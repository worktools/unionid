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

#[test]
fn scheduled_retention_retries_definite_failure_but_never_reopens_uncertain_storage() {
    use crate::server::{ConcurrentEngine, ReceiptRetentionSchedule, ReceiptRetentionState};
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    for uncertain in [false, true] {
        let mut engine = seeded();
        engine.durable = super::tests::engine_with_failure(uncertain).durable;
        let engine = ConcurrentEngine::new(engine);
        let worker = engine
            .start_receipt_retention(
                ReceiptRetentionSchedule {
                    policy: ReceiptRetentionPolicy {
                        min_age_seconds: 1,
                        max_receipts: 1000,
                    },
                    interval_seconds: 1,
                },
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(6);
        while worker.status().state != ReceiptRetentionState::Failed {
            assert!(Instant::now() < until, "injected failure was not observed");
            std::thread::sleep(Duration::from_millis(10));
        }
        engine.with_exclusive(|engine| {
            assert_eq!(engine.idempotency_status().unwrap().count, 5);
            assert_eq!(engine.write_failed, uncertain);
            assert_eq!(engine.durable.is_none(), uncertain);
        });
        let first_pass = worker.status().as_of_unix_ms;
        while worker.status().as_of_unix_ms == first_pass {
            assert!(
                Instant::now() < until,
                "worker did not attempt its next scheduled pass"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.stop().unwrap();
        engine.with_exclusive(|engine| {
            assert_eq!(
                engine.idempotency_status().unwrap().count,
                if uncertain { 5 } else { 0 }
            );
            assert_eq!(engine.write_failed, uncertain);
            if uncertain {
                assert!(engine.durable.is_none());
            }
            assert!(engine.execute("from counters").rows[0]["value"].cmp_eq(&crate::Value::Int(5)));
        });
    }
}

#[test]
fn retention_stop_joins_inflight_commit_before_releasing_writer_ownership() {
    use crate::server::{ConcurrentEngine, ReceiptRetentionSchedule};
    use std::sync::{atomic::AtomicBool, mpsc};
    use std::time::Duration;
    struct BlockingCommit {
        inner: Box<dyn DurableBackend>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl DurableBackend for BlockingCommit {
        fn commit(
            &mut self,
            previous: &Database,
            previous_receipts: &ReceiptMap,
            database: &Database,
            receipts: &ReceiptMap,
            write_set: Option<&LogicalWriteSet>,
        ) -> std::result::Result<DurableCommitProfile, CommitFailure> {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(10)).unwrap();
            self.inner
                .commit(previous, previous_receipts, database, receipts, write_set)
        }
        fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap, StorageCheckProfile)> {
            self.inner.check_integrity()
        }
        fn supports_production_scalars(&self) -> bool {
            self.inner.supports_production_scalars()
        }
        fn versions(&self) -> StorageVersions {
            self.inner.versions()
        }
        fn upgrade(
            &mut self,
            database: &Database,
            receipts: &ReceiptMap,
            target: u32,
        ) -> std::result::Result<StorageUpgrade, CommitFailure> {
            self.inner.upgrade(database, receipts, target)
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut engine = seeded();
    engine.durable = Some(Box::new(BlockingCommit {
        inner: super::tests::engine_with_failure(false).durable.unwrap(),
        entered: entered_tx,
        release: release_rx,
    }));
    let engine = ConcurrentEngine::new(engine);
    let worker = engine
        .start_receipt_retention(
            ReceiptRetentionSchedule {
                policy: ReceiptRetentionPolicy {
                    min_age_seconds: 1,
                    max_receipts: 1000,
                },
                interval_seconds: 1,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(engine.stats().active_writes, 1);
    let (stopping_tx, stopping_rx) = mpsc::channel();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let stopper = std::thread::spawn(move || {
        stopping_tx.send(()).unwrap();
        let result = worker.stop();
        stopped_tx.send(result).unwrap();
    });
    stopping_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        stopped_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(engine.stats().active_writes, 1);
    release_tx.send(()).unwrap();
    stopped_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    stopper.join().unwrap();
    assert_eq!(engine.stats().active_writes, 0);
    engine.with_exclusive(|engine| {
        assert_eq!(engine.idempotency_status().unwrap().count, 5);
        assert!(!engine.write_failed);
    });
}
