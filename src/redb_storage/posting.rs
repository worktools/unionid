//! Projection shared by ordinary indexes and reference reverse postings.
//! Both use the same durable key and row consistency checks; only projection
//! and target-existence semantics differ.

use crate::db::{Database, DurableTable, IndexDefinition, ReferenceDefinition};
use crate::error::Result;
use crate::model::{Row, Value};

#[derive(Clone, Copy)]
pub(super) enum PostingDefinition<'a> {
    Index(&'a IndexDefinition),
    Reference(&'a ReferenceDefinition),
}

impl PostingDefinition<'_> {
    pub(super) fn id(self) -> u64 {
        match self {
            Self::Index(index) => index.id,
            Self::Reference(reference) => reference.id,
        }
    }

    pub(super) fn table_id(self) -> u64 {
        match self {
            Self::Index(index) => index.table_id,
            Self::Reference(reference) => reference.table_id,
        }
    }

    pub(super) fn component_count(self) -> usize {
        match self {
            Self::Index(index) => index.components.len().max(1),
            Self::Reference(reference) => reference.components.len(),
        }
    }

    pub(super) fn project(
        self,
        database: &Database,
        table: &DurableTable,
        row: &Row,
    ) -> Result<Option<Value>> {
        match self {
            Self::Index(index) => database.source_index_value_if_included(&table.name, index, row),
            Self::Reference(reference) => reference.source_value(&row.fields),
        }
    }

    pub(super) fn is_unique(self, table: &DurableTable) -> bool {
        match self {
            Self::Index(index) => {
                index.kind.is_unique() || index.is_primary_index(table.primary_key.as_deref())
            }
            Self::Reference(_) => false,
        }
    }

    pub(super) fn description(self, table: &DurableTable) -> String {
        match self {
            Self::Index(index) => format!("index '{} ({})'", table.name, index.display_shape()),
            Self::Reference(reference) => {
                format!("reference {} on table '{}'", reference.id, table.name)
            }
        }
    }
}
