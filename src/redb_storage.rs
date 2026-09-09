//! Transactional redb storage for the durable Engine mode.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use redb::{
    Database as RedbDatabase, Durability, ReadableDatabase, ReadableTable, TableDefinition,
    TableHandle,
};

use crate::codec::{PRODUCTION_VALUE_CODEC_VERSION, VALUE_CODEC_VERSION};
use crate::db::{Database, DurableCatalogEntry, DurableMeta, LogicalWriteSet};
use crate::error::{Error, Result};
use crate::idempotency::{
    IdempotencyReceipt, MAX_IDEMPOTENCY_RECEIPT_BYTES, MAX_IDEMPOTENCY_TOTAL_BYTES, ReceiptMap,
    encoded_receipt, ensure_legacy_receipts, validate_receipts,
};
use crate::introspection::StorageVersions;
use crate::migration::MigrationEntry;
use crate::model::Value;
use crate::profile::{DurableCommitMode, DurableCommitProfile, StorageOpenProfile};
use crate::row_source::{
    EncodedIndexBounds, IndexHit, IndexHitCursor, RowBatch, RowBatchCursor, SOURCE_BATCH_MAX_BYTES,
    SOURCE_BATCH_MAX_ROWS, SourceIdentity, TableStats, TypedRowSource,
};
use crate::{ExecutionObservation, RowId};

const LEGACY_STORAGE_FORMAT_VERSION: u32 = 1;
const RECEIPT_STORAGE_FORMAT_VERSION: u32 = 2;
const CURSOR_STORAGE_FORMAT_VERSION: u32 = 3;
const SCALAR_STORAGE_FORMAT_VERSION: u32 = 4;
pub(crate) const PRODUCTION_STORAGE_FORMAT_VERSION: u32 = 5;
const CATALOG_CODEC_VERSION: u16 = 2;
const SCALAR_CATALOG_CODEC_VERSION: u16 = 3;
const PRODUCTION_CATALOG_CODEC_VERSION: u16 = 4;
const LEGACY_CATALOG_CODEC_VERSION: u16 = 1;
const INDEX_KEY_VERSION: u16 = 1;
const SCALAR_INDEX_KEY_VERSION: u16 = 2;
const PRODUCTION_INDEX_KEY_VERSION: u16 = crate::ordered_key::INDEX_KEY_CODEC_VERSION;
const MIGRATION_CODEC_VERSION: u16 = 1;
const RECEIPT_CODEC_VERSION: u16 = 1;
const PRODUCTION_RECEIPT_CODEC_VERSION: u16 = 2;

pub(crate) fn production_versions() -> StorageVersions {
    let layout = StorageLayout::production();
    StorageVersions {
        format: layout.format,
        catalog_codec: layout.catalog,
        value_codec: layout.value,
        index_key_codec: layout.index,
        migration_codec: layout.migration,
        receipt_codec: layout.receipt,
        backup_codec: crate::backup::PRODUCTION_BACKUP_FORMAT_VERSION,
    }
}

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const CATALOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("catalog");
const ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("rows");
const SECONDARY_INDEX: TableDefinition<&[u8], u8> = TableDefinition::new("secondary_index");
const MIGRATION_LEDGER: TableDefinition<u64, &[u8]> = TableDefinition::new("migration_ledger");
const IDEMPOTENCY_RECEIPTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("idempotency_receipts");

const FORMAT_KEY: &str = "storage_format_version";
const CATALOG_CODEC_KEY: &str = "catalog_codec_version";
const VALUE_CODEC_KEY: &str = "value_codec_version";
const INDEX_KEY_CODEC_KEY: &str = "index_key_version";
const MIGRATION_CODEC_KEY: &str = "migration_codec_version";
const RECEIPT_CODEC_KEY: &str = "receipt_codec_version";
const SEQUENCE_KEY: &str = "commit_sequence";
const SCHEMA_REVISION_KEY: &str = "schema_revision";
const NEXT_CATALOG_ID_KEY: &str = "next_catalog_id";
const SCHEMA_HASH_KEY: &str = "schema_hash";
const CURSOR_INSTANCE_ID_KEY: &str = "cursor_instance_id";
const CURSOR_SECRET_KEY: &str = "cursor_secret";
const CATALOG_MAGIC: &[u8; 4] = b"UIDC";
const INDEX_MAGIC: &[u8; 4] = b"UIDI";
const MIGRATION_MAGIC: &[u8; 4] = b"UIDM";
const RECEIPT_MAGIC: &[u8; 4] = b"UIDR";

pub(crate) struct RedbStore {
    database: RedbDatabase,
    committed: DurableHead,
}

type RedbOpen = (
    RedbStore,
    Arc<Database>,
    ReceiptMap,
    Arc<dyn TypedRowSource>,
    StorageOpenProfile,
);
type BoundedViewLoad = (
    Arc<Database>,
    ReceiptMap,
    DurableHead,
    Arc<dyn TypedRowSource>,
    StorageOpenProfile,
);

const SNAPSHOT_ROW_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct RedbReadSource {
    transaction: redb::ReadTransaction,
    metadata: Arc<Database>,
    identity: SourceIdentity,
    cache: Mutex<SnapshotRowCache>,
}

#[derive(Default)]
struct SnapshotRowCache {
    entries: BTreeMap<(u64, RowId), CachedRow>,
    bytes: usize,
    clock: u64,
}

struct CachedRow {
    row: Arc<crate::model::Row>,
    bytes: usize,
    last_used: u64,
}

impl SnapshotRowCache {
    fn get(&mut self, key: (u64, RowId)) -> Option<Arc<crate::model::Row>> {
        let entry = self.entries.get_mut(&key)?;
        self.clock = self.clock.saturating_add(1);
        entry.last_used = self.clock;
        Some(entry.row.clone())
    }

    fn insert(&mut self, key: (u64, RowId), row: Arc<crate::model::Row>, bytes: usize) {
        if bytes > SNAPSHOT_ROW_CACHE_MAX_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
        }
        while self.bytes.saturating_add(bytes) > SNAPSHOT_ROW_CACHE_MAX_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.bytes);
            }
        }
        self.clock = self.clock.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.insert(
            key,
            CachedRow {
                row,
                bytes,
                last_used: self.clock,
            },
        );
        debug_assert!(self.bytes <= SNAPSHOT_ROW_CACHE_MAX_BYTES);
    }
}

impl RedbReadSource {
    fn new(transaction: redb::ReadTransaction, metadata: Arc<Database>) -> Self {
        let identity = SourceIdentity {
            database_instance: metadata.durable_meta().cursor_instance_id,
            generation: 0,
            sequence: metadata.sequence,
            schema_hash: metadata.schema_info().hash,
        };
        Self {
            transaction,
            metadata,
            identity,
            cache: Mutex::new(SnapshotRowCache::default()),
        }
    }
}

struct RedbRowCursor<'a> {
    source: &'a RedbReadSource,
    table: String,
    table_id: u64,
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
    done: bool,
}

struct RedbIndexCursor<'a> {
    source: &'a RedbReadSource,
    index_id: u64,
    component_count: u8,
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
    reverse: bool,
    reverse_group: Option<ReverseIndexGroup>,
    remaining: Option<usize>,
    done: bool,
}

struct ReverseIndexGroup {
    before: Vec<u8>,
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
}

impl TypedRowSource for RedbReadSource {
    fn snapshot_identity(&self) -> SourceIdentity {
        self.identity.clone()
    }

    fn table_stats(&self, table: &str) -> Result<TableStats> {
        let (_, next_row_id, _) = self.metadata.source_table_info(table)?;
        Ok(TableStats {
            // Format 5 has no persisted live-row cardinality. The monotonic
            // allocation cursor is a safe upper bound until format 6 adds a
            // generation manifest with exact statistics.
            rows: usize::try_from(next_row_id).unwrap_or(usize::MAX),
            rows_exact: false,
            next_row_id,
        })
    }

    fn has_index(&self, table: &str, shape: &str) -> bool {
        self.metadata.source_has_index(table, shape)
    }

    fn estimate_index_span(
        &self,
        table: &str,
        _shape: &str,
        _bounds: &EncodedIndexBounds,
    ) -> Result<usize> {
        // Exact format-5 span cardinality requires walking the durable range.
        // Explain must remain decode-free, so use the allocation upper bound.
        Ok(self.table_stats(table)?.rows)
    }

    fn get_row(
        &self,
        table: &str,
        row_id: RowId,
        control: Option<&crate::control::ExecutionControl>,
        observation: &mut ExecutionObservation,
    ) -> Result<Option<Arc<crate::model::Row>>> {
        check_source_control(control)?;
        let (table_id, _, _) = self.metadata.source_table_info(table)?;
        let cache_key = (table_id, row_id);
        if let Some(row) = self
            .cache
            .lock()
            .map_err(|_| Error::new("E_STORAGE", "snapshot row cache lock is poisoned"))?
            .get(cache_key)
        {
            observation.row_cache_hits = observation.row_cache_hits.saturating_add(1);
            return Ok(Some(row));
        }
        observation.row_cache_misses = observation.row_cache_misses.saturating_add(1);
        let key = encode_row_key(table_id, row_id);
        let bytes = {
            let rows = self
                .transaction
                .open_table(ROWS)
                .map_err(|error| storage_error("open rows table", error))?;
            rows.get(key.as_slice())
                .map_err(|error| storage_error("read durable row", error))?
                .map(|value| value.value().to_vec())
        };
        let Some(bytes) = bytes else {
            return Err(Error::new(
                "E_STORAGE",
                "durable index references a missing row",
            ));
        };
        check_source_control(control)?;
        let row = self.metadata.decode_source_row(table, row_id, &bytes)?;
        observation.rows_decoded = observation.rows_decoded.saturating_add(1);
        let cache_bytes = self.metadata.source_row_cache_size(&row, bytes.len())?;
        self.cache
            .lock()
            .map_err(|_| Error::new("E_STORAGE", "snapshot row cache lock is poisoned"))?
            .insert(cache_key, row.clone(), cache_bytes);
        Ok(Some(row))
    }

    fn scan_rows<'a>(&'a self, table: &str) -> Result<Box<dyn RowBatchCursor + 'a>> {
        let (table_id, _, _) = self.metadata.source_table_info(table)?;
        Ok(Box::new(RedbRowCursor {
            source: self,
            table: table.to_owned(),
            table_id,
            lower: Bound::Included(encode_row_key(table_id, 0)),
            upper: Bound::Included(encode_row_key(table_id, u64::MAX)),
            done: false,
        }))
    }

    fn scan_index<'a>(
        &'a self,
        table: &str,
        shape: &str,
        bounds: &EncodedIndexBounds,
        reverse: bool,
        read_limit: Option<usize>,
    ) -> Result<Box<dyn IndexHitCursor + 'a>> {
        let (index_id, component_count) = self.metadata.source_index_info(table, shape)?;
        let (lower, upper) = durable_index_bounds(index_id, component_count, bounds)?;
        Ok(Box::new(RedbIndexCursor {
            source: self,
            index_id,
            component_count,
            lower,
            upper,
            reverse,
            reverse_group: None,
            remaining: read_limit,
            done: read_limit == Some(0),
        }))
    }
}

impl RowBatchCursor for RedbRowCursor<'_> {
    fn next_batch(
        &mut self,
        control: Option<&crate::control::ExecutionControl>,
        observation: &mut ExecutionObservation,
    ) -> Result<Option<RowBatch>> {
        check_source_control(control)?;
        if self.done {
            return Ok(None);
        }
        let table = self
            .source
            .transaction
            .open_table(ROWS)
            .map_err(|error| storage_error("open rows table", error))?;
        let bounds = (borrowed_bound(&self.lower), borrowed_bound(&self.upper));
        let range = table
            .range::<&[u8]>(bounds)
            .map_err(|error| storage_error("scan durable rows", error))?;
        let mut rows = Vec::with_capacity(SOURCE_BATCH_MAX_ROWS);
        let mut encoded_bytes = 0_usize;
        let mut reached_end = true;
        for entry in range {
            check_source_control(control)?;
            let (key, value) =
                entry.map_err(|error| storage_error("read durable row range", error))?;
            let key = key.value();
            let value = value.value();
            let (table_id, row_id) = decode_row_key(key)?;
            if table_id != self.table_id {
                return Err(Error::new(
                    "E_STORAGE",
                    "durable row range crossed its table boundary",
                ));
            }
            if !rows.is_empty()
                && (rows.len() == SOURCE_BATCH_MAX_ROWS
                    || encoded_bytes.saturating_add(value.len()) > SOURCE_BATCH_MAX_BYTES)
            {
                reached_end = false;
                break;
            }
            let row = self
                .source
                .metadata
                .decode_source_row(&self.table, row_id, value)?;
            encoded_bytes = encoded_bytes.saturating_add(value.len());
            rows.push(row);
            self.lower = Bound::Excluded(key.to_vec());
            if rows.len() == SOURCE_BATCH_MAX_ROWS {
                reached_end = false;
                break;
            }
        }
        self.done = reached_end;
        if rows.is_empty() {
            self.done = true;
            return Ok(None);
        }
        observation.rows_decoded = observation.rows_decoded.saturating_add(rows.len());
        observation.batches = observation.batches.saturating_add(1);
        observation.observe_working_bytes(encoded_bytes);
        Ok(Some(RowBatch {
            rows,
            encoded_bytes,
        }))
    }
}

impl IndexHitCursor for RedbIndexCursor<'_> {
    fn next_batch(
        &mut self,
        control: Option<&crate::control::ExecutionControl>,
    ) -> Result<Option<Vec<IndexHit>>> {
        check_source_control(control)?;
        if self.done || self.remaining == Some(0) {
            return Ok(None);
        }
        if self.reverse {
            return self.next_reverse_batch(control);
        }
        let table = self
            .source
            .transaction
            .open_table(SECONDARY_INDEX)
            .map_err(|error| storage_error("open secondary index table", error))?;
        let bounds = (borrowed_bound(&self.lower), borrowed_bound(&self.upper));
        let mut range = table
            .range::<&[u8]>(bounds)
            .map_err(|error| storage_error("scan durable secondary index", error))?;
        let mut hits = Vec::with_capacity(SOURCE_BATCH_MAX_ROWS);
        let mut encoded_bytes = 0_usize;
        let mut reached_end = true;
        loop {
            let Some(entry) = range.next() else {
                break;
            };
            check_source_control(control)?;
            let (key, _) =
                entry.map_err(|error| storage_error("read durable index range", error))?;
            let key = key.value();
            if !hits.is_empty()
                && (hits.len() == SOURCE_BATCH_MAX_ROWS
                    || encoded_bytes.saturating_add(key.len()) > SOURCE_BATCH_MAX_BYTES)
            {
                reached_end = false;
                break;
            }
            let hit = decode_durable_index_hit(key, self.index_id, self.component_count)?;
            encoded_bytes = encoded_bytes.saturating_add(key.len());
            hits.push(hit);
            self.lower = Bound::Excluded(key.to_vec());
            if let Some(remaining) = &mut self.remaining {
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 {
                    self.done = true;
                    break;
                }
            }
            if hits.len() == SOURCE_BATCH_MAX_ROWS {
                reached_end = false;
                break;
            }
        }
        self.done |= reached_end;
        if hits.is_empty() {
            self.done = true;
            Ok(None)
        } else {
            Ok(Some(hits))
        }
    }
}

impl RedbIndexCursor<'_> {
    fn next_reverse_batch(
        &mut self,
        control: Option<&crate::control::ExecutionControl>,
    ) -> Result<Option<Vec<IndexHit>>> {
        let mut hits = Vec::with_capacity(SOURCE_BATCH_MAX_ROWS);
        let mut encoded_bytes = 0_usize;
        while hits.len() < SOURCE_BATCH_MAX_ROWS && self.remaining != Some(0) {
            check_source_control(control)?;
            if self.reverse_group.is_none() {
                let table = self
                    .source
                    .transaction
                    .open_table(SECONDARY_INDEX)
                    .map_err(|error| storage_error("open secondary index table", error))?;
                let bounds = (borrowed_bound(&self.lower), borrowed_bound(&self.upper));
                let mut range = table
                    .range::<&[u8]>(bounds)
                    .map_err(|error| storage_error("scan durable secondary index", error))?;
                let Some(entry) = range.next_back() else {
                    self.done = true;
                    break;
                };
                let (key, _) =
                    entry.map_err(|error| storage_error("read durable index range", error))?;
                let hit =
                    decode_durable_index_hit(key.value(), self.index_id, self.component_count)?;
                let mut before = durable_index_prefix(self.index_id, self.component_count);
                before.extend_from_slice(&hit.boundary);
                let mut first = before.clone();
                first.extend_from_slice(&0_u64.to_be_bytes());
                let mut last = before.clone();
                last.extend_from_slice(&u64::MAX.to_be_bytes());
                self.reverse_group = Some(ReverseIndexGroup {
                    before,
                    lower: Bound::Included(first),
                    upper: Bound::Included(last),
                });
            }

            let group = self
                .reverse_group
                .as_mut()
                .expect("reverse index group was initialized");
            let table = self
                .source
                .transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| storage_error("open secondary index table", error))?;
            let bounds = (borrowed_bound(&group.lower), borrowed_bound(&group.upper));
            let mut range = table
                .range::<&[u8]>(bounds)
                .map_err(|error| storage_error("scan durable secondary index group", error))?;
            let Some(entry) = range.next() else {
                self.upper = Bound::Excluded(group.before.clone());
                self.reverse_group = None;
                continue;
            };
            let (key, _) =
                entry.map_err(|error| storage_error("read durable index range", error))?;
            let key = key.value();
            if !hits.is_empty() && encoded_bytes.saturating_add(key.len()) > SOURCE_BATCH_MAX_BYTES
            {
                break;
            }
            let hit = decode_durable_index_hit(key, self.index_id, self.component_count)?;
            encoded_bytes = encoded_bytes.saturating_add(key.len());
            hits.push(hit);
            group.lower = Bound::Excluded(key.to_vec());
            if let Some(remaining) = &mut self.remaining {
                *remaining = remaining.saturating_sub(1);
            }
        }
        if hits.is_empty() {
            self.done = true;
            Ok(None)
        } else {
            Ok(Some(hits))
        }
    }
}

fn check_source_control(control: Option<&crate::control::ExecutionControl>) -> Result<()> {
    control.map_or(Ok(()), crate::control::ExecutionControl::checkpoint)
}

fn durable_index_prefix(index_id: u64, component_count: u8) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(15);
    prefix.extend_from_slice(INDEX_MAGIC);
    prefix.extend_from_slice(&PRODUCTION_INDEX_KEY_VERSION.to_be_bytes());
    prefix.extend_from_slice(&index_id.to_be_bytes());
    prefix.push(component_count);
    prefix
}

fn durable_index_bound(prefix: &[u8], bound: &Bound<Vec<u8>>, lower: bool) -> Bound<Vec<u8>> {
    match bound {
        Bound::Included(tuple) => {
            let mut key = prefix.to_vec();
            key.extend_from_slice(tuple);
            let row_id = if lower { 0_u64 } else { u64::MAX };
            key.extend_from_slice(&row_id.to_be_bytes());
            Bound::Included(key)
        }
        Bound::Excluded(tuple) => {
            let mut key = prefix.to_vec();
            key.extend_from_slice(tuple);
            let row_id = if lower { u64::MAX } else { 0_u64 };
            key.extend_from_slice(&row_id.to_be_bytes());
            Bound::Excluded(key)
        }
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn borrowed_bound(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(value) => Bound::Included(value.as_slice()),
        Bound::Excluded(value) => Bound::Excluded(value.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn durable_index_bounds(
    index_id: u64,
    component_count: u8,
    bounds: &EncodedIndexBounds,
) -> Result<EncodedIndexBounds> {
    let prefix = durable_index_prefix(index_id, component_count);
    let lower = match &bounds.0 {
        Bound::Unbounded => Bound::Included(prefix.clone()),
        bound => durable_index_bound(&prefix, bound, true),
    };
    let upper = match &bounds.1 {
        Bound::Unbounded => prefix_successor_bytes(&prefix)
            .map(Bound::Excluded)
            .unwrap_or(Bound::Unbounded),
        bound => durable_index_bound(&prefix, bound, false),
    };
    Ok((lower, upper))
}

fn prefix_successor_bytes(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut next = prefix.to_vec();
    while next.last() == Some(&u8::MAX) {
        next.pop();
    }
    let last = next.last_mut()?;
    *last = last.saturating_add(1);
    Some(next)
}

fn decode_durable_index_hit(key: &[u8], index_id: u64, component_count: u8) -> Result<IndexHit> {
    validate_index_key(key, PRODUCTION_INDEX_KEY_VERSION)?;
    if key.len() < 23
        || u64::from_be_bytes(key[6..14].try_into().unwrap()) != index_id
        || key[14] != component_count
    {
        return Err(Error::new(
            "E_STORAGE",
            "durable secondary index key does not match its catalog definition",
        ));
    }
    let row_start = key.len() - 8;
    Ok(IndexHit {
        boundary: key[15..row_start].to_vec(),
        row_id: u64::from_be_bytes(key[row_start..].try_into().unwrap()),
    })
}

#[derive(Debug, Clone)]
struct DurableHead {
    layout: StorageLayout,
    meta: DurableMeta,
    catalog_canonical: bool,
}

impl DurableHead {
    fn from_prepared(state: &PreparedState) -> Self {
        Self {
            layout: state.layout,
            meta: state.meta.clone(),
            catalog_canonical: state.catalog.values().all(|value| {
                value
                    .get(4..6)
                    .and_then(|version| version.try_into().ok())
                    .map(u16::from_be_bytes)
                    == Some(state.layout.catalog)
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StorageLayout {
    format: u32,
    catalog: u16,
    value: u16,
    index: u16,
    migration: u16,
    receipt: u16,
}

impl StorageLayout {
    const fn legacy(format: u32) -> Self {
        Self {
            format,
            catalog: CATALOG_CODEC_VERSION,
            value: VALUE_CODEC_VERSION,
            index: INDEX_KEY_VERSION,
            migration: MIGRATION_CODEC_VERSION,
            receipt: RECEIPT_CODEC_VERSION,
        }
    }

    const fn production() -> Self {
        Self {
            format: PRODUCTION_STORAGE_FORMAT_VERSION,
            catalog: PRODUCTION_CATALOG_CODEC_VERSION,
            value: PRODUCTION_VALUE_CODEC_VERSION,
            index: PRODUCTION_INDEX_KEY_VERSION,
            migration: MIGRATION_CODEC_VERSION,
            receipt: PRODUCTION_RECEIPT_CODEC_VERSION,
        }
    }

    const fn scalar() -> Self {
        Self {
            format: SCALAR_STORAGE_FORMAT_VERSION,
            catalog: SCALAR_CATALOG_CODEC_VERSION,
            value: PRODUCTION_VALUE_CODEC_VERSION,
            index: SCALAR_INDEX_KEY_VERSION,
            migration: MIGRATION_CODEC_VERSION,
            receipt: PRODUCTION_RECEIPT_CODEC_VERSION,
        }
    }

    const fn for_format(format: u32) -> Self {
        if format >= PRODUCTION_STORAGE_FORMAT_VERSION {
            Self::production()
        } else if format == SCALAR_STORAGE_FORMAT_VERSION {
            Self::scalar()
        } else {
            Self::legacy(format)
        }
    }

    const fn supports_production_scalars(self) -> bool {
        self.format >= SCALAR_STORAGE_FORMAT_VERSION
    }
}

pub(crate) enum CommitFailure {
    Definite(Error),
    Uncertain(Error),
}

#[derive(Debug, Clone, Copy, Default)]
struct TransactionProfile {
    apply_micros: u64,
    sync_micros: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct DeltaCounts {
    catalog_changes: usize,
    row_changes: usize,
    index_changes: usize,
    migration_changes: usize,
    receipt_changes: usize,
    encoded_change_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpgradeResult {
    pub(crate) previous_format: u32,
    pub(crate) format: u32,
    pub(crate) changed: bool,
}

impl CommitFailure {
    fn definite(context: &str, error: impl std::fmt::Display) -> Self {
        Self::Definite(storage_error(context, error))
    }

    fn uncertain(context: &str, error: impl std::fmt::Display) -> Self {
        Self::Uncertain(storage_error(context, error))
    }

    fn into_error(self) -> Error {
        match self {
            Self::Definite(error) | Self::Uncertain(error) => error,
        }
    }
}

impl RedbStore {
    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<RedbOpen> {
        let total_started = Instant::now();
        let path = resolve_path(path.into())?;
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::new("E_IO", format!("create database directory: {error}"))
            })?;
        }
        let fresh = std::fs::metadata(&path).map_or(true, |metadata| metadata.len() == 0);
        let redb_started = Instant::now();
        let database = RedbDatabase::create(&path).map_err(open_error)?;
        let redb_open_micros = elapsed_micros(redb_started);
        let empty = Database::default();
        let initial = PreparedState::new(&empty, &ReceiptMap::new(), StorageLayout::production())?;
        let mut store = Self {
            database,
            committed: DurableHead::from_prepared(&initial),
        };
        let bootstrap_started = Instant::now();
        if fresh {
            let mut bootstrap = PreparedDelta::between(&initial, &initial);
            bootstrap.expected_meta = None;
            store
                .commit_prepared(&bootstrap)
                .map_err(CommitFailure::into_error)?;
        }
        let bootstrap_micros = if fresh {
            elapsed_micros(bootstrap_started)
        } else {
            0
        };
        let format = store.current_layout()?.format;
        if format == PRODUCTION_STORAGE_FORMAT_VERSION {
            let (loaded, receipts, committed, source, mut profile) = store.load_bounded_view()?;
            store.committed = committed;
            profile.total_micros = elapsed_micros(total_started);
            profile.redb_open_micros = redb_open_micros;
            profile.bootstrap_micros = bootstrap_micros;
            profile.fresh = fresh;
            return Ok((store, loaded, receipts, source, profile));
        }
        let (loaded, receipts, committed, mut profile) = store.load()?;
        let needs_cursor_upgrade = committed.layout.format < CURSOR_STORAGE_FORMAT_VERSION;
        store.committed = DurableHead::from_prepared(&committed);
        if needs_cursor_upgrade {
            store.committed.layout = StorageLayout::legacy(CURSOR_STORAGE_FORMAT_VERSION);
            store
                .commit(&loaded, &loaded, &receipts)
                .map_err(CommitFailure::into_error)?;
        }
        profile.total_micros = elapsed_micros(total_started);
        profile.redb_open_micros = redb_open_micros;
        profile.bootstrap_micros = bootstrap_micros;
        profile.fresh = fresh;
        profile.cursor_upgrade = needs_cursor_upgrade;
        let loaded = Arc::new(loaded);
        let source: Arc<dyn TypedRowSource> = loaded.clone();
        Ok((store, loaded, receipts, source, profile))
    }

    fn current_layout(&self) -> Result<StorageLayout> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin redb read transaction", error))?;
        let table = transaction
            .open_table(META)
            .map_err(|error| storage_error("open meta table", error))?;
        read_meta(&table).map(|(_, layout)| layout)
    }

    pub(crate) fn supports_production_scalars(&self) -> bool {
        self.committed.layout.supports_production_scalars()
    }

    pub(crate) fn committed_view(
        &self,
        database: &Database,
    ) -> Result<(Arc<Database>, Arc<dyn TypedRowSource>)> {
        if self.committed.layout.format != PRODUCTION_STORAGE_FORMAT_VERSION {
            let database = Arc::new(database.clone());
            let source: Arc<dyn TypedRowSource> = database.clone();
            return Ok((database, source));
        }
        let metadata = Arc::new(database.metadata_only()?);
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin committed redb read view", error))?;
        let (meta, layout) = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open committed meta table", error))?;
            read_meta(&table)?
        };
        if layout != self.committed.layout || meta != database.durable_meta() {
            return Err(Error::new(
                "E_STORAGE_REOPEN_REQUIRED",
                "committed redb read view does not match the published database state",
            ));
        }
        let source: Arc<dyn TypedRowSource> =
            Arc::new(RedbReadSource::new(transaction, metadata.clone()));
        Ok((metadata, source))
    }

    pub(crate) fn versions(&self) -> StorageVersions {
        let layout = self.committed.layout;
        StorageVersions {
            format: layout.format,
            catalog_codec: layout.catalog,
            value_codec: layout.value,
            index_key_codec: layout.index,
            migration_codec: layout.migration,
            receipt_codec: layout.receipt,
            backup_codec: crate::backup::PRODUCTION_BACKUP_FORMAT_VERSION,
        }
    }

    pub(crate) fn upgrade(
        &mut self,
        database: &Database,
        receipts: &ReceiptMap,
        target: u32,
    ) -> std::result::Result<UpgradeResult, CommitFailure> {
        if !matches!(
            target,
            SCALAR_STORAGE_FORMAT_VERSION | PRODUCTION_STORAGE_FORMAT_VERSION
        ) {
            return Err(CommitFailure::Definite(Error::new(
                "E_STORAGE_UPGRADE",
                format!("unsupported storage upgrade target {target}"),
            )));
        }
        let previous = self.committed.layout;
        if previous.format == target {
            return Ok(UpgradeResult {
                previous_format: previous.format,
                format: previous.format,
                changed: false,
            });
        }
        if previous.format > target {
            return Err(CommitFailure::Definite(Error::new(
                "E_STORAGE_UPGRADE",
                "storage downgrades are not supported",
            )));
        }
        self.committed.layout = StorageLayout::for_format(target);
        if let Err(error) = self.commit(database, database, receipts) {
            self.committed.layout = previous;
            return Err(error);
        }
        Ok(UpgradeResult {
            previous_format: previous.format,
            format: target,
            changed: true,
        })
    }

    pub(crate) fn commit(
        &mut self,
        _previous: &Database,
        database: &Database,
        receipts: &ReceiptMap,
    ) -> std::result::Result<DurableCommitProfile, CommitFailure> {
        let total_started = Instant::now();
        let prepare_started = Instant::now();
        validate_receipts(receipts, database.sequence).map_err(CommitFailure::Definite)?;
        let layout = self.committed.layout;
        let reload_started = Instant::now();
        let (_, _, previous, _) = self.load().map_err(CommitFailure::Definite)?;
        let reload_previous_micros = elapsed_micros(reload_started);
        let encode_started = Instant::now();
        let next =
            PreparedState::new(database, receipts, layout).map_err(CommitFailure::Definite)?;
        let encode_next_micros = elapsed_micros(encode_started);
        let diff_started = Instant::now();
        let prepared = PreparedDelta::between(&previous, &next);
        let diff_micros = elapsed_micros(diff_started);
        let counts = prepared.counts();
        let prepare_micros = elapsed_micros(prepare_started);
        let transaction = self.commit_prepared(&prepared)?;
        self.committed = DurableHead::from_prepared(&next);
        Ok(DurableCommitProfile {
            mode: DurableCommitMode::FullRebuild,
            total_micros: elapsed_micros(total_started),
            prepare_micros,
            reload_previous_micros,
            encode_next_micros,
            diff_micros,
            transaction_apply_micros: transaction.apply_micros,
            sync_micros: transaction.sync_micros,
            catalog_changes: counts.catalog_changes,
            row_changes: counts.row_changes,
            index_changes: counts.index_changes,
            migration_changes: counts.migration_changes,
            receipt_changes: counts.receipt_changes,
            encoded_change_bytes: counts.encoded_change_bytes,
        })
    }

    pub(crate) fn commit_incremental(
        &mut self,
        previous: &Database,
        previous_receipts: &ReceiptMap,
        database: &Database,
        receipts: &ReceiptMap,
        write_set: &LogicalWriteSet,
    ) -> std::result::Result<DurableCommitProfile, CommitFailure> {
        if !self.committed.catalog_canonical {
            // Opening a legacy catalog can schedule an in-place codec
            // normalization for the next successful write. That maintenance
            // rewrite intentionally uses the full-state path once.
            return self.commit(previous, database, receipts);
        }
        let total_started = Instant::now();
        let prepare_started = Instant::now();
        validate_receipts(receipts, database.sequence).map_err(CommitFailure::Definite)?;
        let prepared = PreparedDelta::incremental(
            previous,
            previous_receipts,
            database,
            &self.committed,
            receipts,
            write_set,
        )
        .map_err(CommitFailure::Definite)?;
        let counts = prepared.counts();
        let prepare_micros = elapsed_micros(prepare_started);
        let transaction = self.commit_prepared(&prepared)?;
        self.committed.layout = prepared.layout;
        self.committed.meta = prepared.meta.clone();
        self.committed.catalog_canonical = true;
        Ok(DurableCommitProfile {
            mode: DurableCommitMode::Incremental,
            total_micros: elapsed_micros(total_started),
            prepare_micros,
            transaction_apply_micros: transaction.apply_micros,
            sync_micros: transaction.sync_micros,
            catalog_changes: counts.catalog_changes,
            row_changes: counts.row_changes,
            index_changes: counts.index_changes,
            migration_changes: counts.migration_changes,
            receipt_changes: counts.receipt_changes,
            encoded_change_bytes: counts.encoded_change_bytes,
            ..DurableCommitProfile::default()
        })
    }

    fn commit_prepared(
        &mut self,
        prepared: &PreparedDelta,
    ) -> std::result::Result<TransactionProfile, CommitFailure> {
        let apply_started = Instant::now();
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin redb write transaction", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure redb durability", error))?;
        transaction.set_two_phase_commit(true);
        {
            let mut table = transaction
                .open_table(META)
                .map_err(|error| CommitFailure::definite("open meta table", error))?;
            write_meta(
                &mut table,
                prepared.expected_meta.as_ref(),
                &prepared.meta,
                prepared.layout,
            )
            .map_err(CommitFailure::Definite)?;
        }
        {
            let mut table = transaction
                .open_table(CATALOG)
                .map_err(|error| CommitFailure::definite("open catalog table", error))?;
            apply_bytes_delta(&mut table, &prepared.catalog, "catalog entry")
                .map_err(CommitFailure::Definite)?;
        }
        {
            let mut table = transaction
                .open_table(ROWS)
                .map_err(|error| CommitFailure::definite("open rows table", error))?;
            apply_bytes_delta(&mut table, &prepared.rows, "row")
                .map_err(CommitFailure::Definite)?;
        }
        {
            let mut table = transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| CommitFailure::definite("open secondary index table", error))?;
            apply_set_delta(&mut table, &prepared.secondary_indexes)
                .map_err(CommitFailure::Definite)?;
        }
        {
            let mut table = transaction
                .open_table(MIGRATION_LEDGER)
                .map_err(|error| CommitFailure::definite("open migration ledger table", error))?;
            apply_ledger_delta(&mut table, &prepared.migrations)
                .map_err(CommitFailure::Definite)?;
        }
        {
            let mut table = transaction
                .open_table(IDEMPOTENCY_RECEIPTS)
                .map_err(|error| {
                    CommitFailure::definite("open idempotency receipt table", error)
                })?;
            apply_bytes_delta(&mut table, &prepared.receipts, "idempotency receipt")
                .map_err(CommitFailure::Definite)?;
        }
        let apply_micros = elapsed_micros(apply_started);
        let sync_started = Instant::now();
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit redb transaction", error))?;
        Ok(TransactionProfile {
            apply_micros,
            sync_micros: elapsed_micros(sync_started),
        })
    }

    pub(crate) fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap)> {
        let backend_clean = self
            .database
            .check_integrity()
            .map_err(|error| storage_error("check redb integrity", error))?;
        let (database, receipts, committed, _) = self.load()?;
        self.committed = DurableHead::from_prepared(&committed);
        Ok((backend_clean, database, receipts))
    }

    fn load_bounded_view(&self) -> Result<BoundedViewLoad> {
        let total_started = Instant::now();
        let mut profile = StorageOpenProfile {
            bounded_view: true,
            ..StorageOpenProfile::default()
        };
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin redb read transaction", error))?;
        let meta_started = Instant::now();
        let (meta, layout) = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open meta table", error))?;
            read_meta(&table)?
        };
        profile.meta_micros = elapsed_micros(meta_started);
        if layout != StorageLayout::production() {
            return Err(Error::new(
                "E_STORAGE",
                "bounded Legacy0 reads require the production format-5 layout",
            ));
        }

        let catalog_started = Instant::now();
        let (entries, catalog_canonical) = {
            let table = transaction
                .open_table(CATALOG)
                .map_err(|error| storage_error("open catalog table", error))?;
            let mut entries = Vec::new();
            let mut canonical = true;
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate catalog table", error))?
            {
                let (key, value) =
                    entry.map_err(|error| storage_error("read catalog entry", error))?;
                let key = key.value();
                let value = value.value();
                entries.push(decode_catalog_entry(key, value, layout.catalog)?);
                canonical &= value
                    .get(4..6)
                    .and_then(|version| version.try_into().ok())
                    .map(u16::from_be_bytes)
                    == Some(layout.catalog);
            }
            (entries, canonical)
        };
        profile.catalog_micros = elapsed_micros(catalog_started);
        profile.catalog_entries = entries.len();

        // Opening the physical tables proves that the expected Legacy0 table
        // definitions exist without traversing their entries.
        let rows_started = Instant::now();
        {
            transaction
                .open_table(ROWS)
                .map_err(|error| storage_error("open rows table", error))?;
        }
        profile.rows_micros = elapsed_micros(rows_started);
        let indexes_started = Instant::now();
        {
            transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| storage_error("open secondary index table", error))?;
        }
        profile.indexes_micros = elapsed_micros(indexes_started);

        let migrations_started = Instant::now();
        let migrations = {
            let table = transaction
                .open_table(MIGRATION_LEDGER)
                .map_err(|error| storage_error("open migration ledger table", error))?;
            let mut migrations = Vec::new();
            for (expected, entry) in table
                .iter()
                .map_err(|error| storage_error("iterate migration ledger", error))?
                .enumerate()
            {
                let (sequence, value) =
                    entry.map_err(|error| storage_error("read migration ledger entry", error))?;
                if sequence.value() != expected as u64 {
                    return Err(Error::new(
                        "E_STORAGE",
                        "migration ledger sequence is not contiguous",
                    ));
                }
                migrations.push(decode_migration_entry(value.value())?);
            }
            migrations
        };
        profile.migrations_micros = elapsed_micros(migrations_started);
        profile.migration_entries = migrations.len();

        let receipts_started = Instant::now();
        let receipts = if transaction
            .list_tables()
            .map_err(|error| storage_error("list redb tables", error))?
            .any(|table| table.name() == IDEMPOTENCY_RECEIPTS.name())
        {
            let table = transaction
                .open_table(IDEMPOTENCY_RECEIPTS)
                .map_err(|error| storage_error("open idempotency receipt table", error))?;
            let mut receipts = ReceiptMap::new();
            let mut stored_bytes = 0_usize;
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate idempotency receipts", error))?
            {
                let (key, value) =
                    entry.map_err(|error| storage_error("read idempotency receipt", error))?;
                let key = String::from_utf8(key.value().to_vec()).map_err(|error| {
                    Error::new(
                        "E_STORAGE",
                        format!("idempotency key is not UTF-8: {error}"),
                    )
                })?;
                let value = value.value();
                if value.len() > MAX_IDEMPOTENCY_RECEIPT_BYTES {
                    return Err(Error::new(
                        "E_STORAGE",
                        "stored idempotency receipt exceeds the supported encoded size",
                    ));
                }
                stored_bytes = stored_bytes
                    .checked_add(value.len())
                    .ok_or_else(|| Error::new("E_STORAGE", "idempotency receipt size overflow"))?;
                if stored_bytes > MAX_IDEMPOTENCY_TOTAL_BYTES {
                    return Err(Error::new(
                        "E_STORAGE",
                        "stored idempotency receipt total exceeds the supported limit",
                    ));
                }
                if receipts
                    .insert(key, decode_receipt(value, layout.receipt)?)
                    .is_some()
                {
                    return Err(Error::new("E_STORAGE", "duplicate idempotency receipt key"));
                }
            }
            profile.receipt_bytes = u64::try_from(stored_bytes).unwrap_or(u64::MAX);
            receipts
        } else {
            ReceiptMap::new()
        };
        profile.receipts_micros = elapsed_micros(receipts_started);
        profile.receipt_entries = receipts.len();

        let construct_started = Instant::now();
        let metadata = Arc::new(Database::from_durable(
            meta.clone(),
            entries,
            Vec::new(),
            migrations,
        )?);
        profile.database_construct_micros = elapsed_micros(construct_started);
        let validation_started = Instant::now();
        validate_receipts(&receipts, metadata.sequence)?;
        profile.validation_micros = elapsed_micros(validation_started);
        profile.total_micros = elapsed_micros(total_started);
        let committed = DurableHead {
            layout,
            meta,
            catalog_canonical,
        };
        let source: Arc<dyn TypedRowSource> =
            Arc::new(RedbReadSource::new(transaction, metadata.clone()));
        Ok((metadata, receipts, committed, source, profile))
    }

    fn load(&self) -> Result<(Database, ReceiptMap, PreparedState, StorageOpenProfile)> {
        let total_started = Instant::now();
        let mut profile = StorageOpenProfile::default();
        let meta_started = Instant::now();
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin redb read transaction", error))?;
        let (meta, layout) = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open meta table", error))?;
            read_meta(&table)?
        };
        profile.meta_micros = elapsed_micros(meta_started);
        let catalog_started = Instant::now();
        let (entries, stored_catalog) = {
            let table = transaction
                .open_table(CATALOG)
                .map_err(|error| storage_error("open catalog table", error))?;
            let mut entries = Vec::new();
            let mut stored = BTreeMap::new();
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate catalog table", error))?
            {
                let (key, value) =
                    entry.map_err(|error| storage_error("read catalog entry", error))?;
                let key = key.value().to_vec();
                let value = value.value().to_vec();
                entries.push(decode_catalog_entry(&key, &value, layout.catalog)?);
                stored.insert(key, value);
            }
            (entries, stored)
        };
        profile.catalog_micros = elapsed_micros(catalog_started);
        profile.catalog_entries = stored_catalog.len();
        let rows_started = Instant::now();
        let (rows, stored_rows) = {
            let table = transaction
                .open_table(ROWS)
                .map_err(|error| storage_error("open rows table", error))?;
            let mut rows = Vec::new();
            let mut stored = BTreeMap::new();
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate rows table", error))?
            {
                let (key, value) = entry.map_err(|error| storage_error("read row", error))?;
                let key = key.value().to_vec();
                let value = value.value().to_vec();
                let (table_id, row_id) = decode_row_key(&key)?;
                rows.push((table_id, row_id, value.clone()));
                stored.insert(key, value);
            }
            (rows, stored)
        };
        profile.rows_micros = elapsed_micros(rows_started);
        profile.row_entries = stored_rows.len();
        profile.row_bytes = map_bytes(&stored_rows);
        let indexes_started = Instant::now();
        let stored_indexes = {
            let table = transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| storage_error("open secondary index table", error))?;
            let mut keys = BTreeSet::new();
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate secondary index table", error))?
            {
                let (key, _) =
                    entry.map_err(|error| storage_error("read secondary index entry", error))?;
                validate_index_key(key.value(), layout.index)?;
                keys.insert(key.value().to_vec());
            }
            keys
        };
        profile.indexes_micros = elapsed_micros(indexes_started);
        profile.index_entries = stored_indexes.len();
        profile.index_key_bytes = set_bytes(&stored_indexes);
        let migrations_started = Instant::now();
        let (migrations, stored_migrations) = {
            let table = transaction
                .open_table(MIGRATION_LEDGER)
                .map_err(|error| storage_error("open migration ledger table", error))?;
            let mut migrations = Vec::new();
            let mut stored = BTreeMap::new();
            for (expected, entry) in table
                .iter()
                .map_err(|error| storage_error("iterate migration ledger", error))?
                .enumerate()
            {
                let (sequence, value) =
                    entry.map_err(|error| storage_error("read migration ledger entry", error))?;
                let sequence = sequence.value();
                if sequence != expected as u64 {
                    return Err(Error::new(
                        "E_STORAGE",
                        "migration ledger sequence is not contiguous",
                    ));
                }
                let value = value.value().to_vec();
                migrations.push(decode_migration_entry(&value)?);
                stored.insert(sequence, value);
            }
            (migrations, stored)
        };
        profile.migrations_micros = elapsed_micros(migrations_started);
        profile.migration_entries = stored_migrations.len();
        let receipts_started = Instant::now();
        let (receipts, stored_receipts) = if transaction
            .list_tables()
            .map_err(|error| storage_error("list redb tables", error))?
            .any(|table| table.name() == IDEMPOTENCY_RECEIPTS.name())
        {
            let table = transaction
                .open_table(IDEMPOTENCY_RECEIPTS)
                .map_err(|error| storage_error("open idempotency receipt table", error))?;
            let mut receipts = ReceiptMap::new();
            let mut stored = BTreeMap::new();
            let mut stored_bytes = 0usize;
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate idempotency receipts", error))?
            {
                let (key, value) =
                    entry.map_err(|error| storage_error("read idempotency receipt", error))?;
                let key_bytes = key.value().to_vec();
                let key = String::from_utf8(key_bytes.clone()).map_err(|error| {
                    Error::new(
                        "E_STORAGE",
                        format!("idempotency key is not UTF-8: {error}"),
                    )
                })?;
                let value = value.value().to_vec();
                if value.len() > MAX_IDEMPOTENCY_RECEIPT_BYTES {
                    return Err(Error::new(
                        "E_STORAGE",
                        "stored idempotency receipt exceeds the supported encoded size",
                    ));
                }
                stored_bytes = stored_bytes
                    .checked_add(value.len())
                    .ok_or_else(|| Error::new("E_STORAGE", "idempotency receipt size overflow"))?;
                if stored_bytes > MAX_IDEMPOTENCY_TOTAL_BYTES {
                    return Err(Error::new(
                        "E_STORAGE",
                        "stored idempotency receipt total exceeds the supported limit",
                    ));
                }
                let receipt = decode_receipt(&value, layout.receipt)?;
                if receipts.insert(key, receipt).is_some() {
                    return Err(Error::new("E_STORAGE", "duplicate idempotency receipt key"));
                }
                stored.insert(key_bytes, value);
            }
            (receipts, stored)
        } else {
            (ReceiptMap::new(), BTreeMap::new())
        };
        profile.receipts_micros = elapsed_micros(receipts_started);
        profile.receipt_entries = stored_receipts.len();
        profile.receipt_bytes = map_bytes(&stored_receipts);
        let construct_started = Instant::now();
        let database = Database::from_durable(meta.clone(), entries, rows, migrations)?;
        profile.database_construct_micros = elapsed_micros(construct_started);
        let validation_started = Instant::now();
        if !layout.supports_production_scalars() {
            database.ensure_legacy_scalars()?;
            ensure_legacy_receipts(&receipts)?;
        }
        validate_receipts(&receipts, database.sequence)?;
        if layout.format == LEGACY_STORAGE_FORMAT_VERSION && !receipts.is_empty() {
            return Err(Error::new(
                "E_STORAGE",
                "storage format 1 must not contain idempotency receipts",
            ));
        }
        let expected_indexes = database
            .durable_secondary_indexes()?
            .into_iter()
            .map(|(index_id, value, row_id)| {
                encode_index_key(&database, index_id, &value, row_id, layout.index)
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if stored_indexes != expected_indexes {
            return Err(Error::new(
                "E_STORAGE",
                "durable secondary indexes do not match the stored rows and catalog",
            ));
        }
        profile.validation_micros = elapsed_micros(validation_started);
        let committed_layout = StorageLayout {
            catalog: layout.catalog.max(CATALOG_CODEC_VERSION),
            ..layout
        };
        let committed = PreparedState {
            layout: committed_layout,
            meta,
            catalog: stored_catalog,
            rows: stored_rows,
            secondary_indexes: stored_indexes,
            migrations: stored_migrations,
            receipts: stored_receipts,
        };
        profile.total_micros = elapsed_micros(total_started);
        Ok((database, receipts, committed, profile))
    }
}

struct PreparedState {
    layout: StorageLayout,
    meta: DurableMeta,
    catalog: BTreeMap<Vec<u8>, Vec<u8>>,
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    secondary_indexes: BTreeSet<Vec<u8>>,
    migrations: BTreeMap<u64, Vec<u8>>,
    receipts: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl PreparedState {
    fn new(database: &Database, receipts: &ReceiptMap, layout: StorageLayout) -> Result<Self> {
        if !layout.supports_production_scalars() {
            database.ensure_legacy_scalars()?;
            ensure_legacy_receipts(receipts)?;
        }
        let mut catalog = BTreeMap::new();
        for entry in database.durable_catalog_entries() {
            catalog.insert(
                encode_catalog_key(entry.kind_tag(), entry.stable_id()),
                encode_catalog_entry(&entry, layout.catalog)?,
            );
        }
        let rows = database
            .durable_rows_with_codec(layout.value)?
            .into_iter()
            .map(|(table_id, row_id, value)| (encode_row_key(table_id, row_id), value))
            .collect::<BTreeMap<_, _>>();
        let secondary_indexes = database
            .durable_secondary_indexes()?
            .into_iter()
            .map(|(index_id, value, row_id)| {
                encode_index_key(database, index_id, &value, row_id, layout.index)
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let migrations = database
            .durable_migrations()
            .iter()
            .enumerate()
            .map(|(sequence, entry)| {
                Ok((
                    u64::try_from(sequence).map_err(|_| {
                        Error::new("E_LIMIT", "migration ledger sequence exhausted")
                    })?,
                    encode_migration_entry(entry)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let receipts = receipts
            .iter()
            .map(|(key, receipt)| {
                Ok((
                    key.as_bytes().to_vec(),
                    encode_receipt(receipt, layout.receipt)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            layout,
            meta: database.durable_meta(),
            catalog,
            rows,
            secondary_indexes,
            migrations,
            receipts,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedDelta {
    layout: StorageLayout,
    expected_meta: Option<(DurableMeta, StorageLayout)>,
    meta: DurableMeta,
    catalog: BytesDelta,
    rows: BytesDelta,
    secondary_indexes: SetDelta,
    migrations: LedgerDelta,
    receipts: BytesDelta,
}

impl PreparedDelta {
    #[cfg(test)]
    fn new(previous: &Database, database: &Database) -> Result<Self> {
        let previous = PreparedState::new(previous, &ReceiptMap::new(), StorageLayout::legacy(1))?;
        let next = PreparedState::new(database, &ReceiptMap::new(), StorageLayout::legacy(1))?;
        Ok(Self::between(&previous, &next))
    }

    fn incremental(
        previous: &Database,
        previous_receipts: &ReceiptMap,
        database: &Database,
        committed: &DurableHead,
        receipts: &ReceiptMap,
        write_set: &LogicalWriteSet,
    ) -> Result<Self> {
        let layout = committed.layout;
        if !layout.supports_production_scalars() {
            database.ensure_legacy_scalars()?;
            ensure_legacy_receipts(receipts)?;
        }
        if committed.meta != previous.durable_meta() {
            return Err(Error::new(
                "E_STORAGE",
                "cached durable head does not match the incremental base state",
            ));
        }

        let mut catalog = BytesDelta::default();
        for (table, (expected_before, expected_after)) in &write_set.table_watermarks {
            let before = previous.durable_table_catalog_entry(table)?;
            let after = database.durable_table_catalog_entry(table)?;
            let (DurableCatalogEntry::Table(before_table), DurableCatalogEntry::Table(after_table)) =
                (&before, &after)
            else {
                unreachable!("table catalog lookup returns a table entry")
            };
            if before_table.id != after_table.id
                || before_table.next_row_id != *expected_before
                || after_table.next_row_id != *expected_after
            {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("table '{table}' watermark does not match its incremental write set"),
                ));
            }
            let key = encode_catalog_key(after.kind_tag(), after.stable_id());
            let expected = encode_catalog_entry(&before, layout.catalog)?;
            let value = encode_catalog_entry(&after, layout.catalog)?;
            if expected != value {
                catalog.writes.push(BytesWrite {
                    key,
                    expected: Some(expected),
                    value,
                });
            }
        }

        let mut rows = BytesDelta::default();
        for ((table, row_id), change) in &write_set.rows {
            let before = change
                .before
                .as_ref()
                .map(|row| {
                    previous
                        .durable_row_with_codec(table, row, layout.value)
                        .map(|(table_id, bytes)| (table_id, row.id, bytes))
                })
                .transpose()?;
            let after = change
                .after
                .as_ref()
                .map(|row| {
                    database
                        .durable_row_with_codec(table, row, layout.value)
                        .map(|(table_id, bytes)| (table_id, row.id, bytes))
                })
                .transpose()?;
            let table_id = before
                .as_ref()
                .map(|entry| entry.0)
                .or_else(|| after.as_ref().map(|entry| entry.0))
                .expect("coalesced row changes retain one image");
            if before
                .as_ref()
                .is_some_and(|entry| entry.0 != table_id || entry.1 != *row_id)
                || after
                    .as_ref()
                    .is_some_and(|entry| entry.0 != table_id || entry.1 != *row_id)
            {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("row '{table}.{row_id}' does not match its incremental key"),
                ));
            }
            let key = encode_row_key(table_id, *row_id);
            match (before.as_ref(), after.as_ref()) {
                (Some(before), Some(after)) if before.2 == after.2 => {}
                (Some(before), Some(after)) => rows.writes.push(BytesWrite {
                    key,
                    expected: Some(before.2.clone()),
                    value: after.2.clone(),
                }),
                (None, Some(after)) => rows.writes.push(BytesWrite {
                    key,
                    expected: None,
                    value: after.2.clone(),
                }),
                (Some(before), None) => rows.deletes.push(BytesDelete {
                    key,
                    expected: before.2.clone(),
                }),
                (None, None) => unreachable!("empty row changes are removed during coalescing"),
            }
        }

        let mut secondary_deletes = Vec::new();
        let mut secondary_inserts = Vec::new();
        for ((index_id, _, row_id), change) in &write_set.index_entries {
            if change.index_id != *index_id || change.row_id != *row_id {
                return Err(Error::new(
                    "E_STORAGE",
                    "index entry does not match its incremental write-set key",
                ));
            }
            let key = encode_index_key(database, *index_id, &change.value, *row_id, layout.index)?;
            match (change.before, change.after) {
                (true, false) => secondary_deletes.push(key),
                (false, true) => secondary_inserts.push(key),
                _ => {
                    return Err(Error::new(
                        "E_STORAGE",
                        "unchanged index entry remained in incremental write set",
                    ));
                }
            }
        }

        let mut receipt_delta = BytesDelta::default();
        for key in &write_set.receipt_keys {
            let before = previous_receipts
                .get(key)
                .map(|receipt| encode_receipt(receipt, layout.receipt))
                .transpose()?;
            let after = receipts
                .get(key)
                .map(|receipt| encode_receipt(receipt, layout.receipt))
                .transpose()?;
            match (before, after) {
                (Some(expected), Some(value)) if expected == value => {}
                (Some(expected), Some(value)) => receipt_delta.writes.push(BytesWrite {
                    key: key.as_bytes().to_vec(),
                    expected: Some(expected),
                    value,
                }),
                (None, Some(value)) => receipt_delta.writes.push(BytesWrite {
                    key: key.as_bytes().to_vec(),
                    expected: None,
                    value,
                }),
                (Some(expected), None) => receipt_delta.deletes.push(BytesDelete {
                    key: key.as_bytes().to_vec(),
                    expected,
                }),
                (None, None) => {}
            }
        }

        Ok(Self {
            layout,
            expected_meta: Some((committed.meta.clone(), committed.layout)),
            meta: database.durable_meta(),
            catalog,
            rows,
            secondary_indexes: SetDelta {
                deletes: secondary_deletes,
                inserts: secondary_inserts,
            },
            migrations: LedgerDelta::default(),
            receipts: receipt_delta,
        })
    }

    fn between(previous: &PreparedState, next: &PreparedState) -> Self {
        Self {
            layout: next.layout,
            expected_meta: None,
            meta: next.meta.clone(),
            catalog: BytesDelta::between(&previous.catalog, &next.catalog),
            rows: BytesDelta::between(&previous.rows, &next.rows),
            secondary_indexes: SetDelta::between(
                &previous.secondary_indexes,
                &next.secondary_indexes,
            ),
            migrations: LedgerDelta::between(&previous.migrations, &next.migrations),
            receipts: BytesDelta::between(&previous.receipts, &next.receipts),
        }
    }

    fn counts(&self) -> DeltaCounts {
        DeltaCounts {
            catalog_changes: self.catalog.change_count(),
            row_changes: self.rows.change_count(),
            index_changes: self.secondary_indexes.change_count(),
            migration_changes: self.migrations.change_count(),
            receipt_changes: self.receipts.change_count(),
            encoded_change_bytes: self
                .catalog
                .encoded_bytes()
                .saturating_add(self.rows.encoded_bytes())
                .saturating_add(self.secondary_indexes.encoded_bytes())
                .saturating_add(self.migrations.encoded_bytes())
                .saturating_add(self.receipts.encoded_bytes()),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct LedgerDelta {
    deletes: Vec<(u64, Vec<u8>)>,
    writes: Vec<(u64, Option<Vec<u8>>, Vec<u8>)>,
}

impl LedgerDelta {
    fn between(previous: &BTreeMap<u64, Vec<u8>>, next: &BTreeMap<u64, Vec<u8>>) -> Self {
        let deletes = previous
            .iter()
            .filter(|(key, _)| !next.contains_key(*key))
            .map(|(key, value)| (*key, value.clone()))
            .collect();
        let writes = next
            .iter()
            .filter_map(|(key, value)| match previous.get(key) {
                Some(previous) if previous == value => None,
                previous => Some((*key, previous.cloned(), value.clone())),
            })
            .collect();
        Self { deletes, writes }
    }

    fn change_count(&self) -> usize {
        self.deletes.len().saturating_add(self.writes.len())
    }

    fn encoded_bytes(&self) -> u64 {
        self.deletes
            .iter()
            .map(|(_, value)| usize_u64(value.len()).saturating_add(8))
            .chain(self.writes.iter().map(|(_, expected, value)| {
                expected
                    .as_ref()
                    .map_or(0, |bytes| usize_u64(bytes.len()))
                    .saturating_add(usize_u64(value.len()))
                    .saturating_add(8)
            }))
            .fold(0, u64::saturating_add)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BytesDelta {
    deletes: Vec<BytesDelete>,
    writes: Vec<BytesWrite>,
}

#[derive(Debug, PartialEq, Eq)]
struct BytesDelete {
    key: Vec<u8>,
    expected: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
struct BytesWrite {
    key: Vec<u8>,
    expected: Option<Vec<u8>>,
    value: Vec<u8>,
}

impl BytesDelta {
    fn change_count(&self) -> usize {
        self.deletes.len().saturating_add(self.writes.len())
    }

    fn encoded_bytes(&self) -> u64 {
        self.deletes
            .iter()
            .map(|entry| usize_u64(entry.key.len()).saturating_add(usize_u64(entry.expected.len())))
            .chain(self.writes.iter().map(|entry| {
                usize_u64(entry.key.len())
                    .saturating_add(
                        entry
                            .expected
                            .as_ref()
                            .map_or(0, |bytes| usize_u64(bytes.len())),
                    )
                    .saturating_add(usize_u64(entry.value.len()))
            }))
            .fold(0, u64::saturating_add)
    }
}

impl BytesDelta {
    fn between(previous: &BTreeMap<Vec<u8>, Vec<u8>>, next: &BTreeMap<Vec<u8>, Vec<u8>>) -> Self {
        let deletes = previous
            .iter()
            .filter(|(key, _)| !next.contains_key(*key))
            .map(|(key, value)| BytesDelete {
                key: key.clone(),
                expected: value.clone(),
            })
            .collect();
        let writes = next
            .iter()
            .filter_map(|(key, value)| match previous.get(key) {
                Some(previous) if previous == value => None,
                previous => Some(BytesWrite {
                    key: key.clone(),
                    expected: previous.cloned(),
                    value: value.clone(),
                }),
            })
            .collect();
        Self { deletes, writes }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SetDelta {
    deletes: Vec<Vec<u8>>,
    inserts: Vec<Vec<u8>>,
}

impl SetDelta {
    fn between(previous: &BTreeSet<Vec<u8>>, next: &BTreeSet<Vec<u8>>) -> Self {
        Self {
            deletes: previous.difference(next).cloned().collect(),
            inserts: next.difference(previous).cloned().collect(),
        }
    }

    fn change_count(&self) -> usize {
        self.deletes.len().saturating_add(self.inserts.len())
    }

    fn encoded_bytes(&self) -> u64 {
        self.deletes
            .iter()
            .chain(&self.inserts)
            .map(|key| usize_u64(key.len()))
            .fold(0, u64::saturating_add)
    }
}

fn apply_bytes_delta(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    delta: &BytesDelta,
    entry_name: &str,
) -> Result<()> {
    for deleted in &delta.deletes {
        let removed = table
            .remove(deleted.key.as_slice())
            .map_err(|error| storage_error(&format!("delete {entry_name}"), error))?;
        if removed.as_ref().map(|value| value.value()) != Some(deleted.expected.as_slice()) {
            return Err(Error::new(
                "E_STORAGE",
                format!("stored {entry_name} changed before incremental delete"),
            ));
        }
    }
    for write in &delta.writes {
        let replaced = table
            .insert(write.key.as_slice(), write.value.as_slice())
            .map_err(|error| storage_error(&format!("write {entry_name}"), error))?;
        if replaced.as_ref().map(|value| value.value().to_vec()) != write.expected {
            return Err(Error::new(
                "E_STORAGE",
                format!("stored {entry_name} changed before incremental write"),
            ));
        }
    }
    Ok(())
}

fn apply_ledger_delta(table: &mut redb::Table<'_, u64, &[u8]>, delta: &LedgerDelta) -> Result<()> {
    for (key, expected) in &delta.deletes {
        let removed = table
            .remove(*key)
            .map_err(|error| storage_error("delete migration ledger entry", error))?;
        if removed.as_ref().map(|value| value.value()) != Some(expected.as_slice()) {
            return Err(Error::new(
                "E_STORAGE",
                "stored migration ledger changed before incremental delete",
            ));
        }
    }
    for (key, expected, value) in &delta.writes {
        let replaced = table
            .insert(*key, value.as_slice())
            .map_err(|error| storage_error("write migration ledger entry", error))?;
        if replaced.as_ref().map(|value| value.value().to_vec()) != *expected {
            return Err(Error::new(
                "E_STORAGE",
                "stored migration ledger changed before incremental write",
            ));
        }
    }
    Ok(())
}

fn apply_set_delta(table: &mut redb::Table<'_, &[u8], u8>, delta: &SetDelta) -> Result<()> {
    for key in &delta.deletes {
        let removed = table
            .remove(key.as_slice())
            .map_err(|error| storage_error("delete secondary index entry", error))?;
        if removed.is_none() {
            return Err(Error::new(
                "E_STORAGE",
                "secondary index entry disappeared before incremental delete",
            ));
        }
    }
    for key in &delta.inserts {
        let replaced = table
            .insert(key.as_slice(), 0)
            .map_err(|error| storage_error("write secondary index entry", error))?;
        if replaced.is_some() {
            return Err(Error::new(
                "E_STORAGE",
                "secondary index entry appeared before incremental insert",
            ));
        }
    }
    Ok(())
}

fn write_meta(
    table: &mut redb::Table<'_, &str, &[u8]>,
    expected: Option<&(DurableMeta, StorageLayout)>,
    meta: &DurableMeta,
    layout: StorageLayout,
) -> Result<()> {
    if let Some((expected_meta, expected_layout)) = expected {
        for (key, expected_value) in meta_entries(expected_meta, *expected_layout) {
            let actual = table
                .get(key)
                .map_err(|error| storage_error("read expected meta entry", error))?;
            if actual.as_ref().map(|value| value.value()) != Some(expected_value.as_slice()) {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("stored meta entry '{key}' changed before incremental commit"),
                ));
            }
        }
    }
    for (key, value) in meta_entries(meta, layout) {
        table
            .insert(key, value.as_slice())
            .map_err(|error| storage_error("write meta entry", error))?;
    }
    Ok(())
}

fn meta_entries(meta: &DurableMeta, layout: StorageLayout) -> [(&'static str, Vec<u8>); 12] {
    [
        (FORMAT_KEY, layout.format.to_be_bytes().to_vec()),
        (CATALOG_CODEC_KEY, layout.catalog.to_be_bytes().to_vec()),
        (VALUE_CODEC_KEY, layout.value.to_be_bytes().to_vec()),
        (INDEX_KEY_CODEC_KEY, layout.index.to_be_bytes().to_vec()),
        (MIGRATION_CODEC_KEY, layout.migration.to_be_bytes().to_vec()),
        (RECEIPT_CODEC_KEY, layout.receipt.to_be_bytes().to_vec()),
        (SEQUENCE_KEY, meta.sequence.to_be_bytes().to_vec()),
        (
            SCHEMA_REVISION_KEY,
            meta.schema_revision.to_be_bytes().to_vec(),
        ),
        (
            NEXT_CATALOG_ID_KEY,
            meta.next_catalog_id.to_be_bytes().to_vec(),
        ),
        (SCHEMA_HASH_KEY, meta.schema_hash.as_bytes().to_vec()),
        (
            CURSOR_INSTANCE_ID_KEY,
            meta.cursor_instance_id.as_slice().to_vec(),
        ),
        (CURSOR_SECRET_KEY, meta.cursor_secret.as_slice().to_vec()),
    ]
}

fn read_meta(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<(DurableMeta, StorageLayout)> {
    let format_version = u32::from_be_bytes(read_fixed::<4>(table, FORMAT_KEY)?);
    if !matches!(
        format_version,
        LEGACY_STORAGE_FORMAT_VERSION
            | RECEIPT_STORAGE_FORMAT_VERSION
            | CURSOR_STORAGE_FORMAT_VERSION
            | SCALAR_STORAGE_FORMAT_VERSION
            | PRODUCTION_STORAGE_FORMAT_VERSION
    ) {
        return Err(Error::new("E_STORAGE", format!("unsupported {FORMAT_KEY}")));
    }
    let catalog_version = u16::from_be_bytes(read_fixed::<2>(table, CATALOG_CODEC_KEY)?);
    let expected = StorageLayout::for_format(format_version);
    if !matches!(
        catalog_version,
        LEGACY_CATALOG_CODEC_VERSION
            | CATALOG_CODEC_VERSION
            | SCALAR_CATALOG_CODEC_VERSION
            | PRODUCTION_CATALOG_CODEC_VERSION
    ) || (format_version >= SCALAR_STORAGE_FORMAT_VERSION && catalog_version != expected.catalog)
    {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {CATALOG_CODEC_KEY}"),
        ));
    }
    let value = u16::from_be_bytes(read_fixed::<2>(table, VALUE_CODEC_KEY)?);
    let index = u16::from_be_bytes(read_fixed::<2>(table, INDEX_KEY_CODEC_KEY)?);
    if value != expected.value {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {VALUE_CODEC_KEY}"),
        ));
    }
    if index != expected.index {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {INDEX_KEY_CODEC_KEY}"),
        ));
    }
    let migration =
        read_optional_version(table, MIGRATION_CODEC_KEY)?.unwrap_or(MIGRATION_CODEC_VERSION);
    let receipt = read_optional_version(table, RECEIPT_CODEC_KEY)?.unwrap_or(RECEIPT_CODEC_VERSION);
    if migration != expected.migration {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {MIGRATION_CODEC_KEY}"),
        ));
    }
    if receipt != expected.receipt {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {RECEIPT_CODEC_KEY}"),
        ));
    }
    let layout = StorageLayout {
        format: format_version,
        catalog: catalog_version,
        value,
        index,
        migration,
        receipt,
    };
    let sequence = u64::from_be_bytes(read_fixed::<8>(table, SEQUENCE_KEY)?);
    let schema_revision = u64::from_be_bytes(read_fixed::<8>(table, SCHEMA_REVISION_KEY)?);
    let next_catalog_id = u64::from_be_bytes(read_fixed::<8>(table, NEXT_CATALOG_ID_KEY)?);
    let hash = read_bytes(table, SCHEMA_HASH_KEY)?;
    let schema_hash = String::from_utf8(hash)
        .map_err(|error| Error::new("E_STORAGE", format!("schema hash is not UTF-8: {error}")))?;
    let identity = if format_version >= CURSOR_STORAGE_FORMAT_VERSION {
        crate::pagination::CursorIdentity::from_bytes(
            read_fixed::<16>(table, CURSOR_INSTANCE_ID_KEY)?,
            read_fixed::<32>(table, CURSOR_SECRET_KEY)?,
        )
    } else {
        crate::pagination::CursorIdentity::generate()?
    };
    Ok((
        DurableMeta {
            sequence,
            schema_revision,
            next_catalog_id,
            schema_hash,
            cursor_instance_id: *identity.instance_id(),
            cursor_secret: *identity.secret(),
        },
        layout,
    ))
}

fn read_optional_version(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<u16>> {
    table
        .get(key)
        .map_err(|error| storage_error("read meta entry", error))?
        .map(|value| {
            value
                .value()
                .try_into()
                .map(u16::from_be_bytes)
                .map_err(|_| {
                    Error::new(
                        "E_STORAGE",
                        format!("invalid byte length for meta key '{key}'"),
                    )
                })
        })
        .transpose()
}

fn encode_receipt(receipt: &IdempotencyReceipt, version: u16) -> Result<Vec<u8>> {
    let mut value = Vec::from(RECEIPT_MAGIC.as_slice());
    value.extend_from_slice(&version.to_be_bytes());
    value.extend(encoded_receipt(receipt)?);
    Ok(value)
}

fn decode_receipt(value: &[u8], expected_version: u16) -> Result<IdempotencyReceipt> {
    if value.len() < 6 || &value[..4] != RECEIPT_MAGIC {
        return Err(Error::new(
            "E_STORAGE",
            "invalid idempotency receipt codec magic",
        ));
    }
    let version = u16::from_be_bytes(value[4..6].try_into().unwrap());
    if version != expected_version {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported idempotency receipt codec version {version}"),
        ));
    }
    serde_json::from_slice(&value[6..])
        .map_err(|error| Error::new("E_STORAGE", format!("decode idempotency receipt: {error}")))
}

fn read_fixed<const N: usize>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<[u8; N]> {
    read_bytes(table, key)?.try_into().map_err(|_| {
        Error::new(
            "E_STORAGE",
            format!("invalid byte length for meta key '{key}'"),
        )
    })
}

fn read_bytes(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Vec<u8>> {
    table
        .get(key)
        .map_err(|error| storage_error("read meta entry", error))?
        .map(|value| value.value().to_vec())
        .ok_or_else(|| Error::new("E_STORAGE", format!("missing meta key '{key}'")))
}

fn encode_catalog_key(kind: u8, stable_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.push(kind);
    key.extend_from_slice(&stable_id.to_be_bytes());
    key
}

fn encode_migration_entry(entry: &MigrationEntry) -> Result<Vec<u8>> {
    let mut value = Vec::from(MIGRATION_MAGIC.as_slice());
    value.extend_from_slice(&MIGRATION_CODEC_VERSION.to_be_bytes());
    value
        .extend(serde_json::to_vec(entry).map_err(|error| {
            Error::new("E_STORAGE", format!("encode migration entry: {error}"))
        })?);
    Ok(value)
}

fn decode_migration_entry(value: &[u8]) -> Result<MigrationEntry> {
    if value.len() < 6 || &value[..4] != MIGRATION_MAGIC {
        return Err(Error::new(
            "E_STORAGE",
            "invalid migration ledger codec magic",
        ));
    }
    let version = u16::from_be_bytes(value[4..6].try_into().unwrap());
    if version != MIGRATION_CODEC_VERSION {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported migration ledger codec version {version}"),
        ));
    }
    serde_json::from_slice(&value[6..]).map_err(|error| {
        Error::new(
            "E_STORAGE",
            format!("decode migration ledger entry: {error}"),
        )
    })
}

fn encode_catalog_entry(entry: &DurableCatalogEntry, version: u16) -> Result<Vec<u8>> {
    if !matches!(
        version,
        LEGACY_CATALOG_CODEC_VERSION
            | CATALOG_CODEC_VERSION
            | SCALAR_CATALOG_CODEC_VERSION
            | PRODUCTION_CATALOG_CODEC_VERSION
    ) {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported catalog codec version {version}"),
        ));
    }
    let mut value = Vec::from(CATALOG_MAGIC.as_slice());
    value.extend_from_slice(&version.to_be_bytes());
    let mut json = serde_json::to_value(entry)
        .map_err(|error| Error::new("E_STORAGE", format!("encode catalog entry: {error}")))?;
    if let DurableCatalogEntry::Index { definition, .. } = entry {
        let components = definition.effective_components();
        if version < PRODUCTION_CATALOG_CODEC_VERSION
            && (components.len() != 1 || components[0].descending)
        {
            return Err(Error::new(
                "E_STORAGE_UPGRADE_REQUIRED",
                "composite or descending indexes require storage format 5; run storage upgrade --to 5",
            ));
        }
        let object = json
            .get_mut("value")
            .and_then(|value| value.get_mut("definition"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("index catalog JSON has a definition object");
        if version < PRODUCTION_CATALOG_CODEC_VERSION {
            object.remove("components");
            object.insert(
                "column".into(),
                serde_json::Value::String(components[0].column.clone()),
            );
            object.insert(
                "field_path".into(),
                serde_json::to_value(&components[0].field_path).map_err(|error| {
                    Error::new("E_STORAGE", format!("encode index field path: {error}"))
                })?,
            );
        } else {
            object.remove("column");
            object.remove("field_path");
            object.insert(
                "components".into(),
                serde_json::to_value(components).map_err(|error| {
                    Error::new("E_STORAGE", format!("encode index components: {error}"))
                })?,
            );
        }
    }
    if version >= CATALOG_CODEC_VERSION
        && let DurableCatalogEntry::Index { definition, .. } = entry
    {
        let kind = if definition.kind.is_unique() {
            "unique"
        } else {
            "ordinary"
        };
        json.get_mut("value")
            .and_then(|value| value.get_mut("definition"))
            .and_then(serde_json::Value::as_object_mut)
            .expect("index catalog JSON has a definition object")
            .insert("kind".into(), serde_json::Value::String(kind.into()));
    }
    value.extend(
        serde_json::to_vec(&json)
            .map_err(|error| Error::new("E_STORAGE", format!("encode catalog entry: {error}")))?,
    );
    Ok(value)
}

fn decode_catalog_entry(
    key: &[u8],
    value: &[u8],
    expected_version: u16,
) -> Result<DurableCatalogEntry> {
    if key.len() != 9 {
        return Err(Error::new("E_STORAGE", "invalid durable catalog key"));
    }
    if value.len() < 6 || &value[..4] != CATALOG_MAGIC {
        return Err(Error::new("E_STORAGE", "invalid catalog codec magic"));
    }
    let version = u16::from_be_bytes(value[4..6].try_into().unwrap());
    let supported_legacy_entry = expected_version <= CATALOG_CODEC_VERSION
        && matches!(
            version,
            LEGACY_CATALOG_CODEC_VERSION | CATALOG_CODEC_VERSION
        );
    if !supported_legacy_entry && version != expected_version {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported catalog codec version {version}"),
        ));
    }
    let entry: DurableCatalogEntry = serde_json::from_slice(&value[6..])
        .map_err(|error| Error::new("E_STORAGE", format!("decode catalog entry: {error}")))?;
    let id = u64::from_be_bytes(key[1..].try_into().unwrap());
    if key[0] != entry.kind_tag() || id != entry.stable_id() {
        return Err(Error::new(
            "E_STORAGE",
            "catalog key does not match its encoded definition",
        ));
    }
    Ok(entry)
}

fn encode_row_key(table_id: u64, row_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.extend_from_slice(&row_id.to_be_bytes());
    key
}

fn decode_row_key(key: &[u8]) -> Result<(u64, u64)> {
    if key.len() != 16 {
        return Err(Error::new("E_STORAGE", "invalid durable row key"));
    }
    Ok((
        u64::from_be_bytes(key[..8].try_into().unwrap()),
        u64::from_be_bytes(key[8..].try_into().unwrap()),
    ))
}

fn encode_index_key(
    database: &Database,
    index_id: u64,
    value: &Value,
    row_id: u64,
    version: u16,
) -> Result<Vec<u8>> {
    if version == PRODUCTION_INDEX_KEY_VERSION {
        return database.encode_secondary_index_key_v3(index_id, value, row_id);
    }
    let mut key = Vec::new();
    key.extend_from_slice(INDEX_MAGIC);
    key.extend_from_slice(&version.to_be_bytes());
    key.extend_from_slice(&index_id.to_be_bytes());
    match version {
        INDEX_KEY_VERSION => {
            let value = value.index_key();
            let value_len = u32::try_from(value.len())
                .map_err(|_| Error::new("E_LIMIT", "secondary index key exceeds u32 length"))?;
            key.extend_from_slice(&value_len.to_be_bytes());
            key.extend_from_slice(value.as_bytes());
        }
        SCALAR_INDEX_KEY_VERSION => encode_ordered_value(value, &mut key, 0)?,
        _ => {
            return Err(Error::new(
                "E_STORAGE",
                format!("unsupported secondary index key version {version}"),
            ));
        }
    }
    key.extend_from_slice(&row_id.to_be_bytes());
    Ok(key)
}

fn validate_index_key(key: &[u8], expected_version: u16) -> Result<()> {
    if key.len() < 23 || &key[..4] != INDEX_MAGIC {
        return Err(Error::new("E_STORAGE", "invalid secondary index key"));
    }
    let version = u16::from_be_bytes(key[4..6].try_into().unwrap());
    if version != expected_version {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported secondary index key version {version}"),
        ));
    }
    if version == PRODUCTION_INDEX_KEY_VERSION {
        return crate::ordered_key::validate_complete(key);
    }
    if version == INDEX_KEY_VERSION {
        if key.len() < 26 {
            return Err(Error::new(
                "E_STORAGE",
                "invalid secondary index key length",
            ));
        }
        let value_len = u32::from_be_bytes(key[14..18].try_into().unwrap()) as usize;
        if key.len() != 26usize.saturating_add(value_len) {
            return Err(Error::new(
                "E_STORAGE",
                "invalid secondary index key length",
            ));
        }
        std::str::from_utf8(&key[18..18 + value_len])
            .map_err(|error| Error::new("E_STORAGE", format!("index key is not UTF-8: {error}")))?;
    }
    Ok(())
}

fn encode_ordered_value(value: &Value, output: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > crate::model::MAX_DEPTH {
        return Err(Error::new(
            "E_LIMIT",
            "secondary index value is too deeply nested",
        ));
    }
    match value {
        Value::Null => output.push(0x00),
        Value::Bool(false) => output.extend_from_slice(&[0x01, 0x00]),
        Value::Bool(true) => output.extend_from_slice(&[0x01, 0x01]),
        Value::Int(value) => {
            output.push(0x02);
            output.extend_from_slice(&((*value as u64) ^ (1_u64 << 63)).to_be_bytes());
        }
        Value::Float(value) => {
            output.push(0x03);
            let bits = if *value == 0.0 {
                0.0_f64.to_bits()
            } else {
                value.to_bits()
            };
            let ordered = if bits & (1_u64 << 63) == 0 {
                bits ^ (1_u64 << 63)
            } else {
                !bits
            };
            output.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Text(value) => {
            output.push(0x04);
            encode_escaped(value.as_bytes(), output);
        }
        Value::Uuid(value) => {
            output.push(0x05);
            output.extend_from_slice(value.as_bytes());
        }
        Value::Date(value) => {
            output.push(0x06);
            output.extend_from_slice(&((value.epoch_days() as u32) ^ (1_u32 << 31)).to_be_bytes());
        }
        Value::Timestamp(value) => {
            output.push(0x07);
            output.extend_from_slice(
                &((value.epoch_microseconds() as u64) ^ (1_u64 << 63)).to_be_bytes(),
            );
        }
        Value::Duration(value) => {
            output.push(0x08);
            output
                .extend_from_slice(&((value.microseconds() as u64) ^ (1_u64 << 63)).to_be_bytes());
        }
        Value::Decimal(value) => {
            output.push(0x09);
            output.extend_from_slice(&value.scale().to_be_bytes());
            output.extend_from_slice(
                &((value.coefficient() as u128) ^ (1_u128 << 127)).to_be_bytes(),
            );
        }
        Value::Bytes(value) => {
            output.push(0x0a);
            encode_escaped(value.as_slice(), output);
        }
        Value::Named { type_id, value } => {
            output.push(0x0b);
            output.extend_from_slice(&type_id.to_be_bytes());
            encode_ordered_value(value, output, depth + 1)?;
        }
        Value::Enum(value) => {
            output.push(0x0c);
            output.extend_from_slice(&value.id.to_be_bytes());
            encode_escaped(value.variant.as_bytes(), output);
            encode_ordered_items(&value.args, output, depth)?;
        }
        Value::Record(fields) => {
            output.push(0x0d);
            output.extend_from_slice(&(fields.len() as u64).to_be_bytes());
            for (name, value) in fields {
                encode_escaped(name.as_bytes(), output);
                encode_ordered_value(value, output, depth + 1)?;
            }
        }
        Value::Tuple(values) => {
            output.push(0x0e);
            encode_ordered_items(values, output, depth)?;
        }
        Value::List(values) => {
            output.push(0x0f);
            encode_ordered_items(values, output, depth)?;
        }
        Value::Option(None) => output.extend_from_slice(&[0x10, 0x00]),
        Value::Option(Some(value)) => {
            output.extend_from_slice(&[0x10, 0x01]);
            encode_ordered_value(value, output, depth + 1)?;
        }
    }
    Ok(())
}

fn encode_ordered_items(values: &[Value], output: &mut Vec<u8>, depth: usize) -> Result<()> {
    output.extend_from_slice(&(values.len() as u64).to_be_bytes());
    for value in values {
        encode_ordered_value(value, output, depth + 1)?;
    }
    Ok(())
}

fn encode_escaped(bytes: &[u8], output: &mut Vec<u8>) {
    for byte in bytes {
        if *byte == 0 {
            output.extend_from_slice(&[0, 0xff]);
        } else {
            output.push(*byte);
        }
    }
    output.extend_from_slice(&[0, 0]);
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn usize_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn map_bytes(values: &BTreeMap<Vec<u8>, Vec<u8>>) -> u64 {
    values
        .iter()
        .map(|(key, value)| usize_u64(key.len()).saturating_add(usize_u64(value.len())))
        .fold(0, u64::saturating_add)
}

fn set_bytes(values: &BTreeSet<Vec<u8>>) -> u64 {
    values
        .iter()
        .map(|value| usize_u64(value.len()))
        .fold(0, u64::saturating_add)
}

fn resolve_path(path: PathBuf) -> Result<PathBuf> {
    match std::fs::canonicalize(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(&path).is_ok() {
                Err(Error::new(
                    "E_CONFIG",
                    "database path must not be a dangling symbolic link",
                ))
            } else {
                Ok(path)
            }
        }
        Err(error) => Err(Error::new(
            "E_IO",
            format!("resolve database path: {error}"),
        )),
    }
}

fn open_error(error: redb::DatabaseError) -> Error {
    if matches!(error, redb::DatabaseError::DatabaseAlreadyOpen) {
        Error::new("E_BUSY", "database file is already in use")
    } else {
        Error::new("E_STORAGE", format!("open redb database: {error}"))
    }
}

fn storage_error(context: &str, error: impl std::fmt::Display) -> Error {
    Error::new("E_STORAGE", format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_test_row(id: RowId) -> Arc<crate::model::Row> {
        Arc::new(crate::model::Row {
            id,
            fields: BTreeMap::new(),
        })
    }

    #[test]
    fn snapshot_row_cache_is_strictly_byte_bounded_and_lru() {
        let mut cache = SnapshotRowCache::default();
        let entry_bytes = 12 * 1024 * 1024;
        cache.insert((1, 1), cached_test_row(1), entry_bytes);
        cache.insert((1, 2), cached_test_row(2), entry_bytes);
        assert!(cache.get((1, 1)).is_some());
        cache.insert((1, 3), cached_test_row(3), entry_bytes);

        assert!(cache.bytes <= SNAPSHOT_ROW_CACHE_MAX_BYTES);
        assert!(cache.get((1, 1)).is_some());
        assert!(cache.get((1, 2)).is_none());
        assert!(cache.get((1, 3)).is_some());

        let before = cache.bytes;
        cache.insert((1, 4), cached_test_row(4), SNAPSHOT_ROW_CACHE_MAX_BYTES + 1);
        assert_eq!(cache.bytes, before);
        assert!(cache.get((1, 4)).is_none());
    }

    fn execute(database: &mut Database, source: &str) {
        let statements = crate::syntax::parse(source).unwrap();
        let changes_schema = statements
            .iter()
            .any(|statement| statement.statement.changes_schema());
        for statement in statements {
            database.execute(statement.statement).unwrap();
        }
        if changes_schema {
            database.advance_schema_revision().unwrap();
        }
        database.sequence += 1;
    }

    #[test]
    fn delta_plan_only_contains_changed_stable_keys() {
        let mut previous = Database::default();
        execute(
            &mut previous,
            "type Entry =\n  id int\n  label text\ntable entries Entry\n  key id\ncreate index entries (label)\ninsert entries {id = 1, label = \"one\"}\ninsert entries {id = 2, label = \"two\"}",
        );
        let unchanged = PreparedDelta::new(&previous, &previous).unwrap();
        assert!(unchanged.catalog.deletes.is_empty());
        assert!(unchanged.catalog.writes.is_empty());
        assert!(unchanged.rows.deletes.is_empty());
        assert!(unchanged.rows.writes.is_empty());
        assert!(unchanged.secondary_indexes.deletes.is_empty());
        assert!(unchanged.secondary_indexes.inserts.is_empty());

        let mut next = previous.clone();
        execute(
            &mut next,
            "update entries | filter id == 1 | set label = \"changed\"\ndelete entries | filter id == 2\ninsert entries {id = 3, label = \"three\"}",
        );
        let delta = PreparedDelta::new(&previous, &next).unwrap();
        assert_eq!(delta.catalog.deletes.len(), 0);
        assert_eq!(delta.catalog.writes.len(), 1);
        assert_eq!(delta.rows.deletes.len(), 1);
        assert_eq!(delta.rows.writes.len(), 2);
        assert_eq!(delta.secondary_indexes.deletes.len(), 3);
        assert_eq!(delta.secondary_indexes.inserts.len(), 3);
    }

    #[test]
    fn incremental_plan_matches_full_state_diff_for_row_only_batch() {
        let mut previous = Database::default();
        execute(
            &mut previous,
            "type Entry =\n  id int\n  label text\ntable entries Entry\n  key id\ncreate index entries (label)\ninsert entries {id = 1, label = \"one\"}\ninsert entries {id = 2, label = \"two\"}",
        );
        let _ = previous.take_write_set();
        let receipts = ReceiptMap::new();
        let committed_state =
            PreparedState::new(&previous, &receipts, StorageLayout::production()).unwrap();
        let committed = DurableHead::from_prepared(&committed_state);

        let mut next = previous.clone();
        execute(
            &mut next,
            "update entries | filter id == 1 | set label = \"changed\"\ndelete entries | filter id == 2\ninsert entries {id = 3, label = \"three\"}",
        );
        let writes = next.take_write_set();
        let incremental =
            PreparedDelta::incremental(&previous, &receipts, &next, &committed, &receipts, &writes)
                .unwrap();
        let complete = PreparedState::new(&next, &receipts, StorageLayout::production()).unwrap();
        let mut full = PreparedDelta::between(&committed_state, &complete);
        assert_eq!(
            incremental.expected_meta.as_ref().unwrap().0,
            committed.meta
        );
        full.expected_meta = incremental.expected_meta.clone();

        assert_eq!(incremental, full);
    }

    #[test]
    fn scalar_index_keys_preserve_scalar_order_and_zero_bytes() {
        fn payload(value: Value) -> Vec<u8> {
            let key = encode_index_key(
                &Database::default(),
                7,
                &value,
                11,
                SCALAR_INDEX_KEY_VERSION,
            )
            .unwrap();
            key[14..key.len() - 8].to_vec()
        }

        assert!(payload(Value::Int(-1)) < payload(Value::Int(0)));
        assert!(payload(Value::Int(0)) < payload(Value::Int(1)));
        assert!(payload(Value::Text("a".into())) < payload(Value::Text("a\0".into())));
        assert!(
            payload(Value::Bytes(crate::scalars::Bytes::new(vec![0]).unwrap()))
                < payload(Value::Bytes(
                    crate::scalars::Bytes::new(vec![0, 1]).unwrap()
                ))
        );
        assert!(
            payload(Value::Decimal(
                crate::scalars::Decimal::new(-1, 6, 2).unwrap()
            )) < payload(Value::Decimal(
                crate::scalars::Decimal::new(1, 6, 2).unwrap()
            ))
        );
    }
}
