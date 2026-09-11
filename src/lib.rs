//! A small typed database with a whitespace-oriented query language.
//!
//! The Engine is shared by embedded applications, the local CLI, and TCP.
pub mod backup;
pub mod cli;
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
mod local;
mod matching;
pub mod migration;
pub mod model;
mod ordered_key;
mod pagination;
mod params;
pub mod profile;
pub mod protocol;
pub mod query;
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
pub use db::{
    IndexRangePlan, IndexTraversal, PageAccessKind, PageInfo, PageOrder, PagePlan, QueryAccessKind,
    QueryAccessPlan, QueryPlan, QueryPlanStage, QueryResponse, QueryStageKind, SchemaInfo,
    TypedPage, UpsertAction,
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
pub use migration::{
    MigrationAbort, MigrationApply, MigrationFile, MigrationMaintenance, MigrationMaintenancePhase,
    MigrationPlan, MigrationProgress, MigrationStatus,
};
pub use model::{RowId, Value};
pub use profile::{
    DurableCommitMode, DurableCommitProfile, ExecutionObservation, MigrationProfile,
    StorageCheckProfile, StorageOpenProfile,
};
pub use protocol::{
    IdempotencyMetadata, ReceiptOperation, ReceiptOperationResult, Request as ProtocolRequest,
    Response as ProtocolResponse, WireValue,
};
pub use query::{PageDirection, PageSpec};
pub use schema::{
    SchemaCheck, SchemaDiff, SchemaDiffImpact, SchemaDiffOperation, SchemaDiffTableImpact,
};
pub use syntax::{InputStatus, input_status};
