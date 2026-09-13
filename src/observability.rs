//! Value-free, transport-neutral request and slow-query events.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{
    ExecutionObservation, ExecutionPlanObservation, MutationProfile, QueryAnalysis,
    QueryPhaseObservation,
};

pub const OBSERVABILITY_VERSION: u32 = 1;

#[derive(Clone)]
pub struct ObserverConfig {
    pub slow_query_threshold: Option<Duration>,
    /// Deterministic slow-event sampling rate, from 0 through 10,000.
    pub slow_query_sample_basis_points: u16,
    /// Explicit secret used to correlate request IDs without retaining them.
    pub request_id_hmac_key: Option<Vec<u8>>,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            slow_query_threshold: None,
            slow_query_sample_basis_points: 10_000,
            request_id_hmac_key: None,
        }
    }
}

impl ObserverConfig {
    pub fn validate(&self) -> crate::Result<()> {
        if self.slow_query_sample_basis_points > 10_000 {
            return Err(crate::Error::new(
                "E_OBSERVER_CONFIG",
                "slow query sample rate must be at most 10,000 basis points",
            ));
        }
        if self.request_id_hmac_key.as_ref().is_some_and(Vec::is_empty) {
            return Err(crate::Error::new(
                "E_OBSERVER_CONFIG",
                "request ID HMAC key must not be empty",
            ));
        }
        Ok(())
    }
}

pub trait RequestObserver: Send + Sync + 'static {
    fn observe(&self, event: &ObservabilityEvent);
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", content = "request", rename_all = "snake_case")]
pub enum ObservabilityEvent {
    RequestTerminal(RequestEvent),
    SlowQuery(RequestEvent),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestOperation {
    Read,
    Write,
    Introspection,
    Receipt,
    StreamQuery,
    StreamCancel,
    Maintenance,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestTerminal {
    Completed,
    Failed,
    Cancelled,
    DeadlineExceeded,
    StorageOutcomeUncertain,
    Dropped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLimit {
    Deadline,
    Request,
    Response,
    BoundedResource,
    Concurrency,
    OperationRegistry,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestPhaseTimings {
    pub total_micros: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepare_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emission_micros: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestWork {
    pub returned_rows: usize,
    pub rows_examined: usize,
    pub rows_decoded: usize,
    pub index_entries_examined: usize,
    pub row_cache_hits: usize,
    pub row_cache_misses: usize,
    pub batches: usize,
    pub working_peak_bytes: usize,
}

impl RequestWork {
    pub(crate) fn from_execution(execution: Option<ExecutionObservation>, returned: usize) -> Self {
        let value = execution.unwrap_or_default();
        Self {
            returned_rows: returned,
            rows_examined: value.rows_examined(),
            rows_decoded: value.rows_decoded,
            index_entries_examined: value.index_entries_examined,
            row_cache_hits: value.row_cache_hits,
            row_cache_misses: value.row_cache_misses,
            batches: value.batches,
            working_peak_bytes: value.working_peak_bytes,
        }
    }

    pub(crate) fn from_analysis(value: QueryAnalysis) -> Self {
        Self {
            returned_rows: value.returned_rows,
            rows_examined: value.rows_examined,
            rows_decoded: value.rows_decoded,
            index_entries_examined: value.index_entries_examined,
            row_cache_hits: value.row_cache_hits,
            row_cache_misses: value.row_cache_misses,
            batches: value.batches,
            working_peak_bytes: value.working_peak_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestEvent {
    pub version: u32,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    pub operation: RequestOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_revision: Option<u64>,
    pub terminal: RequestTerminal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limit: Option<ResourceLimit>,
    pub partial: bool,
    pub phases: RequestPhaseTimings,
    pub work: RequestWork,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<ExecutionPlanObservation>,
}

pub(crate) struct EventInput<'a> {
    pub request_id: Option<&'a str>,
    pub protocol_version: Option<u32>,
    pub operation: RequestOperation,
    pub schema_revision: Option<u64>,
    pub elapsed: Duration,
    pub execution: Option<ExecutionObservation>,
    pub query_phases: Option<QueryPhaseObservation>,
    pub analysis: Option<QueryAnalysis>,
    pub mutation: Option<MutationProfile>,
    pub returned_rows: usize,
    pub plan: Option<ExecutionPlanObservation>,
    pub error_code: Option<&'a str>,
    pub storage_outcome_uncertain: bool,
    pub partial: bool,
    pub emission_micros: Option<u64>,
}

pub(crate) struct ObserverRegistry {
    config: ObserverConfig,
    observer: Arc<dyn RequestObserver>,
    sequence: AtomicU64,
}

impl ObserverRegistry {
    pub(crate) fn new(
        config: ObserverConfig,
        observer: Arc<dyn RequestObserver>,
    ) -> crate::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            observer,
            sequence: AtomicU64::new(0),
        })
    }

    pub(crate) fn record(&self, input: EventInput<'_>) {
        let sequence = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            })
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let terminal = classify_terminal(input.error_code, input.storage_outcome_uncertain);
        let work = input
            .analysis
            .map(RequestWork::from_analysis)
            .unwrap_or_else(|| RequestWork::from_execution(input.execution, input.returned_rows));
        let event = RequestEvent {
            version: OBSERVABILITY_VERSION,
            sequence,
            request_id_digest: input
                .request_id
                .zip(self.config.request_id_hmac_key.as_deref())
                .map(|(id, key)| request_id_digest(key, id)),
            protocol_version: input.protocol_version,
            operation: input.operation,
            schema_revision: input.schema_revision,
            terminal,
            error_code: input.error_code.map(str::to_owned),
            resource_limit: input.error_code.and_then(classify_limit),
            partial: input.partial,
            phases: RequestPhaseTimings {
                total_micros: duration_micros(input.elapsed),
                prepare_micros: input.query_phases.map(|value| value.prepare_micros),
                plan_micros: input.query_phases.map(|value| value.plan_micros),
                execution_micros: input
                    .query_phases
                    .map(|value| value.execution_micros)
                    .or_else(|| input.analysis.map(|value| value.execution_micros)),
                candidate_micros: input.mutation.map(|value| value.candidate_micros),
                commit_micros: input.mutation.map(|value| value.durable_commit_micros),
                emission_micros: input.emission_micros,
            },
            work,
            plan: input.plan,
        };
        self.emit(ObservabilityEvent::RequestTerminal(event.clone()));
        if matches!(
            event.operation,
            RequestOperation::Read | RequestOperation::Write | RequestOperation::StreamQuery
        ) && self
            .config
            .slow_query_threshold
            .is_some_and(|threshold| input.elapsed >= threshold)
            && sampled(sequence, self.config.slow_query_sample_basis_points)
        {
            self.emit(ObservabilityEvent::SlowQuery(event));
        }
    }

    fn emit(&self, event: ObservabilityEvent) {
        let observer = Arc::clone(&self.observer);
        let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(&event)));
    }
}

fn classify_terminal(code: Option<&str>, uncertain: bool) -> RequestTerminal {
    if uncertain {
        RequestTerminal::StorageOutcomeUncertain
    } else {
        match code {
            None => RequestTerminal::Completed,
            Some("E_CANCELLED") => RequestTerminal::Cancelled,
            Some("E_TIMEOUT") => RequestTerminal::DeadlineExceeded,
            Some("E_OPERATION_DROPPED") => RequestTerminal::Dropped,
            Some(_) => RequestTerminal::Failed,
        }
    }
}

fn classify_limit(code: &str) -> Option<ResourceLimit> {
    match code {
        "E_TIMEOUT" => Some(ResourceLimit::Deadline),
        "E_OPERATION_CAPACITY" => Some(ResourceLimit::OperationRegistry),
        "E_BUSY" => Some(ResourceLimit::Concurrency),
        "E_LIMIT" => Some(ResourceLimit::BoundedResource),
        "E_REQUEST_TOO_LARGE" => Some(ResourceLimit::Request),
        "E_RESPONSE_TOO_LARGE" => Some(ResourceLimit::Response),
        _ => None,
    }
}

fn request_id_digest(key: &[u8], request_id: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any non-empty key");
    mac.update(request_id.as_bytes());
    let bytes = mac.finalize().into_bytes();
    let mut encoded = String::with_capacity(7 + bytes.len() * 2);
    encoded.push_str("hmac1:");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn sampled(sequence: u64, basis_points: u16) -> bool {
    basis_points == 10_000
        || basis_points > 0
            && sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) % 10_000 < u64::from(basis_points)
}

pub(crate) fn duration_micros(value: Duration) -> u64 {
    u64::try_from(value.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(feature = "tracing")]
#[derive(Debug, Default)]
pub struct TracingObserver;

#[cfg(feature = "tracing")]
impl RequestObserver for TracingObserver {
    fn observe(&self, event: &ObservabilityEvent) {
        let payload = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_owned());
        tracing::event!(target: "unionid::request", tracing::Level::INFO, event = %payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_boundaries_are_stable() {
        assert!(!sampled(1, 0));
        assert!(sampled(1, 10_000));
        assert_eq!(sampled(42, 500), sampled(42, 500));
    }

    #[test]
    fn terminal_classification_distinguishes_uncertain_storage() {
        assert_eq!(
            classify_terminal(Some("E_STORAGE"), true),
            RequestTerminal::StorageOutcomeUncertain
        );
        assert_eq!(
            classify_terminal(Some("E_STORAGE"), false),
            RequestTerminal::Failed
        );
        assert_eq!(
            classify_terminal(Some("E_TIMEOUT"), false),
            RequestTerminal::DeadlineExceeded
        );
    }
}
