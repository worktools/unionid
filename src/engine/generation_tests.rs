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
    engine.read_only = true;
    assert_eq!(
        engine.execute("insert items {}").error.unwrap().code,
        "E_READ_ONLY"
    );
    assert_eq!(sources.clock_reads.load(Ordering::SeqCst), 0);
    assert_eq!(sources.entropy_reads.load(Ordering::SeqCst), 0);
}
