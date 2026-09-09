//! Value-free storage phase observations for diagnostics and benchmarks.

use serde::{Deserialize, Serialize};

/// Value-free counters for one query or mutation-target execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExecutionObservation {
    pub index_entries_examined: usize,
    pub rows_decoded: usize,
    pub row_cache_hits: usize,
    pub row_cache_misses: usize,
    pub batches: usize,
    pub working_peak_bytes: usize,
}

impl ExecutionObservation {
    pub(crate) fn observe_working_bytes(&mut self, bytes: usize) {
        self.working_peak_bytes = self.working_peak_bytes.max(bytes);
    }
}

/// Timings and cardinalities captured while opening a durable redb database.
///
/// The profile is published only after a successful open and never contains
/// schema names, keys, values, receipts, or cursor material.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct StorageOpenProfile {
    pub total_micros: u64,
    pub redb_open_micros: u64,
    pub bootstrap_micros: u64,
    pub meta_micros: u64,
    pub catalog_micros: u64,
    pub rows_micros: u64,
    pub indexes_micros: u64,
    pub migrations_micros: u64,
    pub receipts_micros: u64,
    pub database_construct_micros: u64,
    pub validation_micros: u64,
    pub catalog_entries: usize,
    pub row_entries: usize,
    pub index_entries: usize,
    pub migration_entries: usize,
    pub receipt_entries: usize,
    pub row_bytes: u64,
    pub index_key_bytes: u64,
    pub receipt_bytes: u64,
    pub fresh: bool,
    pub cursor_upgrade: bool,
    pub read_only: bool,
    /// True when open published a transaction-backed view without loading
    /// durable rows or secondary-index entries into resident maps.
    pub bounded_view: bool,
}

/// Durable preparation path used by a successful mutation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableCommitMode {
    #[default]
    Incremental,
    FullRebuild,
}

/// Timings and change counts for one successful redb commit.
///
/// `prepare_micros` includes validation and all sub-phases before the write
/// transaction begins. Full rebuilds additionally populate reload, encode,
/// and diff timings. `transaction_apply_micros` ends immediately before redb
/// commit, and `sync_micros` covers the synchronous commit call.
/// `encoded_change_bytes` measures keys, expected before-images, and new
/// encodings retained by the delta plan; it is not a physical I/O counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct DurableCommitProfile {
    pub mode: DurableCommitMode,
    pub total_micros: u64,
    pub prepare_micros: u64,
    pub reload_previous_micros: u64,
    pub encode_next_micros: u64,
    pub diff_micros: u64,
    pub transaction_apply_micros: u64,
    pub sync_micros: u64,
    pub catalog_changes: usize,
    pub row_changes: usize,
    pub index_changes: usize,
    pub migration_changes: usize,
    pub receipt_changes: usize,
    pub encoded_change_bytes: u64,
}
