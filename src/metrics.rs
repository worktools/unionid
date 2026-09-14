//! Bounded, value-free service metrics shared by embedded and network adapters.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::server::ConcurrencyStats;

pub const METRICS_VERSION: u32 = 1;
pub const MAX_METRIC_ERROR_CODES: usize = 64;
pub const LATENCY_BUCKETS_MICROS: [u64; 12] = [
    100, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000, 10_000_000,
    25_000_000,
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionMetrics {
    pub active: usize,
    pub accepted: u64,
    pub rejected: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatencyHistogram {
    pub count: u64,
    pub sum_micros: u64,
    pub buckets: Vec<LatencyBucket>,
}

impl LatencyHistogram {
    /// Return the upper bound of the bucket containing a percentile in basis points.
    pub fn percentile_upper_bound_micros(&self, basis_points: u16) -> Option<u64> {
        if self.count == 0 || basis_points == 0 || basis_points > 10_000 {
            return None;
        }
        let rank = self
            .count
            .saturating_mul(u64::from(basis_points))
            .saturating_add(9_999)
            / 10_000;
        self.buckets
            .iter()
            .find(|bucket| bucket.count >= rank)
            .map(|bucket| bucket.le_micros)
            .or(Some(u64::MAX))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatencyBucket {
    pub le_micros: u64,
    /// Cumulative observations at or below `le_micros`.
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationMetrics {
    pub requests: u64,
    pub failures: u64,
    pub latency: LatencyHistogram,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationsMetrics {
    pub read: OperationMetrics,
    pub write: OperationMetrics,
    pub introspection: OperationMetrics,
    pub receipt: OperationMetrics,
    pub stream: OperationMetrics,
    pub maintenance: OperationMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorMetric {
    pub code: String,
    pub count: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptMetrics {
    pub count: usize,
    pub encoded_bytes: usize,
    pub max_count: usize,
    pub max_encoded_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub version: u32,
    pub uptime_micros: u64,
    pub connections: ConnectionMetrics,
    pub operations: OperationsMetrics,
    pub errors: Vec<ErrorMetric>,
    pub error_code_overflow: u64,
    pub concurrency: ConcurrencyStats,
    pub receipts: ReceiptMetrics,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MetricOperation {
    Read,
    Write,
    Introspection,
    Receipt,
    Stream,
    Maintenance,
}

pub(crate) struct MetricsRegistry {
    started: Instant,
    active_connections: AtomicUsize,
    accepted_connections: AtomicU64,
    rejected_connections: AtomicU64,
    read: OperationCounters,
    write: OperationCounters,
    introspection: OperationCounters,
    receipt: OperationCounters,
    stream: OperationCounters,
    maintenance: OperationCounters,
    errors: Mutex<BTreeMap<String, u64>>,
    error_code_overflow: AtomicU64,
    receipt_count: AtomicUsize,
    receipt_encoded_bytes: AtomicUsize,
    receipt_max_count: AtomicUsize,
    receipt_max_encoded_bytes: AtomicUsize,
}

struct OperationCounters {
    requests: AtomicU64,
    failures: AtomicU64,
    latency_count: AtomicU64,
    latency_sum_micros: AtomicU64,
    latency_buckets: [AtomicU64; LATENCY_BUCKETS_MICROS.len()],
}

impl MetricsRegistry {
    pub(crate) fn new(receipts: Option<&crate::IdempotencyStatus>) -> Self {
        let metrics = Self {
            started: Instant::now(),
            active_connections: AtomicUsize::new(0),
            accepted_connections: AtomicU64::new(0),
            rejected_connections: AtomicU64::new(0),
            read: OperationCounters::new(),
            write: OperationCounters::new(),
            introspection: OperationCounters::new(),
            receipt: OperationCounters::new(),
            stream: OperationCounters::new(),
            maintenance: OperationCounters::new(),
            errors: Mutex::new(BTreeMap::new()),
            error_code_overflow: AtomicU64::new(0),
            receipt_count: AtomicUsize::new(0),
            receipt_encoded_bytes: AtomicUsize::new(0),
            receipt_max_count: AtomicUsize::new(0),
            receipt_max_encoded_bytes: AtomicUsize::new(0),
        };
        metrics.update_receipts(receipts);
        metrics
    }

    pub(crate) fn record(
        &self,
        operation: MetricOperation,
        elapsed: Duration,
        error_code: Option<&str>,
    ) {
        self.operation(operation)
            .record(elapsed, error_code.is_some());
        if let Some(code) = error_code {
            let mut errors = self
                .errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(count) = errors.get_mut(code) {
                *count = count.saturating_add(1);
            } else if errors.len() < MAX_METRIC_ERROR_CODES {
                errors.insert(code.to_owned(), 1);
            } else {
                saturating_add(&self.error_code_overflow, 1);
            }
        }
    }

    pub(crate) fn connection_accepted(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        saturating_add(&self.accepted_connections, 1);
    }

    pub(crate) fn connection_rejected(&self) {
        saturating_add(&self.rejected_connections, 1);
    }

    pub(crate) fn connection_closed(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn update_receipts(&self, status: Option<&crate::IdempotencyStatus>) {
        let Some(status) = status else {
            return;
        };
        self.receipt_count.store(status.count, Ordering::Relaxed);
        self.receipt_encoded_bytes
            .store(status.encoded_bytes, Ordering::Relaxed);
        self.receipt_max_count
            .store(status.max_count, Ordering::Relaxed);
        self.receipt_max_encoded_bytes
            .store(status.max_encoded_bytes, Ordering::Relaxed);
    }

    pub(crate) fn receipt_count(&self) -> usize {
        self.receipt_count.load(Ordering::Relaxed)
    }

    pub(crate) fn snapshot(&self, concurrency: ConcurrencyStats) -> MetricsSnapshot {
        let errors = self
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(code, count)| ErrorMetric {
                code: code.clone(),
                count: *count,
            })
            .collect();
        MetricsSnapshot {
            version: METRICS_VERSION,
            uptime_micros: duration_micros(self.started.elapsed()),
            connections: ConnectionMetrics {
                active: self.active_connections.load(Ordering::Relaxed),
                accepted: self.accepted_connections.load(Ordering::Relaxed),
                rejected: self.rejected_connections.load(Ordering::Relaxed),
            },
            operations: OperationsMetrics {
                read: self.read.snapshot(),
                write: self.write.snapshot(),
                introspection: self.introspection.snapshot(),
                receipt: self.receipt.snapshot(),
                stream: self.stream.snapshot(),
                maintenance: self.maintenance.snapshot(),
            },
            errors,
            error_code_overflow: self.error_code_overflow.load(Ordering::Relaxed),
            concurrency,
            receipts: ReceiptMetrics {
                count: self.receipt_count.load(Ordering::Relaxed),
                encoded_bytes: self.receipt_encoded_bytes.load(Ordering::Relaxed),
                max_count: self.receipt_max_count.load(Ordering::Relaxed),
                max_encoded_bytes: self.receipt_max_encoded_bytes.load(Ordering::Relaxed),
            },
        }
    }

    fn operation(&self, operation: MetricOperation) -> &OperationCounters {
        match operation {
            MetricOperation::Read => &self.read,
            MetricOperation::Write => &self.write,
            MetricOperation::Introspection => &self.introspection,
            MetricOperation::Receipt => &self.receipt,
            MetricOperation::Stream => &self.stream,
            MetricOperation::Maintenance => &self.maintenance,
        }
    }
}

impl OperationCounters {
    fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            latency_sum_micros: AtomicU64::new(0),
            latency_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn record(&self, elapsed: Duration, failed: bool) {
        let micros = duration_micros(elapsed);
        saturating_add(&self.requests, 1);
        saturating_add(&self.latency_count, 1);
        saturating_add(&self.latency_sum_micros, micros);
        if failed {
            saturating_add(&self.failures, 1);
        }
        for (limit, bucket) in LATENCY_BUCKETS_MICROS
            .iter()
            .zip(self.latency_buckets.iter())
        {
            if micros <= *limit {
                saturating_add(bucket, 1);
            }
        }
    }

    fn snapshot(&self) -> OperationMetrics {
        OperationMetrics {
            requests: self.requests.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            latency: LatencyHistogram {
                count: self.latency_count.load(Ordering::Relaxed),
                sum_micros: self.latency_sum_micros.load(Ordering::Relaxed),
                buckets: LATENCY_BUCKETS_MICROS
                    .iter()
                    .zip(self.latency_buckets.iter())
                    .map(|(le_micros, count)| LatencyBucket {
                        le_micros: *le_micros,
                        count: count.load(Ordering::Relaxed),
                    })
                    .collect(),
            },
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn saturating_add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

#[cfg(feature = "metrics")]
impl MetricsSnapshot {
    /// Render a Prometheus-compatible text exposition without starting a listener.
    pub fn prometheus_text(&self) -> String {
        crate::metrics_export::render(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_labels_stop_at_the_fixed_capacity() {
        let registry = MetricsRegistry::new(None);
        for index in 0..=MAX_METRIC_ERROR_CODES {
            registry.record(
                MetricOperation::Read,
                Duration::ZERO,
                Some(&format!("E_TEST_{index}")),
            );
        }
        let snapshot = registry.snapshot(ConcurrencyStats::default());
        assert_eq!(snapshot.errors.len(), MAX_METRIC_ERROR_CODES);
        assert_eq!(snapshot.error_code_overflow, 1);
        assert_eq!(
            snapshot.operations.read.requests,
            (MAX_METRIC_ERROR_CODES + 1) as u64
        );
        assert_eq!(
            snapshot.operations.read.failures,
            snapshot.operations.read.requests
        );
    }
}
