mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::TempDir;
use unionid::error::ConstraintKind;
use unionid::protocol::{PRODUCTION_VERSION, Request};
use unionid::server::ConcurrentEngine;
use unionid::{Engine, Value};

const TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA: &str = "struct Order {id: int}\nstruct Line {id: int, order_id: int}\ntable orders: Order {key id}\ntable lines: Line {key id}\ncreate reference lines (order_id) references orders (id)\ninsert orders {id: 1}";
const INSERT: &str = "insert lines {id: 7, order_id: 1}";

fn controlled_writers(durable: bool, source_first: bool, target_change: &str) {
    let dir = TempDir::new();
    let path = dir.0.join("concurrent-references.redb");
    let mut engine = if durable {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        engine
    } else {
        Engine::memory()
    };
    let setup = engine.execute(SCHEMA);
    assert!(setup.ok, "{}", setup.message);
    let schema = engine.schema_info();
    let shared = ConcurrentEngine::new(engine);
    let (first, second, rejected_kind) = if source_first {
        (INSERT, target_change, ConstraintKind::ReferenceRestricted)
    } else {
        (target_change, INSERT, ConstraintKind::ReferenceMissing)
    };

    std::thread::scope(|scope| {
        let (acquired, acquired_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let first_engine = shared.clone();
        let first_writer = scope.spawn(move || {
            first_engine.with_exclusive(|engine| {
                // Hold actual writer ownership before the contender submits.
                // No candidate is constructed until both threads are present.
                acquired.send(()).unwrap();
                release_rx.recv_timeout(TIMEOUT).unwrap();
                engine.execute(first)
            })
        });
        acquired_rx.recv_timeout(TIMEOUT).unwrap();
        let second_engine = shared.clone();
        let second_writer = scope.spawn(move || {
            second_engine.execute_protocol_request(
                Request::query("reference-contender", second)
                    .with_version(PRODUCTION_VERSION)
                    .unwrap(),
            )
        });
        let deadline = Instant::now() + TIMEOUT;
        while shared.stats().queued_writes != 1 {
            assert!(Instant::now() < deadline, "contender did not queue");
            std::thread::yield_now();
        }
        assert_eq!(shared.stats().active_writes, 1);
        release.send(()).unwrap();
        let committed = first_writer.join().unwrap();
        assert!(committed.ok, "{}", committed.message);
        let rejected = second_writer.join().unwrap();
        assert!(!rejected.ok, "accepted stale candidate: {second}");
        let error = rejected.error.unwrap();
        assert_eq!(error.code, "E_CONSTRAINT");
        assert_eq!(error.constraint, Some(rejected_kind));
        assert_eq!(rejected.schema, committed.schema);
        assert_eq!(committed.schema.as_ref(), Some(&schema));
    });

    let orders = shared.execute("from orders");
    let lines = shared.execute("from lines");
    assert!(orders.ok && lines.ok);
    assert_eq!(lines.rows.len(), usize::from(source_first));
    if source_first {
        assert_eq!(orders.rows.len(), 1);
        assert!(matches!(orders.rows[0]["id"], Value::Int(1)));
        assert!(matches!(lines.rows[0]["order_id"], Value::Int(1)));
    } else if target_change.starts_with("delete") {
        assert!(orders.rows.is_empty());
    } else {
        assert_eq!(orders.rows.len(), 1);
        assert!(matches!(orders.rows[0]["id"], Value::Int(2)));
    }
    assert_eq!(shared.stats().queued_writes, 0);
    assert_eq!(shared.stats().active_writes, 0);
    if durable {
        shared.with_exclusive(|engine| engine.check_integrity().unwrap());
    }
    drop(shared);
    if durable {
        let mut reopened = Engine::open_redb(&path).unwrap();
        reopened.check_integrity().unwrap();
        assert_eq!(reopened.schema_info(), schema);
        assert_eq!(
            serde_json::to_value(reopened.execute("from orders").rows).unwrap(),
            serde_json::to_value(orders.rows).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(reopened.execute("from lines").rows).unwrap(),
            serde_json::to_value(lines.rows).unwrap(),
        );
    }
}

#[test]
fn memory_reference_writers_validate_latest_commit_in_both_orders() {
    for source_first in [true, false] {
        for target_change in ["delete orders", "update orders | set id = 2"] {
            controlled_writers(false, source_first, target_change);
        }
    }
}

#[test]
fn redb_reference_writers_validate_latest_commit_in_both_orders_and_reopen() {
    for source_first in [true, false] {
        for target_change in ["delete orders", "update orders | set id = 2"] {
            controlled_writers(true, source_first, target_change);
        }
    }
}
