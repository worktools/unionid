//! Optional Prometheus text rendering for [`crate::metrics::MetricsSnapshot`].

use std::fmt::Write;

use crate::metrics::{MetricsSnapshot, OperationMetrics};

pub(crate) fn render(snapshot: &MetricsSnapshot) -> String {
    let mut output = String::new();
    metric_header(
        &mut output,
        "unionid_info",
        "Unionid metrics schema information",
        "gauge",
    );
    writeln!(
        output,
        "unionid_info{{metrics_version=\"{}\"}} 1",
        snapshot.version
    )
    .unwrap();
    gauge(
        &mut output,
        "unionid_uptime_seconds",
        "Process-local ConcurrentEngine metrics lifetime",
        snapshot.uptime_micros as f64 / 1_000_000.0,
    );
    gauge(
        &mut output,
        "unionid_connections_active",
        "Active built-in TCP connections",
        snapshot.connections.active,
    );
    counter(
        &mut output,
        "unionid_connections_accepted_total",
        "Accepted built-in TCP connections",
        snapshot.connections.accepted,
    );
    counter(
        &mut output,
        "unionid_connections_rejected_total",
        "Rejected built-in TCP connections",
        snapshot.connections.rejected,
    );
    metric_header(
        &mut output,
        "unionid_requests_total",
        "Completed requests by bounded operation class",
        "counter",
    );
    metric_header(
        &mut output,
        "unionid_request_failures_total",
        "Failed requests by bounded operation class",
        "counter",
    );
    metric_header(
        &mut output,
        "unionid_request_duration_seconds",
        "Request duration by bounded operation class",
        "histogram",
    );
    for (name, operation) in operations(snapshot) {
        writeln!(
            output,
            "unionid_requests_total{{operation=\"{name}\"}} {}",
            operation.requests
        )
        .unwrap();
        writeln!(
            output,
            "unionid_request_failures_total{{operation=\"{name}\"}} {}",
            operation.failures
        )
        .unwrap();
        for bucket in &operation.latency.buckets {
            writeln!(
                output,
                "unionid_request_duration_seconds_bucket{{operation=\"{name}\",le=\"{}\"}} {}",
                seconds_label(bucket.le_micros),
                bucket.count
            )
            .unwrap();
        }
        writeln!(
            output,
            "unionid_request_duration_seconds_bucket{{operation=\"{name}\",le=\"+Inf\"}} {}",
            operation.latency.count
        )
        .unwrap();
        writeln!(
            output,
            "unionid_request_duration_seconds_sum{{operation=\"{name}\"}} {}",
            operation.latency.sum_micros as f64 / 1_000_000.0
        )
        .unwrap();
        writeln!(
            output,
            "unionid_request_duration_seconds_count{{operation=\"{name}\"}} {}",
            operation.latency.count
        )
        .unwrap();
    }
    metric_header(
        &mut output,
        "unionid_errors_total",
        "Failed requests by bounded internal error code",
        "counter",
    );
    for error in &snapshot.errors {
        writeln!(
            output,
            "unionid_errors_total{{code=\"{}\"}} {}",
            escape_label(&error.code),
            error.count
        )
        .unwrap();
    }
    counter(
        &mut output,
        "unionid_error_code_overflow_total",
        "Errors omitted after the bounded error-code capacity",
        snapshot.error_code_overflow,
    );
    for (name, value) in [
        ("active_reads", snapshot.concurrency.active_reads),
        ("queued_reads", snapshot.concurrency.queued_reads),
        ("active_writes", snapshot.concurrency.active_writes),
        ("queued_writes", snapshot.concurrency.queued_writes),
        (
            "registered_operations",
            snapshot.concurrency.registered_operations,
        ),
        ("queued_operations", snapshot.concurrency.queued_operations),
        (
            "executing_operations",
            snapshot.concurrency.executing_operations,
        ),
        (
            "emitting_operations",
            snapshot.concurrency.emitting_operations,
        ),
        (
            "cancelling_operations",
            snapshot.concurrency.cancelling_operations,
        ),
    ] {
        writeln!(
            output,
            "# HELP unionid_concurrency_{name} Current {name} count"
        )
        .unwrap();
        writeln!(output, "# TYPE unionid_concurrency_{name} gauge").unwrap();
        writeln!(output, "unionid_concurrency_{name} {value}").unwrap();
    }
    gauge(
        &mut output,
        "unionid_receipts_count",
        "Current idempotency receipt count",
        snapshot.receipts.count,
    );
    gauge(
        &mut output,
        "unionid_receipts_encoded_bytes",
        "Current encoded idempotency receipt bytes",
        snapshot.receipts.encoded_bytes,
    );
    gauge(
        &mut output,
        "unionid_receipts_max_count",
        "Configured idempotency receipt count capacity",
        snapshot.receipts.max_count,
    );
    gauge(
        &mut output,
        "unionid_receipts_max_encoded_bytes",
        "Configured encoded idempotency receipt byte capacity",
        snapshot.receipts.max_encoded_bytes,
    );
    output
}

fn operations(snapshot: &MetricsSnapshot) -> [(&'static str, &OperationMetrics); 6] {
    [
        ("read", &snapshot.operations.read),
        ("write", &snapshot.operations.write),
        ("introspection", &snapshot.operations.introspection),
        ("receipt", &snapshot.operations.receipt),
        ("stream", &snapshot.operations.stream),
        ("maintenance", &snapshot.operations.maintenance),
    ]
}

fn metric_header(output: &mut String, name: &str, help: &str, kind: &str) {
    writeln!(output, "# HELP {name} {help}").unwrap();
    writeln!(output, "# TYPE {name} {kind}").unwrap();
}

fn counter(output: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    metric_header(output, name, help, "counter");
    writeln!(output, "{name} {value}").unwrap();
}

fn gauge(output: &mut String, name: &str, help: &str, value: impl std::fmt::Display) {
    metric_header(output, name, help, "gauge");
    writeln!(output, "{name} {value}").unwrap();
}

fn seconds_label(micros: u64) -> String {
    if micros.is_multiple_of(1_000_000) {
        (micros / 1_000_000).to_string()
    } else {
        format!("{:.6}", micros as f64 / 1_000_000.0)
            .trim_end_matches('0')
            .to_owned()
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}
