use std::time::Duration;

use unionid::{ConcurrentEngine, Engine, ProtocolRequest, asynchronous};

#[tokio::test]
async fn asynchronous_run_offloads_to_a_blocking_worker() {
    let value = asynchronous::run(|| 40 + 2).await.unwrap();
    assert_eq!(value, 42);
}

#[tokio::test]
async fn asynchronous_adapter_executes_protocol_requests() {
    let mut engine = Engine::memory();
    assert!(engine.execute("create table items (id int, label text)").ok);
    let concurrent = ConcurrentEngine::new(engine);

    let insert = asynchronous::execute_protocol_request(
        concurrent.clone(),
        ProtocolRequest::query("insert", "insert items { id = 1, label = \"a\" }"),
        Duration::from_secs(5),
    )
    .await;
    assert!(insert.ok, "{}", insert.message);
    assert_eq!(insert.affected_rows, Some(1));

    let read = asynchronous::execute_protocol_request(
        concurrent,
        ProtocolRequest::query("read", "from items"),
        Duration::from_secs(5),
    )
    .await;
    assert!(read.ok, "{}", read.message);
    assert_eq!(read.rows.len(), 1);
}
