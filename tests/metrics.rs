use std::time::{Duration, Instant};

use unionid::protocol::Request;
use unionid::{ConcurrentEngine, Engine, IntrospectionKind, METRICS_VERSION};

#[test]
fn metrics_snapshot_counts_bounded_operations_errors_receipts_and_streams() {
    let shared = ConcurrentEngine::new(Engine::memory());
    let initial = shared.metrics_snapshot();
    assert_eq!(initial.version, METRICS_VERSION);
    assert_eq!(initial.operations.read.requests, 0);
    assert_eq!(initial.operations.write.requests, 0);
    assert_eq!(initial.errors, []);

    assert!(
        shared
            .execute("type Item = {id int, secret text}\ntable items Item\n  key id")
            .ok
    );
    assert!(
        shared
            .execute("insert items {id = 1, secret = \"never-export-this\"}")
            .ok
    );
    assert!(shared.execute("from items | filter id == 1").ok);
    let failed = shared.execute("from missing_private_table");
    assert_eq!(failed.error.unwrap().code, "E_TABLE");

    let introspection = shared.execute_protocol_request(Request::introspection(
        "private-request-id",
        IntrospectionKind::Storage,
    ));
    assert!(introspection.ok, "{}", introspection.message);
    let receipts = shared.execute_protocol_request(Request::receipt_status("receipt-status"));
    assert!(receipts.ok, "{}", receipts.message);
    shared
        .with_maintenance(|_| Ok::<_, unionid::Error>(()))
        .unwrap();
    let maintenance_error = shared
        .with_maintenance(|_| Err::<(), _>(unionid::Error::new("PRIVATE", "hidden")))
        .unwrap_err();
    assert_eq!(maintenance_error.message, "hidden");

    let idempotent = shared.execute_protocol_request(
        Request::query(
            "write-request",
            "insert items {id = 2, secret = \"also-private\"}",
        )
        .with_idempotency_key("private-idempotency-key")
        .unwrap(),
    );
    assert!(idempotent.ok, "{}", idempotent.message);

    let stream = shared
        .register_read(
            Request::query("private-stream-request", "from items | sort id"),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap()
        .start();
    assert!(stream.ok, "{}", stream.message);
    assert_eq!(stream.rows.len(), 2);

    let cancelled = shared
        .register_read(
            Request::query("cancelled-stream", "from items"),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(
        shared.cancel(cancelled.id()).unwrap().status,
        unionid::CancelStatus::Accepted
    );
    assert_eq!(cancelled.start().error.unwrap().code, "E_CANCELLED");

    let snapshot = shared.metrics_snapshot();
    assert_eq!(snapshot.operations.write.requests, 3);
    assert_eq!(snapshot.operations.write.failures, 0);
    assert_eq!(snapshot.operations.read.requests, 2);
    assert_eq!(snapshot.operations.read.failures, 1);
    assert_eq!(snapshot.operations.introspection.requests, 1);
    assert_eq!(snapshot.operations.receipt.requests, 1);
    assert_eq!(snapshot.operations.stream.requests, 3);
    assert_eq!(snapshot.operations.stream.failures, 1);
    assert_eq!(snapshot.operations.maintenance.requests, 2);
    assert_eq!(snapshot.operations.maintenance.failures, 1);
    assert_eq!(snapshot.receipts.count, 1);
    assert!(snapshot.receipts.encoded_bytes > 0);
    assert_eq!(snapshot.errors.len(), 3);
    for code in ["E_TABLE", "E_CANCELLED", "E_MAINTENANCE"] {
        assert_eq!(
            snapshot
                .errors
                .iter()
                .find(|error| error.code == code)
                .unwrap()
                .count,
            1
        );
    }

    let read_latency = &snapshot.operations.read.latency;
    assert_eq!(read_latency.count, snapshot.operations.read.requests);
    assert!(
        read_latency
            .buckets
            .windows(2)
            .all(|pair| pair[0].count <= pair[1].count)
    );
    assert!(read_latency.percentile_upper_bound_micros(5_000).is_some());
    assert!(read_latency.percentile_upper_bound_micros(9_500).is_some());

    let json = serde_json::to_string(&snapshot).unwrap();
    let decoded: unionid::MetricsSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, snapshot);
    for private in [
        "never-export-this",
        "also-private",
        "missing_private_table",
        "private-request-id",
        "private-stream-request",
        "private-idempotency-key",
        "PRIVATE",
        "hidden",
    ] {
        assert!(!json.contains(private), "metrics leaked {private}");
    }
}

#[cfg(feature = "metrics")]
#[test]
fn optional_prometheus_text_has_parseable_bounded_labels() {
    let shared = ConcurrentEngine::new(Engine::memory());
    assert!(shared.execute("create table items (id int)").ok);
    assert!(shared.execute("from items").ok);
    assert!(!shared.execute("from absent").ok);
    let text = shared.metrics_snapshot().prometheus_text();

    assert!(text.contains("# TYPE unionid_request_duration_seconds histogram"));
    assert!(text.contains("unionid_requests_total{operation=\"read\"} 2"));
    assert!(text.contains("unionid_errors_total{code=\"E_TABLE\"} 1"));
    assert!(text.ends_with('\n'));
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let mut fields = line.split_ascii_whitespace();
        let metric = fields.next().unwrap();
        let value = fields.next().unwrap();
        assert!(
            fields.next().is_none(),
            "unexpected exposition fields: {line}"
        );
        assert!(metric.starts_with("unionid_"), "{metric}");
        assert!(value.parse::<f64>().unwrap().is_finite(), "{line}");
    }
    assert!(!text.contains("items"));
    assert!(!text.contains("absent"));
}
