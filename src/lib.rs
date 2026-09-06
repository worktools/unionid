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
mod local;
mod matching;
pub mod migration;
pub mod model;
mod params;
pub mod protocol;
pub mod query;
mod redb_storage;
pub mod schema;
pub mod server;
pub mod snapshot;
pub mod syntax;
pub mod wal;

pub use backup::BackupInfo;
pub use db::{QueryResponse, SchemaInfo, UpsertAction};
pub use engine::{Engine, PreparedQuery, StorageIntegrity};
pub use error::{Error, Result, Span};
pub use migration::{MigrationApply, MigrationFile, MigrationPlan, MigrationStatus};
pub use model::{RowId, Value};
pub use protocol::{Request as ProtocolRequest, Response as ProtocolResponse, WireValue};
pub use schema::{
    SchemaCheck, SchemaDiff, SchemaDiffImpact, SchemaDiffOperation, SchemaDiffTableImpact,
};
