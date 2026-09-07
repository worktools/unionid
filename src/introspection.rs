use serde::{Deserialize, Serialize};

use crate::SchemaInfo;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    #[default]
    Memory,
    Redb,
    LegacyWal,
    LegacyWalSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntrospectionKind {
    Schema,
    Tables,
    Types,
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Introspection {
    pub schema: SchemaInfo,
    pub schema_source: String,
    pub tables: Vec<String>,
    pub types: Vec<String>,
    pub fields: Vec<String>,
    pub storage: StorageMode,
    pub migration_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_head: Option<String>,
}
