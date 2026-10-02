use std::collections::BTreeSet;
use std::ops::Bound;
use std::sync::Arc;

use crate::control::ExecutionControl;
use crate::error::Result;
use crate::model::{Row, RowId};
use crate::profile::ExecutionObservation;

pub(crate) const SOURCE_BATCH_MAX_ROWS: usize = 1_024;
pub(crate) const SOURCE_BATCH_MAX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const GENERAL_WORKING_MAX_BYTES: usize = 64 * 1024 * 1024;

pub(crate) type EncodedIndexBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub(crate) database_instance: [u8; 16],
    pub(crate) generation: u64,
    pub(crate) sequence: u64,
    pub(crate) schema_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableStats {
    pub(crate) rows: usize,
    pub(crate) rows_exact: bool,
    pub(crate) next_row_id: RowId,
}

#[derive(Debug, Clone)]
pub(crate) struct RowBatch {
    pub(crate) rows: Vec<Arc<Row>>,
    pub(crate) encoded_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexHit {
    pub(crate) boundary: Vec<u8>,
    pub(crate) row_id: RowId,
}

pub(crate) trait RowBatchCursor {
    fn next_batch(
        &mut self,
        control: Option<&ExecutionControl>,
        observation: &mut ExecutionObservation,
    ) -> Result<Option<RowBatch>>;
}

pub(crate) trait IndexHitCursor {
    fn next_batch(&mut self, control: Option<&ExecutionControl>) -> Result<Option<Vec<IndexHit>>>;
}

pub(crate) trait TypedRowSource: Send + Sync {
    /// Optional value-free scan label exposed by `explain` for external row
    /// sources. Resident and durable UnionID tables keep the legacy plan shape.
    fn external_scan_kind(&self) -> Option<&'static str> {
        None
    }

    fn snapshot_identity(&self) -> SourceIdentity;

    fn table_stats(&self, table: &str) -> Result<TableStats>;

    fn has_index(&self, table: &str, shape: &str) -> bool;

    /// Whether the reverse posting contains an unchanged source row. Excluded
    /// RowIds belong to the current statement's complete candidate changes.
    fn reference_source_exists(
        &self,
        _reference_id: u64,
        _key: &[u8],
        _excluded: &BTreeSet<RowId>,
        _control: Option<&ExecutionControl>,
    ) -> Result<bool> {
        Err(crate::error::Error::new(
            "E_STORAGE",
            "row source does not support reference postings",
        ))
    }

    fn estimate_index_span(
        &self,
        table: &str,
        shape: &str,
        bounds: &EncodedIndexBounds,
    ) -> Result<usize>;

    fn get_row(
        &self,
        table: &str,
        row_id: RowId,
        control: Option<&ExecutionControl>,
        observation: &mut ExecutionObservation,
    ) -> Result<Option<Arc<Row>>>;

    fn scan_rows<'a>(&'a self, table: &str) -> Result<Box<dyn RowBatchCursor + 'a>>;

    /// Scan rows while allowing external columnar sources to avoid decoding
    /// unused top-level fields. Row-oriented sources may ignore the hint.
    fn scan_rows_projected<'a>(
        &'a self,
        table: &str,
        _projection: &BTreeSet<String>,
    ) -> Result<Box<dyn RowBatchCursor + 'a>> {
        self.scan_rows(table)
    }

    fn scan_index<'a>(
        &'a self,
        table: &str,
        shape: &str,
        bounds: &EncodedIndexBounds,
        reverse: bool,
        read_limit: Option<usize>,
    ) -> Result<Box<dyn IndexHitCursor + 'a>>;
}

/// A request-local view over the already coalesced persistent candidate root.
///
/// The candidate `Database` is one copy-on-write layer over its committed root;
/// mutations update that root and its `LogicalWriteSet` in place. Keeping this
/// adapter deliberately shallow prevents request overlays from forming chains.
pub(crate) struct CandidateRowSource<'a> {
    candidate: &'a dyn TypedRowSource,
}

impl<'a> CandidateRowSource<'a> {
    pub(crate) fn new(candidate: &'a dyn TypedRowSource) -> Self {
        Self { candidate }
    }
}

impl TypedRowSource for CandidateRowSource<'_> {
    fn reference_source_exists(
        &self,
        reference_id: u64,
        key: &[u8],
        excluded: &BTreeSet<RowId>,
        control: Option<&ExecutionControl>,
    ) -> Result<bool> {
        self.candidate
            .reference_source_exists(reference_id, key, excluded, control)
    }
    fn external_scan_kind(&self) -> Option<&'static str> {
        self.candidate.external_scan_kind()
    }

    fn snapshot_identity(&self) -> SourceIdentity {
        self.candidate.snapshot_identity()
    }

    fn table_stats(&self, table: &str) -> Result<TableStats> {
        self.candidate.table_stats(table)
    }

    fn has_index(&self, table: &str, shape: &str) -> bool {
        self.candidate.has_index(table, shape)
    }

    fn estimate_index_span(
        &self,
        table: &str,
        shape: &str,
        bounds: &EncodedIndexBounds,
    ) -> Result<usize> {
        self.candidate.estimate_index_span(table, shape, bounds)
    }

    fn get_row(
        &self,
        table: &str,
        row_id: RowId,
        control: Option<&ExecutionControl>,
        observation: &mut ExecutionObservation,
    ) -> Result<Option<Arc<Row>>> {
        self.candidate.get_row(table, row_id, control, observation)
    }

    fn scan_rows<'a>(&'a self, table: &str) -> Result<Box<dyn RowBatchCursor + 'a>> {
        self.candidate.scan_rows(table)
    }

    fn scan_rows_projected<'a>(
        &'a self,
        table: &str,
        projection: &BTreeSet<String>,
    ) -> Result<Box<dyn RowBatchCursor + 'a>> {
        self.candidate.scan_rows_projected(table, projection)
    }

    fn scan_index<'a>(
        &'a self,
        table: &str,
        shape: &str,
        bounds: &EncodedIndexBounds,
        reverse: bool,
        read_limit: Option<usize>,
    ) -> Result<Box<dyn IndexHitCursor + 'a>> {
        self.candidate
            .scan_index(table, shape, bounds, reverse, read_limit)
    }
}
