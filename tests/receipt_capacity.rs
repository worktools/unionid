mod common;
use common::TempDir;
use std::{collections::BTreeMap, process::Command};
use unionid::{ConcurrentEngine, Engine, IdempotencyPruneOptions, ReceiptCapacityState, backup};

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const QUERY: &str = "update counters | set value = value + 1 | returning {value}";

#[test]
fn status_doctor_and_metrics_report_capacity_without_exposing_keys_in_doctor() {
    let dir = TempDir::new();
    let db = dir.0.join("db.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    engine
        .execute_idempotent_with_params(
            "private-business-key",
            DIGEST,
            "struct Entry {id: int}\ntable entries Entry",
            BTreeMap::new(),
            None,
        )
        .unwrap();
    let status = engine.idempotency_status().unwrap();
    let capacity = status.capacity.unwrap();
    assert_eq!(capacity.state, ReceiptCapacityState::Normal);
    assert_eq!(capacity.remaining_count, 9_999);
    let shared = ConcurrentEngine::new(engine);
    assert_eq!(shared.metrics_snapshot().receipts.capacity, Some(capacity));
    drop(shared);
    let source_bytes = std::fs::read(&db).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["doctor", "--db"])
        .arg(&db)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-business-key"));
    assert!(std::fs::read(&db).unwrap() == source_bytes);
    let doctor: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        doctor["database"]["receipt_capacity"],
        serde_json::to_value(capacity).unwrap()
    );
}

/// Explicit release-mode acceptance, kept out of ordinary PR fast checks.
#[test]
#[ignore = "10,000 durable commits; run explicitly with --release --ignored"]
fn real_capacity_saturation_warns_replays_prunes_and_restores() {
    let dir = TempDir::new();
    let db = dir.0.join("capacity.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    assert!(engine.execute("struct Counter {id: int, value: int}\ntable counters Counter\n  key id\ninsert counters {id: 1, value: 0}").ok);
    let mut last_response = None;
    for index in 1..=10_000 {
        let result = engine
            .execute_idempotent_with_params(
                &format!("key-{index}"),
                DIGEST,
                QUERY,
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(!result.replayed);
        assert_eq!(result.response.warnings.is_empty(), index < 8_000);
        if index == 9_001 {
            assert_eq!(
                engine.idempotency_status().unwrap().capacity.unwrap().state,
                ReceiptCapacityState::Warning
            );
            let shared = ConcurrentEngine::new(engine);
            assert_eq!(
                shared.metrics_snapshot().receipts.capacity.unwrap().state,
                ReceiptCapacityState::Warning
            );
            drop(shared);
            for args in [vec!["receipts", "status"], vec!["doctor"]] {
                let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
                    .args(&args)
                    .arg("--db")
                    .arg(&db)
                    .args(["--format", "json"])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                let state = if args[0] == "doctor" {
                    &value["database"]["receipt_capacity"]["state"]
                } else {
                    &value["capacity"]["state"]
                };
                assert_eq!(state, "warning");
            }
            engine = Engine::open_redb(&db).unwrap();
        }
        if index % 1_000 == 0 {
            eprintln!("validated {index} durable receipts");
        }
        last_response = Some(result.response);
    }
    let original = serde_json::to_value(last_response.unwrap()).unwrap();
    assert_eq!(
        engine.idempotency_status().unwrap().capacity.unwrap().state,
        ReceiptCapacityState::Full
    );
    let failed = engine
        .execute_idempotent_with_params("overflow", DIGEST, QUERY, BTreeMap::new(), None)
        .unwrap_err();
    assert_eq!(failed.code, "E_IDEMPOTENCY_CAPACITY");
    assert!(failed.hint.unwrap().contains("receipts prune"));
    assert!(engine.execute("from counters").rows[0]["value"].cmp_eq(&unionid::Value::Int(10_000)));
    let replay = engine
        .execute_idempotent_with_params("key-10000", DIGEST, QUERY, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(serde_json::to_value(replay.response).unwrap(), original);
    for _ in 0..3 {
        engine
            .prune_idempotency_receipts(IdempotencyPruneOptions {
                completed_before_unix_ms: None,
                committed_through_sequence: Some(u64::MAX),
                max_receipts: 1_000,
            })
            .unwrap();
    }
    assert_eq!(
        engine.idempotency_status().unwrap().capacity.unwrap().state,
        ReceiptCapacityState::Normal
    );
    drop(engine);
    let archive = dir.0.join("backup.json");
    backup::create(&db, &archive).unwrap();
    let restored = dir.0.join("restored.redb");
    backup::restore(&archive, &restored).unwrap();
    let mut engine = Engine::open_redb(&restored).unwrap();
    let replay = engine
        .execute_idempotent_with_params("key-10000", DIGEST, QUERY, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(serde_json::to_value(replay.response).unwrap(), original);
    engine.check_integrity().unwrap();
}

#[test]
#[ignore = "saturates the durable 64 MiB receipt budget; run explicitly in release mode"]
fn byte_capacity_warns_and_rejects_before_the_count_limit() {
    let dir = TempDir::new();
    let mut engine = Engine::open_redb(dir.0.join("bytes.redb")).unwrap();
    assert!(engine.execute("struct Counter {id: int, value: int, payload: text}\ntable counters Counter\n  key id\ninsert counters {id: 1, value: 0, payload: \"\"}").ok);
    let source = "update counters | set {value = value + 1, payload = $payload} | returning {value, payload}";
    let mut committed = 0;
    let mut warned = false;
    for index in 1..100 {
        let result = engine.execute_idempotent_with_params(
            &format!("bytes-{index}"),
            DIGEST,
            source,
            BTreeMap::from([("payload".into(), unionid::Value::Text("x".repeat(900_000)))]),
            None,
        );
        match result {
            Ok(result) => {
                committed += 1;
                warned |= !result.response.warnings.is_empty();
            }
            Err(error) => {
                assert_eq!(error.code, "E_IDEMPOTENCY_CAPACITY");
                assert!(error.hint.unwrap().contains("receipts status"));
                break;
            }
        }
    }
    assert!(warned);
    assert!(committed < 99);
    let status = engine.idempotency_status().unwrap();
    assert_eq!(status.count, committed);
    assert_eq!(
        status.capacity.unwrap().state,
        ReceiptCapacityState::Warning
    );
    assert!(status.encoded_bytes < status.max_encoded_bytes);
    assert!(
        engine.execute("from counters | select {value}").rows[0]["value"]
            .cmp_eq(&unionid::Value::Int(committed as i64))
    );
    engine.check_integrity().unwrap();
}
