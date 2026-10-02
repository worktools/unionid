mod common;
use common::TempDir;
use std::process::Command;

#[test]
fn server_rejects_invalid_retention_before_creating_database() {
    let dir = TempDir::new();
    let db = dir.0.join("must-not-create.redb");
    for args in [
        vec!["--receipt-retention-seconds", "0"],
        vec!["--receipt-retention-seconds", "18446744073709551615"],
        vec![
            "--receipt-retention-seconds",
            "1",
            "--receipt-retention-interval-seconds",
            "0",
        ],
        vec![
            "--receipt-retention-seconds",
            "1",
            "--receipt-retention-interval-seconds",
            "86401",
        ],
        vec![
            "--receipt-retention-seconds",
            "1",
            "--receipt-retention-max-receipts",
            "0",
        ],
        vec![
            "--receipt-retention-seconds",
            "1",
            "--receipt-retention-max-receipts",
            "1001",
        ],
        vec!["--receipt-retention-interval-seconds", "1"],
        vec!["--receipt-retention-max-receipts", "1"],
        vec!["--receipt-retention-seconds", "1", "--read-only"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["server", "--addr", "127.0.0.1:0", "--db"])
            .arg(&db)
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(!db.exists(), "created database for {args:?}");
    }
}

#[test]
fn service_retention_help_is_explicit_about_reuse_and_enablement() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["server", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for fragment in [
        "--receipt-retention-seconds",
        "--receipt-retention-interval-seconds",
        "--receipt-retention-max-receipts",
        "keys can execute again",
        "requires an explicit window",
    ] {
        assert!(help.contains(fragment), "missing {fragment}");
    }
}

#[test]
fn tcp_cleanup_restart_and_both_backup_paths_preserve_effects_and_replay() {
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};
    use unionid::backup::incremental::{self, ArchiveLimits};
    use unionid::backup::incremental::{IncrementalExportOptions, IncrementalInitOptions};
    use unionid::{
        Engine, ProtocolRequest, ReceiptRetentionPolicy, cli,
        server::{self, ConcurrentEngine, ReceiptRetentionSchedule},
    };

    struct Running {
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<Result<server::ServerStats, String>>>,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap().unwrap();
            }
        }
    }
    let dir = TempDir::new();
    let db = dir.0.join("service.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    assert!(engine.execute("struct Counter {id: int, value: int}\ntable counters Counter\n  key id\ninsert counters {id: 1, value: 0}").ok);
    drop(engine);
    let repo = dir.0.join("archive");
    incremental::init(&db, &repo, IncrementalInitOptions::default()).unwrap();
    let request = ProtocolRequest::query(
        "increment",
        "update counters | set value = value + 1 | returning {value}",
    )
    .with_idempotency_key("delivery")
    .unwrap();

    for enabled in [true, false] {
        let engine = ConcurrentEngine::new(Engine::open_redb(&db).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let signal = shutdown.clone();
        let service = engine.clone();
        let retention = enabled.then_some(ReceiptRetentionSchedule {
            policy: ReceiptRetentionPolicy {
                min_age_seconds: 1,
                max_receipts: 1,
            },
            interval_seconds: 1,
        });
        let running = Running {
            shutdown,
            thread: Some(std::thread::spawn(move || {
                server::serve_until_concurrent_with_retention(listener, service, signal, retention)
            })),
        };
        let first = cli::send_request(&addr, &request).unwrap();
        assert!(first.ok, "{}", first.message);
        assert_eq!(first.idempotency.unwrap().replayed, !enabled);
        if enabled {
            let until = Instant::now() + Duration::from_secs(8);
            loop {
                if engine.with_exclusive(|engine| engine.idempotency_status().unwrap().count) == 0 {
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "service did not prune expired receipt"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            let reused = cli::send_request(&addr, &request).unwrap();
            assert!(reused.ok);
            assert!(!reused.idempotency.unwrap().replayed);
        } else {
            // Restart without flags must not reactivate the previous policy.
            std::thread::sleep(Duration::from_millis(1200));
        }
        assert!(
            cli::send_request(&addr, &request)
                .unwrap()
                .idempotency
                .unwrap()
                .replayed
        );
        drop(running);
        assert_eq!(engine.stats().active_writes, 0);
        engine.with_exclusive(|engine| {
            assert_eq!(engine.idempotency_status().unwrap().count, 1);
            assert!(
                engine.execute("from counters").rows[0]["value"].cmp_eq(&unionid::Value::Int(2))
            );
            engine.check_integrity().unwrap();
        });
        drop(engine);
    }
    let logical = dir.0.join("backup.json");
    unionid::backup::create(&db, &logical).unwrap();
    let logical_db = dir.0.join("logical.redb");
    unionid::backup::restore(&logical, &logical_db).unwrap();
    incremental::export(&db, &repo, IncrementalExportOptions::default()).unwrap();
    let engine = Engine::open_redb(&db).unwrap();
    let sequence = engine.backup_journal_status().unwrap().head_sequence;
    drop(engine);
    let incremental_db = dir.0.join("incremental.redb");
    incremental::restore(&repo, &incremental_db, sequence, ArchiveLimits::default()).unwrap();
    for restored in [logical_db, incremental_db] {
        let mut engine = Engine::open_redb(restored).unwrap();
        assert_eq!(engine.idempotency_status().unwrap().count, 1);
        let replay = server::execute_protocol_request(&mut engine, request.clone());
        assert!(replay.ok);
        assert!(replay.idempotency.unwrap().replayed);
        assert!(engine.execute("from counters").rows[0]["value"].cmp_eq(&unionid::Value::Int(2)));
        engine.check_integrity().unwrap();
    }
}

#[test]
fn active_migration_pauses_worker_without_advancing_or_aborting_it() {
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant};
    use unionid::{
        Engine, MigrationFile, ReceiptRetentionPolicy,
        server::{ConcurrentEngine, ReceiptRetentionSchedule, ReceiptRetentionState},
    };
    let dir = TempDir::new();
    let mut engine = Engine::open_redb(dir.0.join("maintenance.redb")).unwrap();
    assert!(
        engine
            .execute("struct Item {id: int}\ntable items Item\ninsert items {id: 1}")
            .ok
    );
    let files =
        [MigrationFile::parse("migration note\n  add field Item.note: text = \"\"\n").unwrap()];
    engine.advance_migrations(&files, 1).unwrap();
    let before = engine.migration_status(&files).unwrap();
    let engine = ConcurrentEngine::new(engine);
    let worker = engine
        .start_receipt_retention(
            ReceiptRetentionSchedule {
                policy: ReceiptRetentionPolicy {
                    min_age_seconds: 1,
                    max_receipts: 1,
                },
                interval_seconds: 1,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while worker.status().state != ReceiptRetentionState::Maintenance {
        assert!(Instant::now() < until, "maintenance status was not exposed");
        std::thread::sleep(Duration::from_millis(10));
    }
    worker.stop().unwrap();
    engine.with_exclusive(|engine| {
        assert_eq!(engine.migration_status(&files).unwrap(), before);
        engine.abort_migration().unwrap();
        engine.check_integrity().unwrap();
    });
}
