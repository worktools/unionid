//! A small typed database with a whitespace-oriented query language.
//!
//! The Engine is shared by embedded applications, the local CLI, and TCP.
pub mod cli;
pub mod codec;
pub mod db;
pub mod engine;
pub mod error;
mod expression;
mod matching;
pub mod migration;
pub mod model;
pub mod query;
mod redb_storage;
pub mod server;
pub mod snapshot;
pub mod syntax;
pub mod wal;

pub use db::{QueryResponse, SchemaInfo, UpsertAction};
pub use engine::{Engine, StorageIntegrity};
pub use error::{Error, Result, Span};
pub use model::{RowId, Value};
