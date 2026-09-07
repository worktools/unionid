//! A small typed database with a whitespace-oriented query language.
//!
//! The Engine is shared by embedded applications, the local CLI, and TCP.
pub mod backup;
pub mod cli;
pub mod codec;
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
mod pagination;
mod params;
pub mod protocol;
pub mod query;
mod redb_storage;
mod repl;
pub mod schema;
mod serde_value;
pub mod server;
pub mod snapshot;
pub mod syntax;
pub mod wal;

pub use backup::BackupInfo;
pub use db::{
    PageAccessKind, PageInfo, PageOrder, PagePlan, QueryAccessKind, QueryAccessPlan, QueryPlan,
    QueryPlanStage, QueryResponse, QueryStageKind, SchemaInfo, UpsertAction,
};
pub use engine::{Engine, PreparedQuery, StorageIntegrity};
pub use error::{Error, Result, Span};
pub use formatter::format_source;
pub use idempotency::{
    IdempotencyBoundary, IdempotencyDurability, IdempotencyPruneOptions, IdempotencyPruneResult,
    IdempotencyReceipt, IdempotencyStatus, IdempotentExecution,
};
pub use introspection::{Introspection, IntrospectionKind, StorageMode};
pub use migration::{MigrationApply, MigrationFile, MigrationPlan, MigrationStatus};
pub use model::{RowId, Value};
pub use protocol::{
    IdempotencyMetadata, ReceiptOperation, ReceiptOperationResult, Request as ProtocolRequest,
    Response as ProtocolResponse, WireValue,
};
pub use query::{PageDirection, PageSpec};
pub use schema::{
    SchemaCheck, SchemaDiff, SchemaDiffImpact, SchemaDiffOperation, SchemaDiffTableImpact,
};
pub use syntax::{InputStatus, input_status};
