//! A small typed database with a whitespace-oriented query language.
//!
//! The Engine is shared by embedded applications, the local CLI, and TCP.
pub mod cli;
pub mod db;
pub mod engine;
pub mod error;
pub mod model;
pub mod query;
pub mod server;
pub mod snapshot;
pub mod syntax;
pub mod wal;

pub use db::QueryResponse;
pub use engine::Engine;
pub use error::{Error, Result, Span};
pub use model::Value;
