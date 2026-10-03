use super::*;
use crate::db::generated::TestGenerationSources;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, UNIX_EPOCH};

const SETUP: &str = "sequence ids {start 1}\nstruct Item {id: int, public_id: uuid, created_at: timestamp}\ntable items: Item {key id, default id = next(ids), default public_id = uuid_v7(), default created_at = now()}";
const BATCH: &str = "insert many items [{}, {}] | returning {id, public_id, created_at}";
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn inject(engine: &mut Engine, sources: Arc<TestGenerationSources>) {
    Arc::make_mut(&mut Arc::make_mut(&mut engine.committed).db)
        .set_generation_test_sources(sources);
}

fn assert_empty(engine: &mut Engine) {
    let response = engine.execute("from items");
    assert!(response.ok);
    assert!(response.rows.is_empty());
    assert_eq!(engine.idempotency_status().unwrap().count, 0);
}

fn failure_journey(engine: &mut Engine) {
    assert!(engine.execute(SETUP).ok);
    let schema = engine.schema_info();
    let invalid =
        TestGenerationSources::new(UNIX_EPOCH + Duration::from_secs(253_402_300_800), None);
    inject(engine, invalid.clone());
    let reply = engine
        .execute_idempotent_with_params("retry", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap_err();
    assert_eq!(reply.code, "E_GENERATION");
    assert_eq!(invalid.clock_reads.load(Ordering::SeqCst), 1);
    assert_eq!(invalid.entropy_reads.load(Ordering::SeqCst), 0);
    assert_empty(engine);
    assert_eq!(engine.schema_info(), schema);
    let sources =
        TestGenerationSources::new(UNIX_EPOCH + Duration::from_secs(1_700_000_000), Some(2));
    inject(engine, sources.clone());
    let reply = engine
        .execute_idempotent_with_params("retry", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap_err();
    assert_eq!(reply.code, "E_GENERATION");
    assert_empty(engine);
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 1);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 2);
    let reply = engine
        .execute_idempotent_with_params("retry", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap();
    assert!(reply.response.ok, "{:?}", reply.response.error);
    assert!(reply.response.rows[0]["id"].cmp_eq(&crate::Value::Int(1)));
    assert!(reply.response.rows[1]["id"].cmp_eq(&crate::Value::Int(2)));
    assert!(reply.response.rows[0]["created_at"].cmp_eq(&reply.response.rows[1]["created_at"]));
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 2);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 4);
    let replay = engine
        .execute_idempotent_with_params("retry", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(
        serde_json::to_value(reply.response.rows).unwrap(),
        serde_json::to_value(replay.response.rows).unwrap()
    );
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 2);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 4);
}

#[test]
fn generation_failures_roll_back_rows_counters_and_receipt_reservations() {
    failure_journey(&mut Engine::memory());
}

#[test]
fn native_generation_failures_and_retry_remain_atomic_after_reopening() {
    let directory = std::env::temp_dir().join(format!(
        "unionid-generation-fault-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("data.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    failure_journey(&mut engine);
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.check_integrity().unwrap();
    assert_eq!(engine.idempotency_status().unwrap().count, 1);
    let replay = engine
        .execute_idempotent_with_params("retry", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap();
    assert!(replay.replayed);
    let response = engine.execute("insert items {} | returning id");
    assert!(response.ok);
    assert!(response.rows[0]["id"].cmp_eq(&crate::Value::Int(3)));
    drop(engine);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn preparation_explain_explicit_values_and_read_only_do_not_call_sources() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let sources = TestGenerationSources::new(UNIX_EPOCH, Some(1));
    inject(&mut engine, sources.clone());
    engine.prepare("insert items $row | returning id").unwrap();
    assert!(engine.execute("explain insert items {}").ok);
    let explicit = engine.execute("insert items {id: 90, public_id: uuid \"018f29bd-93a4-7000-8000-000000000001\", created_at: @2020-01-01T00:00:00Z}");
    assert!(explicit.ok, "{:?}", explicit.error);
    assert!(
        engine
            .execute("update items | set id = 91 | returning id")
            .ok
    );
    assert!(engine.execute("delete items | returning id").ok);
    engine.read_only = true;
    assert_eq!(
        engine.execute("insert items {}").error.unwrap().code,
        "E_READ_ONLY"
    );
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 0);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 0);
    let mut sequence_only = Engine::memory();
    assert!(sequence_only.execute("sequence ids {start 1}\nstruct Only {id: int}\ntable only: Only {default id = next(ids)}").ok);
    inject(&mut sequence_only, sources.clone());
    assert!(sequence_only.execute("insert only {}").ok);
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 0);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 0);
}

fn interruption_journey(engine: &mut Engine) {
    use crate::db::generated::TestGenerationInterrupt;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;
    assert!(engine.execute(SETUP).ok);
    let schema = engine.schema_info();
    let mut expected_id = 1;
    for timed in [false, true] {
        let sequence = engine.committed.db.sequence;
        let receipt_count = engine.idempotency_status().unwrap().count;
        let signal = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + Duration::from_secs(5);
        let interrupt = if timed {
            TestGenerationInterrupt::Deadline(deadline)
        } else {
            TestGenerationInterrupt::Cancel(signal.clone())
        };
        let sources = TestGenerationSources::interrupt_after_entropy(UNIX_EPOCH, 1, interrupt);
        inject(engine, sources.clone());
        let control = ExecutionControl::cancellable(deadline, signal, None);
        let key = if timed { "timeout" } else { "cancel" };
        let failure = engine
            .execute_idempotent_with_deadline(
                key,
                DIGEST,
                BATCH,
                BTreeMap::new(),
                None,
                Some(&control),
            )
            .unwrap_err();
        assert_eq!(
            failure.code,
            if timed { "E_TIMEOUT" } else { "E_CANCELLED" }
        );
        // Entropy is sampled only after allocating the sequence ID and time:
        // the failure therefore occurs inside allocation, not at admission.
        assert!(sources.entropy_reads.load(Ordering::SeqCst) >= 1);
        assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 1);
        assert_eq!(engine.committed.db.sequence, sequence);
        assert_eq!(engine.schema_info(), schema);
        assert_eq!(engine.idempotency_status().unwrap().count, receipt_count);
        let rows = engine.execute("from items");
        assert!(rows.ok);
        assert_eq!(rows.rows.len(), (expected_id - 1) as usize);
        let good = TestGenerationSources::new(UNIX_EPOCH, None);
        inject(engine, good.clone());
        let retry = engine
            .execute_idempotent_with_params(key, DIGEST, BATCH, BTreeMap::new(), None)
            .unwrap();
        assert!(!retry.replayed);
        assert!(retry.response.rows[0]["id"].cmp_eq(&crate::Value::Int(expected_id)));
        assert!(retry.response.rows[1]["id"].cmp_eq(&crate::Value::Int(expected_id + 1)));
        expected_id += 2;
        let replay = engine
            .execute_idempotent_with_params(key, DIGEST, BATCH, BTreeMap::new(), None)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(good.entropy_reads.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn cancellation_and_deadline_after_generation_roll_back_memory_candidates() {
    interruption_journey(&mut Engine::memory());
}

#[test]
fn cancellation_and_deadline_after_generation_do_not_publish_native_counters_or_receipts() {
    let directory = std::env::temp_dir().join(format!(
        "unionid-generated-interruption-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("data.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(&["generated_defaults".into()], None)
        .unwrap();
    interruption_journey(&mut engine);
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.check_integrity().unwrap();
    assert_eq!(engine.idempotency_status().unwrap().count, 2);
    let next = engine.execute("insert items {} | returning id");
    assert!(next.ok);
    assert!(next.rows[0]["id"].cmp_eq(&crate::Value::Int(5)));
    drop(engine);
    std::fs::remove_dir_all(directory).unwrap();
}

fn reference_failure_journey(engine: &mut Engine) {
    assert!(engine.execute(SETUP).ok);
    assert!(engine.execute("struct Parent {id: int}\ntable parents: Parent {key id}\ncreate reference items (id) references parents (id)").ok);
    let schema = engine.schema_info();
    let sequence = engine.committed.db.sequence;
    let sources = TestGenerationSources::new(UNIX_EPOCH, None);
    inject(engine, sources.clone());
    let failed = engine
        .execute_idempotent_with_params("reference", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap_err();
    assert_eq!(failed.code, "E_CONSTRAINT");
    assert_eq!(
        failed.constraint,
        Some(crate::error::ConstraintKind::ReferenceMissing)
    );
    assert!(sources.entropy_reads.load(Ordering::SeqCst) >= 1);
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 1);
    assert_eq!(engine.committed.db.sequence, sequence);
    assert_eq!(engine.schema_info(), schema);
    assert_empty(engine);
    assert!(engine.execute("insert many parents [{id: 1}, {id: 2}]").ok);
    let retry = engine
        .execute_idempotent_with_params("reference", DIGEST, BATCH, BTreeMap::new(), None)
        .unwrap();
    assert!(!retry.replayed);
    assert!(retry.response.rows[0]["id"].cmp_eq(&crate::Value::Int(1)));
    assert!(retry.response.rows[1]["id"].cmp_eq(&crate::Value::Int(2)));
}

#[test]
fn reference_failure_after_generation_rolls_back_memory_counter_and_receipt() {
    reference_failure_journey(&mut Engine::memory());
}

#[test]
fn reference_failure_after_generation_rolls_back_native_counter_and_receipt() {
    let directory = std::env::temp_dir().join(format!(
        "unionid-generated-reference-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("data.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.upgrade_storage(14).unwrap();
    engine
        .install_storage_capabilities(
            &["generated_defaults".into(), "typed_references".into()],
            None,
        )
        .unwrap();
    reference_failure_journey(&mut engine);
    drop(engine);
    let mut engine = Engine::open_redb(&path).unwrap();
    engine.check_integrity().unwrap();
    assert_eq!(engine.idempotency_status().unwrap().count, 1);
    assert!(engine.execute("insert parents {id: 3}").ok);
    let next = engine.execute("insert items {} | returning id");
    assert!(next.ok, "{:?}", next.error);
    assert!(next.rows[0]["id"].cmp_eq(&crate::Value::Int(3)));
    drop(engine);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn prepared_generation_survives_counter_changes_but_rejects_policy_drift_without_sampling() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let prepared = engine.prepare("insert items $row | returning id").unwrap();
    let parameters = BTreeMap::from([("row".into(), crate::Value::Record(BTreeMap::new()))]);
    let schema = engine.schema_info();
    for expected in [1, 2] {
        let inserted = engine.execute_prepared(&prepared, parameters.clone());
        assert!(inserted.ok, "{:?}", inserted.error);
        assert!(inserted.rows[0]["id"].cmp_eq(&crate::Value::Int(expected)));
        assert_eq!(engine.schema_info(), schema);
    }
    assert!(
        engine
            .execute("migration remove {drop default items.id}")
            .ok
    );
    assert_ne!(engine.schema_info().hash, schema.hash);
    let sources = TestGenerationSources::new(UNIX_EPOCH, Some(1));
    inject(&mut engine, sources.clone());
    let stale = engine.execute_prepared(&prepared, parameters.clone());
    assert_eq!(stale.error.unwrap().code, "E_SCHEMA_CHANGED");
    let current = engine.prepare("insert items $row | returning id").unwrap();
    let missing = engine.execute_prepared(&current, parameters);
    assert_eq!(missing.error.unwrap().code, "E_FIELD");
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 0);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 0);
    assert_eq!(engine.execute("from items").rows.len(), 2);
}
