//! A small typed database with a whitespace-oriented query language.
//!
//! The Engine is shared by embedded applications, the local CLI, and TCP.
#[cfg(feature = "asynchronous")]
pub mod asynchronous;
pub mod backup;
pub mod cli;
pub mod client;
pub mod codec;
pub mod codegen;
mod control;
pub mod db;
pub mod engine;
pub mod error;
mod expression;
pub mod formatter;
pub mod idempotency;
pub mod introspection;
pub mod llm_docs;
mod local;
mod matching;
pub mod metrics;
#[cfg(feature = "metrics")]
mod metrics_export;
pub mod migration;
pub mod model;
pub mod observability;
mod ordered_key;
mod pagination;
mod params;
pub mod portable;
pub mod profile;
pub mod project;
pub mod protocol;
pub mod query;
pub mod query_contract;
mod redb_storage;
mod repl;
mod row_source;
pub mod scalars;
pub mod schema;
mod serde_value;
pub mod server;
pub mod snapshot;
pub mod stream;
pub mod syntax;
pub mod wal;

pub use backup::BackupInfo;
pub use backup::incremental::{
    BACKUP_JOURNAL_STATUS_VERSION, BackupJournalConfig, BackupJournalState, BackupJournalStatus,
    DEFAULT_JOURNAL_MAX_BYTES, DEFAULT_JOURNAL_MAX_COMMITS, HARD_JOURNAL_MAX_BYTES,
    HARD_JOURNAL_MAX_COMMITS,
};
#[cfg(feature = "http-client")]
pub use client::http::{HttpClient, HttpStream};
#[cfg(feature = "asynchronous")]
pub use client::{AsyncTcpClient, AsyncTcpStream};
pub use client::{MAX_RESPONSE_BYTES, TcpClient, TypedStreamEvent};
pub use db::{
    ExecutionPlanObservation, ExistsCorrelationPlan, ExistsPlan, IndexRangePlan, IndexTraversal,
    LookupPlan, MAX_EXECUTION_PLAN_STAGES, MAX_EXISTS_DRIVERS, PageAccessKind, PageInfo, PageOrder,
    PagePlan, QueryAccessKind, QueryAccessPlan, QueryAnalysis, QueryPlan, QueryPlanStage,
    QueryResponse, QueryStageKind, SchemaInfo, TypedPage, UpsertAction,
};
pub use engine::{
    Engine, MutationProfile, PreparedQuery, StorageCompaction, StorageIntegrity, StorageUpgrade,
};
pub use error::{Error, Result, Span};
pub use formatter::format_source;
pub use idempotency::{
    IdempotencyBoundary, IdempotencyDurability, IdempotencyPruneOptions, IdempotencyPruneResult,
    IdempotencyReceipt, IdempotencyStatus, IdempotentExecution,
};
pub use introspection::{Introspection, IntrospectionKind, StorageMode, StorageVersions};
pub use llm_docs::{
    LLM_QUERY_DOCS_VERSION, LlmQueryDocs, LlmQueryExample, QUERY_LANGUAGE_VERSION, query_docs,
    render_query_docs_markdown,
};
pub use metrics::{
    ConnectionMetrics, ErrorMetric, LATENCY_BUCKETS_MICROS, LatencyBucket, LatencyHistogram,
    MAX_METRIC_ERROR_CODES, METRICS_VERSION, MetricsSnapshot, OperationMetrics, OperationsMetrics,
    ReceiptMetrics,
};
pub use migration::{
    MigrationAbort, MigrationApply, MigrationFile, MigrationMaintenance, MigrationMaintenancePhase,
    MigrationPlan, MigrationProgress, MigrationStatus,
};
pub use model::{RowId, Value};
#[cfg(feature = "tracing")]
pub use observability::TracingObserver;
pub use observability::{
    OBSERVABILITY_VERSION, ObservabilityEvent, ObserverConfig, RequestEvent, RequestObserver,
    RequestOperation, RequestPhaseTimings, RequestTerminal, RequestWork, ResourceLimit,
};
pub use portable::{
    CompatibilityAxis, CompatibilityFinding, CompatibilityLevel, EvolutionReport, PortableContract,
    PortableSchemaIdentity, SchemaDescription,
};
pub use profile::{
    DurableCommitMode, DurableCommitProfile, ExecutionObservation, MigrationProfile,
    QueryPhaseObservation, StorageCheckProfile, StorageOpenProfile,
};
pub use protocol::{
    IdempotencyMetadata, ReceiptOperation, ReceiptOperationResult, Request as ProtocolRequest,
    Response as ProtocolResponse, WireValue,
};
pub use query::{PageDirection, PageSpec};
pub use query_contract::{
    QUERY_DESCRIPTION_VERSION, QueryCardinality, QueryDescription, QueryFieldDescription,
    QueryOperation, QueryParameterDescription, QueryResultDescription,
};
pub use schema::{
    SchemaBuilder, SchemaCheck, SchemaDiff, SchemaDiffImpact, SchemaDiffOperation,
    SchemaDiffTableImpact, UnionidSchema,
};
pub use server::{
    CancelResult, CancelStatus, ConcurrencyStats, ConcurrentEngine, OperationOutcome,
    ReadOperation, ServerStats,
};
pub use syntax::{InputStatus, input_status};
