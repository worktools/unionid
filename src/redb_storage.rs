//! Transactional redb storage for the durable Engine mode.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use redb::{
    CompactionError as RedbCompactionError, Database as RedbDatabase, Durability, ReadableDatabase,
    ReadableTable, TableDefinition, TableHandle,
};
use sha2::{Digest, Sha256};

use crate::codec::{PRODUCTION_VALUE_CODEC_VERSION, VALUE_CODEC_VERSION};
use crate::db::{
    Database, DurableCatalogEntry, DurableMeta, DurableTable, IndexDefinition, LogicalWriteSet,
};
use crate::error::{Error, Result};
use crate::idempotency::{
    IdempotencyReceipt, MAX_IDEMPOTENCY_RECEIPT_BYTES, MAX_IDEMPOTENCY_TOTAL_BYTES, ReceiptMap,
    encoded_receipt, ensure_legacy_receipts, validate_receipts,
};
use crate::introspection::StorageVersions;
use crate::migration::{MigrationEntry, MigrationFile};
use crate::model::Value;
use crate::profile::{
    DurableCommitMode, DurableCommitProfile, StorageCheckProfile, StorageOpenProfile,
};
use crate::row_source::{
    EncodedIndexBounds, IndexHit, IndexHitCursor, RowBatch, RowBatchCursor, SOURCE_BATCH_MAX_BYTES,
    SOURCE_BATCH_MAX_ROWS, SourceIdentity, TableStats, TypedRowSource,
};
use crate::{ExecutionObservation, RowId};

const LEGACY_STORAGE_FORMAT_VERSION: u32 = 1;
const RECEIPT_STORAGE_FORMAT_VERSION: u32 = 2;
const CURSOR_STORAGE_FORMAT_VERSION: u32 = 3;
const SCALAR_STORAGE_FORMAT_VERSION: u32 = 4;
const LEGACY_BOUNDED_STORAGE_FORMAT_VERSION: u32 = 5;
pub(crate) const PRODUCTION_STORAGE_FORMAT_VERSION: u32 = 6;
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
const MAINTENANCE_CODEC_VERSION: u16 = 1;
const GENERATION_KEY_CODEC_VERSION: u16 = 1;
const MAINTENANCE_EXECUTOR_VERSION: u16 = 1;
const MAINTENANCE_TARGET_BATCH_MAX_BYTES: usize = 32 * 1024 * 1024;
const MAINTENANCE_GENERATION_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAINTENANCE_RECLAIM_MAX_ENTRIES: usize = 1_024;

pub(crate) fn production_versions() -> StorageVersions {
    let layout = StorageLayout::production();
    StorageVersions {
        format: layout.format,
        catalog_codec: layout.catalog,
        value_codec: layout.value,
        index_key_codec: layout.index,
        migration_codec: layout.migration,
        receipt_codec: layout.receipt,
        maintenance_codec: layout.maintenance,
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
const GENERATION_CATALOG: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("generation_catalog");
const GENERATION_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("generation_rows");
const GENERATION_INDEX: TableDefinition<&[u8], u8> = TableDefinition::new("generation_index");
const MAINTENANCE_GENERATION: TableDefinition<u64, &[u8]> =
    TableDefinition::new("maintenance_generation");

const FORMAT_KEY: &str = "storage_format_version";
const CATALOG_CODEC_KEY: &str = "catalog_codec_version";
const VALUE_CODEC_KEY: &str = "value_codec_version";
const INDEX_KEY_CODEC_KEY: &str = "index_key_version";
const MIGRATION_CODEC_KEY: &str = "migration_codec_version";
const RECEIPT_CODEC_KEY: &str = "receipt_codec_version";
const MAINTENANCE_CODEC_KEY: &str = "maintenance_codec_version";
const ACTIVE_GENERATION_KEY: &str = "active_generation";
const NEXT_GENERATION_ID_KEY: &str = "next_generation_id";
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
const GENERATION_KEY_MAGIC: &[u8; 4] = b"UIDG";
const MAINTENANCE_MAGIC: &[u8; 4] = b"UIDN";

pub(crate) struct RedbStore {
    database: RedbDatabase,
    path: PathBuf,
    committed: DurableHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RedbCompaction {
    pub(crate) changed: bool,
    pub(crate) before_bytes: u64,
    pub(crate) after_bytes: u64,
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
type OwnedKeyBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);
type StoredCatalog = (Vec<DurableCatalogEntry>, BTreeMap<Vec<u8>, Vec<u8>>);

const SNAPSHOT_ROW_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct RedbReadSource {
    transaction: redb::ReadTransaction,
    metadata: Arc<Database>,
    generation: GenerationRef,
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
    fn new(
        transaction: redb::ReadTransaction,
        metadata: Arc<Database>,
        generation: GenerationRef,
    ) -> Self {
        let identity = SourceIdentity {
            database_instance: metadata.durable_meta().cursor_instance_id,
            generation: generation.encoded(),
            sequence: metadata.sequence,
            schema_hash: metadata.schema_info().hash,
        };
        Self {
            transaction,
            metadata,
            generation,
            identity,
            cache: Mutex::new(SnapshotRowCache::default()),
        }
    }

    fn physical_key(&self, logical: &[u8]) -> Result<Vec<u8>> {
        match self.generation {
            GenerationRef::Legacy0 => Ok(logical.to_vec()),
            GenerationRef::Generated(generation) => encode_generation_key(generation, logical),
        }
    }

    fn logical_key<'a>(&self, physical: &'a [u8]) -> Result<&'a [u8]> {
        match self.generation {
            GenerationRef::Legacy0 => Ok(physical),
            GenerationRef::Generated(generation) => decode_generation_key(physical, generation),
        }
    }

    fn physical_bound(&self, bound: Bound<Vec<u8>>) -> Result<Bound<Vec<u8>>> {
        match bound {
            Bound::Included(key) => Ok(Bound::Included(self.physical_key(&key)?)),
            Bound::Excluded(key) => Ok(Bound::Excluded(self.physical_key(&key)?)),
            Bound::Unbounded => Ok(Bound::Unbounded),
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
            // Legacy0 and the initial format-6 envelope have no persisted
            // live-row cardinality. The monotonic allocation cursor remains
            // a safe upper bound until maintenance manifests publish exact
            // generation statistics.
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
        // Exact active-generation span cardinality requires walking the durable range.
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
        let key = self.physical_key(&encode_row_key(table_id, row_id))?;
        let bytes = match self.generation {
            GenerationRef::Legacy0 => {
                let rows = self
                    .transaction
                    .open_table(ROWS)
                    .map_err(|error| storage_error("open rows table", error))?;
                rows.get(key.as_slice())
                    .map_err(|error| storage_error("read durable row", error))?
                    .map(|value| value.value().to_vec())
            }
            GenerationRef::Generated(_) => {
                let rows = self
                    .transaction
                    .open_table(GENERATION_ROWS)
                    .map_err(|error| storage_error("open generation rows table", error))?;
                rows.get(key.as_slice())
                    .map_err(|error| storage_error("read durable generation row", error))?
                    .map(|value| value.value().to_vec())
            }
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
        let lower = self.physical_bound(Bound::Included(encode_row_key(table_id, 0)))?;
        let upper = self.physical_bound(Bound::Included(encode_row_key(table_id, u64::MAX)))?;
        Ok(Box::new(RedbRowCursor {
            source: self,
            table: table.to_owned(),
            table_id,
            lower,
            upper,
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
        let lower = self.physical_bound(lower)?;
        let upper = self.physical_bound(upper)?;
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
        match self.source.generation {
            GenerationRef::Legacy0 => {
                let table = self
                    .source
                    .transaction
                    .open_table(ROWS)
                    .map_err(|error| storage_error("open rows table", error))?;
                self.next_batch_from(&table, control, observation)
            }
            GenerationRef::Generated(_) => {
                let table = self
                    .source
                    .transaction
                    .open_table(GENERATION_ROWS)
                    .map_err(|error| storage_error("open generation rows table", error))?;
                self.next_batch_from(&table, control, observation)
            }
        }
    }
}

impl RedbRowCursor<'_> {
    fn next_batch_from(
        &mut self,
        table: &impl ReadableTable<&'static [u8], &'static [u8]>,
        control: Option<&crate::control::ExecutionControl>,
        observation: &mut ExecutionObservation,
    ) -> Result<Option<RowBatch>> {
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
            let physical_key = key.value();
            let value = value.value();
            let key = self.source.logical_key(physical_key)?;
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
            self.lower = Bound::Excluded(physical_key.to_vec());
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
        match self.source.generation {
            GenerationRef::Legacy0 => {
                let table = self
                    .source
                    .transaction
                    .open_table(SECONDARY_INDEX)
                    .map_err(|error| storage_error("open secondary index table", error))?;
                self.next_forward_batch_from(&table, control)
            }
            GenerationRef::Generated(_) => {
                let table = self
                    .source
                    .transaction
                    .open_table(GENERATION_INDEX)
                    .map_err(|error| storage_error("open generation index table", error))?;
                self.next_forward_batch_from(&table, control)
            }
        }
    }
}

impl RedbIndexCursor<'_> {
    fn next_forward_batch_from(
        &mut self,
        table: &impl ReadableTable<&'static [u8], u8>,
        control: Option<&crate::control::ExecutionControl>,
    ) -> Result<Option<Vec<IndexHit>>> {
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
            let physical_key = key.value();
            if !hits.is_empty()
                && (hits.len() == SOURCE_BATCH_MAX_ROWS
                    || encoded_bytes.saturating_add(physical_key.len()) > SOURCE_BATCH_MAX_BYTES)
            {
                reached_end = false;
                break;
            }
            let key = self.source.logical_key(physical_key)?;
            let hit = decode_durable_index_hit(key, self.index_id, self.component_count)?;
            encoded_bytes = encoded_bytes.saturating_add(physical_key.len());
            hits.push(hit);
            self.lower = Bound::Excluded(physical_key.to_vec());
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
                let bounds = (borrowed_bound(&self.lower), borrowed_bound(&self.upper));
                let Some(physical_key) = self.last_key(bounds)? else {
                    self.done = true;
                    break;
                };
                let key = self.source.logical_key(&physical_key)?;
                let hit = decode_durable_index_hit(key, self.index_id, self.component_count)?;
                let mut before = durable_index_prefix(self.index_id, self.component_count);
                before.extend_from_slice(&hit.boundary);
                let mut first = before.clone();
                first.extend_from_slice(&0_u64.to_be_bytes());
                let mut last = before.clone();
                last.extend_from_slice(&u64::MAX.to_be_bytes());
                self.reverse_group = Some(ReverseIndexGroup {
                    before: self.source.physical_key(&before)?,
                    lower: self.source.physical_bound(Bound::Included(first))?,
                    upper: self.source.physical_bound(Bound::Included(last))?,
                });
            }

            let (group_lower, group_upper, group_before) = {
                let group = self
                    .reverse_group
                    .as_ref()
                    .expect("reverse index group was initialized");
                (
                    group.lower.clone(),
                    group.upper.clone(),
                    group.before.clone(),
                )
            };
            let bounds = (borrowed_bound(&group_lower), borrowed_bound(&group_upper));
            let Some(physical_key) = self.first_key(bounds)? else {
                self.upper = Bound::Excluded(group_before);
                self.reverse_group = None;
                continue;
            };
            if !hits.is_empty()
                && encoded_bytes.saturating_add(physical_key.len()) > SOURCE_BATCH_MAX_BYTES
            {
                break;
            }
            let key = self.source.logical_key(&physical_key)?;
            let hit = decode_durable_index_hit(key, self.index_id, self.component_count)?;
            encoded_bytes = encoded_bytes.saturating_add(physical_key.len());
            hits.push(hit);
            self.reverse_group
                .as_mut()
                .expect("reverse index group was initialized")
                .lower = Bound::Excluded(physical_key);
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

    fn last_key(&self, bounds: (Bound<&[u8]>, Bound<&[u8]>)) -> Result<Option<Vec<u8>>> {
        match self.source.generation {
            GenerationRef::Legacy0 => {
                let table = self
                    .source
                    .transaction
                    .open_table(SECONDARY_INDEX)
                    .map_err(|error| storage_error("open secondary index table", error))?;
                let mut range = table
                    .range::<&[u8]>(bounds)
                    .map_err(|error| storage_error("scan durable secondary index", error))?;
                range
                    .next_back()
                    .transpose()
                    .map_err(|error| storage_error("read durable index range", error))
                    .map(|entry| entry.map(|(key, _)| key.value().to_vec()))
            }
            GenerationRef::Generated(_) => {
                let table = self
                    .source
                    .transaction
                    .open_table(GENERATION_INDEX)
                    .map_err(|error| storage_error("open generation index table", error))?;
                let mut range = table
                    .range::<&[u8]>(bounds)
                    .map_err(|error| storage_error("scan generation index", error))?;
                range
                    .next_back()
                    .transpose()
                    .map_err(|error| storage_error("read generation index range", error))
                    .map(|entry| entry.map(|(key, _)| key.value().to_vec()))
            }
        }
    }

    fn first_key(&self, bounds: (Bound<&[u8]>, Bound<&[u8]>)) -> Result<Option<Vec<u8>>> {
        match self.source.generation {
            GenerationRef::Legacy0 => {
                let table = self
                    .source
                    .transaction
                    .open_table(SECONDARY_INDEX)
                    .map_err(|error| storage_error("open secondary index table", error))?;
                let mut range = table
                    .range::<&[u8]>(bounds)
                    .map_err(|error| storage_error("scan durable secondary index group", error))?;
                range
                    .next()
                    .transpose()
                    .map_err(|error| storage_error("read durable index range", error))
                    .map(|entry| entry.map(|(key, _)| key.value().to_vec()))
            }
            GenerationRef::Generated(_) => {
                let table = self
                    .source
                    .transaction
                    .open_table(GENERATION_INDEX)
                    .map_err(|error| storage_error("open generation index table", error))?;
                let mut range = table
                    .range::<&[u8]>(bounds)
                    .map_err(|error| storage_error("scan generation index group", error))?;
                range
                    .next()
                    .transpose()
                    .map_err(|error| storage_error("read generation index range", error))
                    .map(|entry| entry.map(|(key, _)| key.value().to_vec()))
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum GenerationRef {
    Legacy0,
    Generated(u64),
}

impl GenerationRef {
    fn encoded(self) -> u64 {
        match self {
            Self::Legacy0 => 0,
            Self::Generated(id) => id,
        }
    }

    fn from_encoded(value: u64) -> Self {
        if value == 0 {
            Self::Legacy0
        } else {
            Self::Generated(value)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GenerationState {
    active: GenerationRef,
    next_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MaintenanceState {
    Building,
    Ready,
    Aborting,
    Reclaimable,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct MaintenanceManifest {
    state: MaintenanceState,
    source: GenerationRef,
    target: GenerationRef,
    #[serde(default)]
    database_instance: [u8; 16],
    #[serde(default)]
    migration_id: String,
    #[serde(default)]
    migration_parent: Option<String>,
    #[serde(default)]
    migration_checksum: String,
    #[serde(default)]
    source_schema_revision: u64,
    #[serde(default)]
    source_schema_hash: String,
    #[serde(default)]
    source_sequence: u64,
    #[serde(default)]
    target_schema_revision: u64,
    #[serde(default)]
    target_schema_hash: String,
    #[serde(default)]
    target_catalog_digest: String,
    #[serde(default)]
    checkpoint: Option<(u64, RowId)>,
    #[serde(default)]
    source_rows_seen: u64,
    #[serde(default)]
    target_rows_written: u64,
    #[serde(default)]
    index_entries_written: u64,
    #[serde(default)]
    logical_bytes: u64,
    #[serde(default)]
    rolling_digest: String,
    #[serde(default)]
    executor_version: u16,
    #[serde(default)]
    created_at_unix_ms: u64,
    #[serde(default)]
    updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaintenanceInfo {
    pub(crate) state: MaintenanceState,
    pub(crate) source_generation: u64,
    pub(crate) target_generation: u64,
    pub(crate) migration_id: String,
    pub(crate) migration_checksum: String,
    pub(crate) source_schema_revision: u64,
    pub(crate) source_schema_hash: String,
    pub(crate) source_sequence: u64,
    pub(crate) target_schema_revision: u64,
    pub(crate) target_schema_hash: String,
    pub(crate) checkpoint: Option<(u64, RowId)>,
    pub(crate) source_rows_seen: u64,
    pub(crate) target_rows_written: u64,
    pub(crate) index_entries_written: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) updated_at_unix_ms: u64,
}

impl MaintenanceInfo {
    fn from_manifest(manifest: &MaintenanceManifest) -> Self {
        Self {
            state: manifest.state,
            source_generation: manifest.source.encoded(),
            target_generation: manifest.target.encoded(),
            migration_id: manifest.migration_id.clone(),
            migration_checksum: manifest.migration_checksum.clone(),
            source_schema_revision: manifest.source_schema_revision,
            source_schema_hash: manifest.source_schema_hash.clone(),
            source_sequence: manifest.source_sequence,
            target_schema_revision: manifest.target_schema_revision,
            target_schema_hash: manifest.target_schema_hash.clone(),
            checkpoint: manifest.checkpoint,
            source_rows_seen: manifest.source_rows_seen,
            target_rows_written: manifest.target_rows_written,
            index_entries_written: manifest.index_entries_written,
            logical_bytes: manifest.logical_bytes,
            updated_at_unix_ms: manifest.updated_at_unix_ms,
        }
    }
}

impl GenerationState {
    const fn legacy() -> Self {
        Self {
            active: GenerationRef::Legacy0,
            next_id: 1,
        }
    }

    const fn initial() -> Self {
        Self {
            active: GenerationRef::Generated(1),
            next_id: 2,
        }
    }
}

#[derive(Debug, Clone)]
struct DurableHead {
    layout: StorageLayout,
    meta: DurableMeta,
    catalog_canonical: bool,
    generation: GenerationState,
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
            generation: state.generation,
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
    maintenance: u16,
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
            maintenance: 0,
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
            maintenance: MAINTENANCE_CODEC_VERSION,
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
            maintenance: 0,
        }
    }

    const fn for_format(format: u32) -> Self {
        if format >= PRODUCTION_STORAGE_FORMAT_VERSION {
            Self::production()
        } else if format == LEGACY_BOUNDED_STORAGE_FORMAT_VERSION {
            Self {
                format,
                catalog: PRODUCTION_CATALOG_CODEC_VERSION,
                value: PRODUCTION_VALUE_CODEC_VERSION,
                index: PRODUCTION_INDEX_KEY_VERSION,
                migration: MIGRATION_CODEC_VERSION,
                receipt: PRODUCTION_RECEIPT_CODEC_VERSION,
                maintenance: 0,
            }
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

#[derive(Debug)]
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
        let path = std::fs::canonicalize(&path)
            .map_err(|error| Error::new("E_IO", format!("resolve opened database: {error}")))?;
        let redb_open_micros = elapsed_micros(redb_started);
        let empty = Database::default();
        let initial = PreparedState::new(&empty, &ReceiptMap::new(), StorageLayout::production())?;
        let mut store = Self {
            database,
            path,
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
        if format >= LEGACY_BOUNDED_STORAGE_FORMAT_VERSION {
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
        read_meta(&table).map(|(_, layout, _)| layout)
    }

    pub(crate) fn supports_production_scalars(&self) -> bool {
        self.committed.layout.supports_production_scalars()
    }

    pub(crate) fn supports_bounded_row_mutation(&self) -> bool {
        self.committed.layout.format >= LEGACY_BOUNDED_STORAGE_FORMAT_VERSION
            && self.committed.catalog_canonical
    }

    pub(crate) fn compact(&mut self) -> std::result::Result<RedbCompaction, CommitFailure> {
        let before_bytes = std::fs::metadata(&self.path)
            .map_err(|error| {
                CommitFailure::Definite(Error::new(
                    "E_IO",
                    format!("read database size before compaction: {error}"),
                ))
            })?
            .len();
        self.database.compact().map_err(|error| match error {
            RedbCompactionError::PersistentSavepointExists
            | RedbCompactionError::EphemeralSavepointExists
            | RedbCompactionError::TransactionInProgress => CommitFailure::Definite(Error::new(
                "E_BUSY",
                format!("database compaction requires exclusive access: {error}"),
            )),
            RedbCompactionError::Storage(_) => CommitFailure::Uncertain(storage_error(
                "compact redb database; reopen and run an integrity check before retrying",
                error,
            )),
            _ => CommitFailure::Uncertain(storage_error(
                "compact redb database; its result is unknown",
                error,
            )),
        })?;
        // Native compaction returns before redb has persisted a region-consistent
        // layout. Reopen the same path so the repair that the next open would
        // otherwise run happens inside this maintenance operation, then read the
        // settled file size. redb may round the file up to its region boundary,
        // so only an actually smaller file counts as changed; a run that leaves
        // the durable size unchanged is a valid no-op.
        let after_bytes = self.reopen_after_compaction()?;
        Ok(RedbCompaction {
            changed: before_bytes > after_bytes,
            before_bytes,
            after_bytes,
        })
    }

    fn reopen_after_compaction(&mut self) -> std::result::Result<u64, CommitFailure> {
        let placeholder = RedbDatabase::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(|error| {
                CommitFailure::Uncertain(storage_error(
                    "create compaction reopen placeholder",
                    error,
                ))
            })?;
        let compacted = std::mem::replace(&mut self.database, placeholder);
        drop(compacted);
        let compacted_bytes = std::fs::metadata(&self.path)
            .map_err(|error| {
                CommitFailure::Uncertain(Error::new(
                    "E_IO",
                    format!("stat database before compaction reopen: {error}"),
                ))
            })?
            .len();
        if compacted_bytes == 0 {
            return Err(CommitFailure::Uncertain(Error::new(
                "E_STORAGE",
                "database disappeared during compaction; reopen and run an integrity check",
            )));
        }
        self.database = RedbDatabase::create(&self.path).map_err(|error| {
            CommitFailure::Uncertain(storage_error(
                "reopen redb database after compaction",
                error,
            ))
        })?;
        let after_bytes = std::fs::metadata(&self.path)
            .map_err(|error| {
                CommitFailure::Uncertain(Error::new(
                    "E_IO",
                    format!("read database size after compaction: {error}"),
                ))
            })?
            .len();
        if after_bytes == 0 {
            return Err(CommitFailure::Uncertain(Error::new(
                "E_STORAGE",
                "database disappeared during compaction; reopen and run an integrity check",
            )));
        }
        Ok(after_bytes)
    }

    pub(crate) fn committed_view(
        &self,
        database: &Database,
    ) -> Result<(Arc<Database>, Arc<dyn TypedRowSource>)> {
        if self.committed.layout.format < LEGACY_BOUNDED_STORAGE_FORMAT_VERSION {
            let database = Arc::new(database.clone());
            let source: Arc<dyn TypedRowSource> = database.clone();
            return Ok((database, source));
        }
        let metadata = Arc::new(database.metadata_only()?);
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin committed redb read view", error))?;
        let (meta, layout, generation) = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open committed meta table", error))?;
            read_meta(&table)?
        };
        if layout != self.committed.layout
            || generation != self.committed.generation
            || meta != database.durable_meta()
        {
            return Err(Error::new(
                "E_STORAGE_REOPEN_REQUIRED",
                "committed redb read view does not match the published database state",
            ));
        }
        let source: Arc<dyn TypedRowSource> = Arc::new(RedbReadSource::new(
            transaction,
            metadata.clone(),
            generation.active,
        ));
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
            maintenance_codec: layout.maintenance,
            backup_codec: crate::backup::PRODUCTION_BACKUP_FORMAT_VERSION,
        }
    }

    pub(crate) fn maintenance_info(&self) -> Result<Option<MaintenanceInfo>> {
        if self.committed.layout.format < PRODUCTION_STORAGE_FORMAT_VERSION {
            return Ok(None);
        }
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin maintenance status transaction", error))?;
        read_maintenance_manifest(&transaction)
            .map(|entry| entry.map(|(_, manifest)| MaintenanceInfo::from_manifest(&manifest)))
    }

    fn read_manifest(&self, target_generation: u64) -> Result<MaintenanceManifest> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin maintenance manifest read", error))?;
        let table = transaction
            .open_table(MAINTENANCE_GENERATION)
            .map_err(|error| storage_error("open maintenance generation table", error))?;
        let value = table
            .get(target_generation)
            .map_err(|error| storage_error("read maintenance generation", error))?
            .ok_or_else(|| {
                Error::new("E_MAINTENANCE_REQUIRED", "maintenance manifest not found")
            })?;
        decode_maintenance_manifest(value.value())
    }

    fn generation_summary(&self, generation: GenerationRef) -> Result<GenerationSummary> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin generation summary", error))?;
        let catalog = match generation {
            GenerationRef::Legacy0 => transaction
                .open_table(CATALOG)
                .map_err(|error| storage_error("open generation catalog", error))?,
            GenerationRef::Generated(_) => transaction
                .open_table(GENERATION_CATALOG)
                .map_err(|error| storage_error("open generation catalog", error))?,
        };
        let rows = match generation {
            GenerationRef::Legacy0 => transaction
                .open_table(ROWS)
                .map_err(|error| storage_error("open generation rows", error))?,
            GenerationRef::Generated(_) => transaction
                .open_table(GENERATION_ROWS)
                .map_err(|error| storage_error("open generation rows", error))?,
        };
        let indexes = match generation {
            GenerationRef::Legacy0 => transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| storage_error("open generation indexes", error))?,
            GenerationRef::Generated(_) => transaction
                .open_table(GENERATION_INDEX)
                .map_err(|error| storage_error("open generation indexes", error))?,
        };
        let (lower, upper) = generation_scan_bounds(generation)?;
        let mut catalog_values = BTreeMap::new();
        let mut logical_bytes = 0_u64;
        for entry in catalog
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("scan generation catalog", error))?
        {
            let (key, value) =
                entry.map_err(|error| storage_error("read generation catalog", error))?;
            let key = logical_generation_key(generation, key.value())?.to_vec();
            let value = value.value().to_vec();
            logical_bytes = logical_bytes
                .saturating_add(u64::try_from(key.len()).unwrap_or(u64::MAX))
                .saturating_add(u64::try_from(value.len()).unwrap_or(u64::MAX));
            catalog_values.insert(key, value);
        }
        let mut row_count = 0_u64;
        let mut rolling_digest = empty_rolling_digest();
        for entry in rows
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("scan generation rows", error))?
        {
            let (key, value) =
                entry.map_err(|error| storage_error("read generation row", error))?;
            let key = logical_generation_key(generation, key.value())?.to_vec();
            let value = value.value().to_vec();
            logical_bytes = logical_bytes
                .saturating_add(u64::try_from(key.len()).unwrap_or(u64::MAX))
                .saturating_add(u64::try_from(value.len()).unwrap_or(u64::MAX));
            rolling_digest = advance_rolling_digest_entry(&rolling_digest, &key, &value)?;
            row_count = row_count.saturating_add(1);
        }
        let mut index_count = 0_u64;
        for entry in indexes
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("scan generation indexes", error))?
        {
            let (key, _) = entry.map_err(|error| storage_error("read generation index", error))?;
            let key = logical_generation_key(generation, key.value())?;
            logical_bytes =
                logical_bytes.saturating_add(u64::try_from(key.len()).unwrap_or(u64::MAX));
            index_count = index_count.saturating_add(1);
        }
        Ok(GenerationSummary {
            catalog_digest: digest_bytes_map(&catalog_values),
            rows: row_count,
            indexes: index_count,
            logical_bytes,
            rolling_digest,
        })
    }

    pub(crate) fn start_maintenance(
        &mut self,
        source: &Database,
        target: &Database,
        file: &MigrationFile,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        if self.committed.layout.format != PRODUCTION_STORAGE_FORMAT_VERSION {
            return Err(CommitFailure::Definite(Error::new(
                "E_STORAGE_UPGRADE_REQUIRED",
                "recoverable migrations require storage format 6",
            )));
        }
        if source.durable_meta() != self.committed.meta {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "migration source no longer matches the committed database",
            )));
        }
        if target.sequence != source.sequence.saturating_add(1)
            || target.schema_info().revision != source.schema_info().revision.saturating_add(1)
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "migration target does not extend the source identity",
            )));
        }
        let target_id = self.committed.generation.next_id;
        let target_generation = GenerationRef::Generated(target_id);
        let next_id = target_id.checked_add(1).ok_or_else(|| {
            CommitFailure::Definite(Error::new("E_LIMIT", "maintenance generation ID exhausted"))
        })?;
        let target_state = GenerationState {
            active: target_generation,
            next_id,
        };
        let prepared = PreparedState::new_in_generation(
            target,
            &ReceiptMap::new(),
            self.committed.layout,
            target_state,
        )
        .map_err(CommitFailure::Definite)?;
        if !prepared.rows.is_empty() || !prepared.secondary_indexes.is_empty() {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance target planning must not contain resident rows",
            )));
        }
        let catalog_bytes = map_bytes(&prepared.catalog);
        if catalog_bytes > MAINTENANCE_GENERATION_MAX_BYTES {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_LIMIT",
                "target generation catalog exceeds the 1 GiB logical budget",
            )));
        }
        let now = maintenance_time_ms().map_err(CommitFailure::Definite)?;
        let manifest = MaintenanceManifest {
            state: MaintenanceState::Building,
            source: self.committed.generation.active,
            target: target_generation,
            database_instance: source.durable_meta().cursor_instance_id,
            migration_id: file.id.clone(),
            migration_parent: file.parent.clone(),
            migration_checksum: file.checksum.clone(),
            source_schema_revision: source.schema_info().revision,
            source_schema_hash: source.schema_info().hash,
            source_sequence: source.sequence,
            target_schema_revision: target.schema_info().revision,
            target_schema_hash: target.schema_info().hash,
            target_catalog_digest: digest_bytes_map(&prepared.catalog),
            checkpoint: None,
            source_rows_seen: 0,
            target_rows_written: 0,
            index_entries_written: 0,
            logical_bytes: catalog_bytes,
            rolling_digest: empty_rolling_digest(),
            executor_version: MAINTENANCE_EXECUTOR_VERSION,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        };
        let expected = (
            self.committed.meta.clone(),
            self.committed.layout,
            self.committed.generation,
        );
        let next_generation = GenerationState {
            active: self.committed.generation.active,
            next_id,
        };
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin maintenance start", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure maintenance start", error))?;
        transaction.set_two_phase_commit(true);
        {
            let mut manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            if manifests
                .iter()
                .map_err(|error| CommitFailure::definite("read maintenance manifest", error))?
                .next()
                .is_some()
            {
                return Err(CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_CONFLICT",
                    "another maintenance generation already exists",
                )));
            }
            let encoded =
                encode_maintenance_manifest(&manifest).map_err(CommitFailure::Definite)?;
            manifests
                .insert(target_id, encoded.as_slice())
                .map_err(|error| CommitFailure::definite("write maintenance manifest", error))?;
        }
        {
            let mut catalog = transaction
                .open_table(GENERATION_CATALOG)
                .map_err(|error| CommitFailure::definite("open generation catalog", error))?;
            for (key, value) in &prepared.catalog {
                let key = encode_generation_key(target_id, key).map_err(CommitFailure::Definite)?;
                if catalog
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|error| {
                        CommitFailure::definite("write target generation catalog", error)
                    })?
                    .is_some()
                {
                    return Err(CommitFailure::Definite(Error::new(
                        "E_STORAGE",
                        "new maintenance generation already contains catalog data",
                    )));
                }
            }
        }
        {
            let mut meta = transaction
                .open_table(META)
                .map_err(|error| CommitFailure::definite("open maintenance meta", error))?;
            write_meta(
                &mut meta,
                Some(&expected),
                &self.committed.meta,
                self.committed.layout,
                next_generation,
            )
            .map_err(CommitFailure::Definite)?;
        }
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit maintenance start", error))?;
        self.committed.generation = next_generation;
        Ok(MaintenanceInfo::from_manifest(&manifest))
    }

    pub(crate) fn append_maintenance_batch(
        &mut self,
        file: &MigrationFile,
        target_batch: &Database,
        checkpoint: (u64, RowId),
        source_rows: usize,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        if source_rows == 0 || source_rows > SOURCE_BATCH_MAX_ROWS {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_LIMIT",
                "maintenance source batch must contain 1 to 1024 rows",
            )));
        }
        let current = self
            .maintenance_info()
            .map_err(CommitFailure::Definite)?
            .ok_or_else(|| {
                CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_REQUIRED",
                    "no maintenance generation is available to build",
                ))
            })?;
        if current.state != MaintenanceState::Building {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance generation is not in the building state",
            )));
        }
        verify_maintenance_file(&current, file).map_err(CommitFailure::Definite)?;
        if current
            .checkpoint
            .is_some_and(|previous| checkpoint <= previous)
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance checkpoint must advance monotonically",
            )));
        }
        let target_id = current.target_generation;
        let target_state = GenerationState {
            active: GenerationRef::Generated(target_id),
            next_id: self.committed.generation.next_id,
        };
        let prepared = PreparedState::new_in_generation(
            target_batch,
            &ReceiptMap::new(),
            self.committed.layout,
            target_state,
        )
        .map_err(CommitFailure::Definite)?;
        if digest_bytes_map(&prepared.catalog)
            != self
                .read_manifest(target_id)
                .map_err(CommitFailure::Definite)?
                .target_catalog_digest
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "migration target catalog changed while building",
            )));
        }
        let batch_bytes =
            map_bytes(&prepared.rows).saturating_add(set_bytes(&prepared.secondary_indexes));
        if batch_bytes > u64::try_from(MAINTENANCE_TARGET_BATCH_MAX_BYTES).unwrap() {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_LIMIT",
                "maintenance target batch exceeds 32 MiB",
            )));
        }

        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin maintenance batch", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure maintenance batch", error))?;
        transaction.set_two_phase_commit(true);
        let mut manifest = {
            let manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            let value = manifests
                .get(target_id)
                .map_err(|error| CommitFailure::definite("read maintenance manifest", error))?
                .ok_or_else(|| {
                    CommitFailure::Definite(Error::new(
                        "E_MAINTENANCE_CONFLICT",
                        "maintenance manifest disappeared",
                    ))
                })?;
            decode_maintenance_manifest(value.value()).map_err(CommitFailure::Definite)?
        };
        verify_manifest_file(&manifest, file).map_err(CommitFailure::Definite)?;
        if manifest.state != MaintenanceState::Building
            || manifest
                .checkpoint
                .is_some_and(|previous| checkpoint <= previous)
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance checkpoint or state changed before the batch",
            )));
        }
        let next_bytes = manifest
            .logical_bytes
            .checked_add(batch_bytes)
            .ok_or_else(|| {
                CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_LIMIT",
                    "maintenance logical byte counter overflow",
                ))
            })?;
        if next_bytes > MAINTENANCE_GENERATION_MAX_BYTES {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_LIMIT",
                "target generation exceeds the 1 GiB logical budget",
            )));
        }
        {
            let mut rows = transaction
                .open_table(GENERATION_ROWS)
                .map_err(|error| CommitFailure::definite("open target generation rows", error))?;
            for (key, value) in &prepared.rows {
                let key = encode_generation_key(target_id, key).map_err(CommitFailure::Definite)?;
                if rows
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|error| CommitFailure::definite("write target generation row", error))?
                    .is_some()
                {
                    return Err(CommitFailure::Definite(Error::new(
                        "E_STORAGE",
                        "target generation row was already written",
                    )));
                }
            }
        }
        {
            let mut indexes = transaction.open_table(GENERATION_INDEX).map_err(|error| {
                CommitFailure::definite("open target generation indexes", error)
            })?;
            for key in &prepared.secondary_indexes {
                let key = encode_generation_key(target_id, key).map_err(CommitFailure::Definite)?;
                if indexes
                    .insert(key.as_slice(), 0)
                    .map_err(|error| {
                        CommitFailure::definite("write target generation index", error)
                    })?
                    .is_some()
                {
                    return Err(CommitFailure::Definite(Error::new(
                        "E_STORAGE",
                        "target generation index key was already written",
                    )));
                }
            }
        }
        manifest.checkpoint = Some(checkpoint);
        manifest.source_rows_seen = manifest
            .source_rows_seen
            .saturating_add(u64::try_from(source_rows).unwrap_or(u64::MAX));
        manifest.target_rows_written = manifest
            .target_rows_written
            .saturating_add(u64::try_from(prepared.rows.len()).unwrap_or(u64::MAX));
        manifest.index_entries_written = manifest
            .index_entries_written
            .saturating_add(u64::try_from(prepared.secondary_indexes.len()).unwrap_or(u64::MAX));
        manifest.logical_bytes = next_bytes;
        manifest.rolling_digest = advance_rolling_digest(&manifest.rolling_digest, &prepared.rows)
            .map_err(CommitFailure::Definite)?;
        manifest.updated_at_unix_ms = maintenance_time_ms().map_err(CommitFailure::Definite)?;
        {
            let mut manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            let encoded =
                encode_maintenance_manifest(&manifest).map_err(CommitFailure::Definite)?;
            manifests
                .insert(target_id, encoded.as_slice())
                .map_err(|error| CommitFailure::definite("checkpoint maintenance batch", error))?;
        }
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit maintenance batch", error))?;
        Ok(MaintenanceInfo::from_manifest(&manifest))
    }

    pub(crate) fn mark_maintenance_ready(
        &mut self,
        file: &MigrationFile,
        target: &Database,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        let current = self
            .maintenance_info()
            .map_err(CommitFailure::Definite)?
            .ok_or_else(|| {
                CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_REQUIRED",
                    "no maintenance generation is available to validate",
                ))
            })?;
        verify_maintenance_file(&current, file).map_err(CommitFailure::Definite)?;
        if current.state == MaintenanceState::Ready {
            return Ok(current);
        }
        if current.state != MaintenanceState::Building
            || current.target_schema_revision != target.schema_info().revision
            || current.target_schema_hash != target.schema_info().hash
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance target identity changed before validation",
            )));
        }
        let target_generation = GenerationRef::Generated(current.target_generation);
        self.validate_bounded_integrity_for(target, target_generation)
            .map_err(CommitFailure::Definite)?;
        let summary = self
            .generation_summary(target_generation)
            .map_err(CommitFailure::Definite)?;
        let manifest = self
            .read_manifest(current.target_generation)
            .map_err(CommitFailure::Definite)?;
        if summary.catalog_digest != manifest.target_catalog_digest
            || summary.rows != manifest.target_rows_written
            || summary.indexes != manifest.index_entries_written
            || summary.logical_bytes != manifest.logical_bytes
            || summary.rolling_digest != manifest.rolling_digest
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_STORAGE",
                "target generation counters or rolling digest do not match its manifest",
            )));
        }
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin maintenance validation", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure maintenance validation", error))?;
        transaction.set_two_phase_commit(true);
        let mut ready = {
            let manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            let value = manifests
                .get(current.target_generation)
                .map_err(|error| CommitFailure::definite("read maintenance manifest", error))?
                .ok_or_else(|| {
                    CommitFailure::Definite(Error::new(
                        "E_MAINTENANCE_CONFLICT",
                        "maintenance manifest disappeared before validation",
                    ))
                })?;
            decode_maintenance_manifest(value.value()).map_err(CommitFailure::Definite)?
        };
        verify_manifest_file(&ready, file).map_err(CommitFailure::Definite)?;
        if ready.state != MaintenanceState::Building
            || ready.checkpoint != manifest.checkpoint
            || ready.rolling_digest != manifest.rolling_digest
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance generation changed during validation",
            )));
        }
        ready.state = MaintenanceState::Ready;
        ready.updated_at_unix_ms = maintenance_time_ms().map_err(CommitFailure::Definite)?;
        {
            let mut manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            let encoded = encode_maintenance_manifest(&ready).map_err(CommitFailure::Definite)?;
            manifests
                .insert(current.target_generation, encoded.as_slice())
                .map_err(|error| CommitFailure::definite("mark maintenance ready", error))?;
        }
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit maintenance validation", error))?;
        Ok(MaintenanceInfo::from_manifest(&ready))
    }

    pub(crate) fn cutover_maintenance(
        &mut self,
        file: &MigrationFile,
        target: &Database,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        let current = self
            .maintenance_info()
            .map_err(CommitFailure::Definite)?
            .ok_or_else(|| {
                CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_REQUIRED",
                    "no maintenance generation is ready for cutover",
                ))
            })?;
        verify_maintenance_file(&current, file).map_err(CommitFailure::Definite)?;
        if current.state != MaintenanceState::Ready
            || current.source_generation != self.committed.generation.active.encoded()
            || current.source_schema_revision != self.committed.meta.schema_revision
            || current.source_schema_hash != self.committed.meta.schema_hash
            || current.source_sequence != self.committed.meta.sequence
            || current.target_schema_revision != target.schema_info().revision
            || current.target_schema_hash != target.schema_info().hash
        {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "ready maintenance generation no longer matches the cutover source or target",
            )));
        }
        let migration_position = target
            .durable_migrations()
            .len()
            .checked_sub(1)
            .ok_or_else(|| {
                CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_CONFLICT",
                    "cutover target has no migration ledger entry",
                ))
            })?;
        let entry = &target.durable_migrations()[migration_position];
        if entry.id != file.id || entry.checksum != file.checksum {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "cutover migration ledger entry does not match the maintenance file",
            )));
        }
        let target_generation = GenerationRef::Generated(current.target_generation);
        let next_generation = GenerationState {
            active: target_generation,
            next_id: self.committed.generation.next_id,
        };
        let expected = (
            self.committed.meta.clone(),
            self.committed.layout,
            self.committed.generation,
        );
        let expected_manifest = self
            .read_manifest(current.target_generation)
            .map_err(CommitFailure::Definite)?;
        let mut reclaimable = expected_manifest.clone();
        reclaimable.state = MaintenanceState::Reclaimable;
        reclaimable.updated_at_unix_ms = maintenance_time_ms().map_err(CommitFailure::Definite)?;
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin maintenance cutover", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure maintenance cutover", error))?;
        transaction.set_two_phase_commit(true);
        {
            let mut meta = transaction
                .open_table(META)
                .map_err(|error| CommitFailure::definite("open cutover meta", error))?;
            write_meta(
                &mut meta,
                Some(&expected),
                &target.durable_meta(),
                self.committed.layout,
                next_generation,
            )
            .map_err(CommitFailure::Definite)?;
        }
        {
            let mut ledger = transaction
                .open_table(MIGRATION_LEDGER)
                .map_err(|error| CommitFailure::definite("open migration ledger", error))?;
            if ledger
                .get(u64::try_from(migration_position).unwrap_or(u64::MAX))
                .map_err(|error| CommitFailure::definite("read migration ledger", error))?
                .is_some()
            {
                return Err(CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_CONFLICT",
                    "cutover migration ledger position is already occupied",
                )));
            }
            let encoded = encode_migration_entry(entry).map_err(CommitFailure::Definite)?;
            ledger
                .insert(
                    u64::try_from(migration_position).unwrap_or(u64::MAX),
                    encoded.as_slice(),
                )
                .map_err(|error| CommitFailure::definite("append migration ledger", error))?;
        }
        {
            let mut manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            let value = manifests
                .get(current.target_generation)
                .map_err(|error| CommitFailure::definite("read maintenance manifest", error))?
                .ok_or_else(|| {
                    CommitFailure::Definite(Error::new(
                        "E_MAINTENANCE_CONFLICT",
                        "maintenance manifest disappeared before cutover",
                    ))
                })?;
            let stored =
                decode_maintenance_manifest(value.value()).map_err(CommitFailure::Definite)?;
            drop(value);
            if stored != expected_manifest {
                return Err(CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_CONFLICT",
                    "maintenance manifest changed before cutover",
                )));
            }
            let encoded =
                encode_maintenance_manifest(&reclaimable).map_err(CommitFailure::Definite)?;
            manifests
                .insert(current.target_generation, encoded.as_slice())
                .map_err(|error| CommitFailure::definite("mark source reclaimable", error))?;
        }
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit maintenance cutover", error))?;
        self.committed.meta = target.durable_meta();
        self.committed.generation = next_generation;
        self.committed.catalog_canonical = true;
        Ok(MaintenanceInfo::from_manifest(&reclaimable))
    }

    pub(crate) fn begin_maintenance_abort(
        &mut self,
    ) -> std::result::Result<Option<MaintenanceInfo>, CommitFailure> {
        let Some(current) = self.maintenance_info().map_err(CommitFailure::Definite)? else {
            return Ok(None);
        };
        if current.state == MaintenanceState::Reclaimable {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "an applied migration cannot be aborted; reclaim its old generation instead",
            )));
        }
        if current.state == MaintenanceState::Aborting {
            return Ok(Some(current));
        }
        let mut manifest = self
            .read_manifest(current.target_generation)
            .map_err(CommitFailure::Definite)?;
        if !matches!(
            manifest.state,
            MaintenanceState::Building | MaintenanceState::Ready
        ) {
            return Err(CommitFailure::Definite(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "maintenance generation cannot enter abort cleanup from its current state",
            )));
        }
        manifest.state = MaintenanceState::Aborting;
        manifest.updated_at_unix_ms = maintenance_time_ms().map_err(CommitFailure::Definite)?;
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin maintenance abort", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure maintenance abort", error))?;
        transaction.set_two_phase_commit(true);
        {
            let mut manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            let encoded =
                encode_maintenance_manifest(&manifest).map_err(CommitFailure::Definite)?;
            manifests
                .insert(current.target_generation, encoded.as_slice())
                .map_err(|error| CommitFailure::definite("mark maintenance aborting", error))?;
        }
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit maintenance abort", error))?;
        Ok(Some(MaintenanceInfo::from_manifest(&manifest)))
    }

    pub(crate) fn reclaim_maintenance_step(&mut self) -> std::result::Result<bool, CommitFailure> {
        let Some(current) = self.maintenance_info().map_err(CommitFailure::Definite)? else {
            return Ok(true);
        };
        let manifest = self
            .read_manifest(current.target_generation)
            .map_err(CommitFailure::Definite)?;
        let generation = match manifest.state {
            MaintenanceState::Aborting => manifest.target,
            MaintenanceState::Reclaimable => manifest.source,
            MaintenanceState::Building | MaintenanceState::Ready => {
                return Err(CommitFailure::Definite(Error::new(
                    "E_MAINTENANCE_REQUIRED",
                    "maintenance generation must be aborted or cut over before cleanup",
                )));
            }
        };
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|error| CommitFailure::definite("begin generation reclaim", error))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|error| CommitFailure::definite("configure generation reclaim", error))?;
        transaction.set_two_phase_commit(true);
        let mut remaining = MAINTENANCE_RECLAIM_MAX_ENTRIES;
        match generation {
            GenerationRef::Legacy0 => {
                {
                    let mut table = transaction
                        .open_table(CATALOG)
                        .map_err(|error| CommitFailure::definite("open reclaim catalog", error))?;
                    remaining = remaining.saturating_sub(
                        delete_bytes_entries(&mut table, generation, remaining)
                            .map_err(CommitFailure::Definite)?,
                    );
                }
                {
                    let mut table = transaction
                        .open_table(ROWS)
                        .map_err(|error| CommitFailure::definite("open reclaim rows", error))?;
                    remaining = remaining.saturating_sub(
                        delete_bytes_entries(&mut table, generation, remaining)
                            .map_err(CommitFailure::Definite)?,
                    );
                }
                {
                    let mut table = transaction
                        .open_table(SECONDARY_INDEX)
                        .map_err(|error| CommitFailure::definite("open reclaim indexes", error))?;
                    let _ = delete_set_entries(&mut table, generation, remaining)
                        .map_err(CommitFailure::Definite)?;
                }
            }
            GenerationRef::Generated(_) => {
                {
                    let mut table = transaction
                        .open_table(GENERATION_CATALOG)
                        .map_err(|error| CommitFailure::definite("open reclaim catalog", error))?;
                    remaining = remaining.saturating_sub(
                        delete_bytes_entries(&mut table, generation, remaining)
                            .map_err(CommitFailure::Definite)?,
                    );
                }
                {
                    let mut table = transaction
                        .open_table(GENERATION_ROWS)
                        .map_err(|error| CommitFailure::definite("open reclaim rows", error))?;
                    remaining = remaining.saturating_sub(
                        delete_bytes_entries(&mut table, generation, remaining)
                            .map_err(CommitFailure::Definite)?,
                    );
                }
                {
                    let mut table = transaction
                        .open_table(GENERATION_INDEX)
                        .map_err(|error| CommitFailure::definite("open reclaim indexes", error))?;
                    let _ = delete_set_entries(&mut table, generation, remaining)
                        .map_err(CommitFailure::Definite)?;
                }
            }
        }
        let complete =
            generation_tables_empty(&transaction, generation).map_err(CommitFailure::Definite)?;
        {
            let mut manifests = transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("open maintenance manifest", error))?;
            if complete {
                manifests
                    .remove(current.target_generation)
                    .map_err(|error| {
                        CommitFailure::definite("remove maintenance manifest", error)
                    })?;
            } else {
                let mut updated = manifest;
                updated.updated_at_unix_ms =
                    maintenance_time_ms().map_err(CommitFailure::Definite)?;
                let encoded =
                    encode_maintenance_manifest(&updated).map_err(CommitFailure::Definite)?;
                manifests
                    .insert(current.target_generation, encoded.as_slice())
                    .map_err(|error| {
                        CommitFailure::definite("checkpoint generation reclaim", error)
                    })?;
            }
        }
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit generation reclaim", error))?;
        Ok(complete)
    }

    pub(crate) fn upgrade(
        &mut self,
        database: &Database,
        receipts: &ReceiptMap,
        target: u32,
    ) -> std::result::Result<UpgradeResult, CommitFailure> {
        if !matches!(
            target,
            SCALAR_STORAGE_FORMAT_VERSION
                | LEGACY_BOUNDED_STORAGE_FORMAT_VERSION
                | PRODUCTION_STORAGE_FORMAT_VERSION
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
        if target == PRODUCTION_STORAGE_FORMAT_VERSION {
            if previous.format != LEGACY_BOUNDED_STORAGE_FORMAT_VERSION {
                return Err(CommitFailure::Definite(Error::new(
                    "E_STORAGE_UPGRADE",
                    "storage format 6 requires format 5; upgrade to 5 first",
                )));
            }
            let generation = GenerationState::legacy();
            let layout = StorageLayout::production();
            let expected = (database.durable_meta(), previous, self.committed.generation);
            let mut transaction = self
                .database
                .begin_write()
                .map_err(|error| CommitFailure::definite("begin format-6 upgrade", error))?;
            transaction
                .set_durability(Durability::Immediate)
                .map_err(|error| CommitFailure::definite("configure format-6 upgrade", error))?;
            transaction.set_two_phase_commit(true);
            transaction
                .open_table(GENERATION_CATALOG)
                .map_err(|error| CommitFailure::definite("create generation catalog", error))?;
            transaction
                .open_table(GENERATION_ROWS)
                .map_err(|error| CommitFailure::definite("create generation rows", error))?;
            transaction
                .open_table(GENERATION_INDEX)
                .map_err(|error| CommitFailure::definite("create generation index", error))?;
            transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| CommitFailure::definite("create maintenance generation", error))?;
            {
                let mut meta = transaction
                    .open_table(META)
                    .map_err(|error| CommitFailure::definite("open format-6 meta", error))?;
                write_meta(
                    &mut meta,
                    Some(&expected),
                    &database.durable_meta(),
                    layout,
                    generation,
                )
                .map_err(CommitFailure::Definite)?;
            }
            transaction
                .commit()
                .map_err(|error| CommitFailure::uncertain("commit format-6 upgrade", error))?;
            self.committed.layout = layout;
            self.committed.generation = generation;
            return Ok(UpgradeResult {
                previous_format: previous.format,
                format: target,
                changed: true,
            });
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
            PreparedState::new_in_generation(database, receipts, layout, self.committed.generation)
                .map_err(CommitFailure::Definite)?;
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
                prepared.generation,
            )
            .map_err(CommitFailure::Definite)?;
        }
        match prepared.generation.active {
            GenerationRef::Legacy0 => {
                let mut catalog = transaction
                    .open_table(CATALOG)
                    .map_err(|error| CommitFailure::definite("open catalog table", error))?;
                apply_bytes_delta(&mut catalog, &prepared.catalog, "catalog entry")
                    .map_err(CommitFailure::Definite)?;
                let mut rows = transaction
                    .open_table(ROWS)
                    .map_err(|error| CommitFailure::definite("open rows table", error))?;
                apply_bytes_delta(&mut rows, &prepared.rows, "row")
                    .map_err(CommitFailure::Definite)?;
                let mut indexes = transaction.open_table(SECONDARY_INDEX).map_err(|error| {
                    CommitFailure::definite("open secondary index table", error)
                })?;
                apply_set_delta(&mut indexes, &prepared.secondary_indexes)
                    .map_err(CommitFailure::Definite)?;
            }
            GenerationRef::Generated(generation) => {
                let catalog_delta = generation_bytes_delta(&prepared.catalog, generation)
                    .map_err(CommitFailure::Definite)?;
                let row_delta = generation_bytes_delta(&prepared.rows, generation)
                    .map_err(CommitFailure::Definite)?;
                let index_delta = generation_set_delta(&prepared.secondary_indexes, generation)
                    .map_err(CommitFailure::Definite)?;
                let mut catalog = transaction
                    .open_table(GENERATION_CATALOG)
                    .map_err(|error| {
                        CommitFailure::definite("open generation catalog table", error)
                    })?;
                apply_bytes_delta(&mut catalog, &catalog_delta, "generation catalog entry")
                    .map_err(CommitFailure::Definite)?;
                let mut rows = transaction.open_table(GENERATION_ROWS).map_err(|error| {
                    CommitFailure::definite("open generation rows table", error)
                })?;
                apply_bytes_delta(&mut rows, &row_delta, "generation row")
                    .map_err(CommitFailure::Definite)?;
                let mut indexes = transaction.open_table(GENERATION_INDEX).map_err(|error| {
                    CommitFailure::definite("open generation index table", error)
                })?;
                apply_set_delta(&mut indexes, &index_delta).map_err(CommitFailure::Definite)?;
            }
        }
        if prepared.layout.format >= PRODUCTION_STORAGE_FORMAT_VERSION {
            transaction
                .open_table(MAINTENANCE_GENERATION)
                .map_err(|error| {
                    CommitFailure::definite("open maintenance generation table", error)
                })?;
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

    pub(crate) fn check_integrity(
        &mut self,
    ) -> Result<(bool, Database, ReceiptMap, StorageCheckProfile)> {
        let total_started = Instant::now();
        let backend_started = Instant::now();
        let backend_clean = self.database.check_integrity().map_err(|error| {
            if matches!(error, redb::DatabaseError::TransactionInProgress) {
                Error::new(
                    "E_BUSY",
                    "redb integrity maintenance requires all read snapshots to be released",
                )
            } else {
                storage_error("check redb integrity", error)
            }
        })?;
        let backend_micros = elapsed_micros(backend_started);
        if self.committed.layout.format >= LEGACY_BOUNDED_STORAGE_FORMAT_VERSION {
            let logical_started = Instant::now();
            let (database, receipts, committed, source, _) = self.load_bounded_view()?;
            drop(source);
            let mut profile =
                self.validate_bounded_integrity_for(&database, committed.generation.active)?;
            profile.backend_micros = backend_micros;
            profile.logical_micros = elapsed_micros(logical_started);
            profile.total_micros = elapsed_micros(total_started);
            self.committed = committed;
            return Ok((backend_clean, database.as_ref().clone(), receipts, profile));
        }
        let logical_started = Instant::now();
        let (database, receipts, committed, open_profile) = self.load()?;
        self.committed = DurableHead::from_prepared(&committed);
        Ok((
            backend_clean,
            database,
            receipts,
            StorageCheckProfile {
                total_micros: elapsed_micros(total_started),
                backend_micros,
                logical_micros: elapsed_micros(logical_started),
                rows_checked: open_profile.row_entries,
                row_bytes: open_profile.row_bytes,
                index_entries_checked: open_profile.index_entries,
                index_key_bytes: open_profile.index_key_bytes,
                point_lookups: 0,
                working_peak_bytes: usize::try_from(
                    open_profile
                        .row_bytes
                        .saturating_add(open_profile.index_key_bytes),
                )
                .unwrap_or(usize::MAX),
                bounded: false,
            },
        ))
    }

    fn validate_bounded_integrity_for(
        &self,
        metadata: &Database,
        generation: GenerationRef,
    ) -> Result<StorageCheckProfile> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin integrity read transaction", error))?;
        let rows = match generation {
            GenerationRef::Legacy0 => transaction
                .open_table(ROWS)
                .map_err(|error| storage_error("open rows table", error))?,
            GenerationRef::Generated(_) => transaction
                .open_table(GENERATION_ROWS)
                .map_err(|error| storage_error("open generation rows table", error))?,
        };
        let indexes = match generation {
            GenerationRef::Legacy0 => transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| storage_error("open secondary index table", error))?,
            GenerationRef::Generated(_) => transaction
                .open_table(GENERATION_INDEX)
                .map_err(|error| storage_error("open generation index table", error))?,
        };

        let mut tables = BTreeMap::<u64, DurableTable>::new();
        let mut definitions = BTreeMap::<u64, (DurableTable, IndexDefinition)>::new();
        let mut by_table = BTreeMap::<u64, Vec<IndexDefinition>>::new();
        let catalog_entries = metadata.durable_catalog_entries();
        for entry in &catalog_entries {
            if let DurableCatalogEntry::Table(table) = entry {
                tables.insert(table.id, table.clone());
            }
        }
        for entry in catalog_entries {
            if let DurableCatalogEntry::Index { table, definition } = entry {
                let durable_table = tables
                    .get(&definition.table_id)
                    .filter(|candidate| candidate.name == table)
                    .cloned()
                    .ok_or_else(|| {
                        Error::new("E_STORAGE", "index references an unknown durable table")
                    })?;
                by_table
                    .entry(definition.table_id)
                    .or_default()
                    .push(definition.clone());
                definitions.insert(definition.id, (durable_table, definition));
            }
        }

        let mut profile = StorageCheckProfile {
            bounded: true,
            ..StorageCheckProfile::default()
        };
        let mut expected_index_entries = 0_usize;
        let (row_lower, row_upper) = generation_scan_bounds(generation)?;
        for entry in rows
            .range::<&[u8]>((borrowed_bound(&row_lower), borrowed_bound(&row_upper)))
            .map_err(|error| storage_error("iterate rows for integrity check", error))?
        {
            let (key, value) =
                entry.map_err(|error| storage_error("read row for integrity check", error))?;
            let logical_key = logical_generation_key(generation, key.value())?;
            let (table_id, row_id) = decode_row_key(logical_key)?;
            let table = tables.get(&table_id).ok_or_else(|| {
                Error::new(
                    "E_STORAGE",
                    format!("row references unknown table ID {table_id}"),
                )
            })?;
            let row = metadata.decode_source_row(&table.name, row_id, value.value())?;
            profile.rows_checked = profile.rows_checked.saturating_add(1);
            profile.row_bytes = profile
                .row_bytes
                .saturating_add(u64::try_from(value.value().len()).unwrap_or(u64::MAX));
            let mut row_working = value.value().len();
            if let Some(table_indexes) = by_table.get(&table_id) {
                for definition in table_indexes {
                    let indexed = metadata.source_index_value(&table.name, definition, &row)?;
                    let expected = encode_index_key(
                        metadata,
                        definition.id,
                        &indexed,
                        row_id,
                        PRODUCTION_INDEX_KEY_VERSION,
                    )?;
                    row_working = row_working.saturating_add(expected.len());
                    profile.point_lookups = profile.point_lookups.saturating_add(1);
                    let physical_expected = physical_generation_key(generation, &expected)?;
                    if indexes
                        .get(physical_expected.as_slice())
                        .map_err(|error| storage_error("lookup expected secondary index", error))?
                        .is_none()
                    {
                        return Err(Error::new(
                            "E_STORAGE",
                            format!(
                                "durable secondary indexes do not match the stored rows and catalog: row {row_id} is missing from index '{} ({})'",
                                table.name,
                                definition.display_shape()
                            ),
                        ));
                    }
                    expected_index_entries =
                        expected_index_entries.checked_add(1).ok_or_else(|| {
                            Error::new("E_STORAGE", "secondary index cardinality overflow")
                        })?;
                }
            }
            profile.working_peak_bytes = profile.working_peak_bytes.max(row_working);
        }

        let mut previous_unique_prefix: Option<Vec<u8>> = None;
        let (index_lower, index_upper) = generation_scan_bounds(generation)?;
        for entry in indexes
            .range::<&[u8]>((borrowed_bound(&index_lower), borrowed_bound(&index_upper)))
            .map_err(|error| storage_error("iterate indexes for integrity check", error))?
        {
            let (key, _) =
                entry.map_err(|error| storage_error("read index for integrity check", error))?;
            let physical_key = key.value();
            let key = logical_generation_key(generation, physical_key)?;
            validate_index_key(key, PRODUCTION_INDEX_KEY_VERSION)?;
            let index_id = u64::from_be_bytes(key[6..14].try_into().unwrap());
            let component_count = key[14] as usize;
            let row_id = u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap());
            let (table, definition) = definitions.get(&index_id).ok_or_else(|| {
                Error::new(
                    "E_STORAGE",
                    format!("secondary index references unknown index ID {index_id}"),
                )
            })?;
            if component_count != definition.effective_components().len() {
                return Err(Error::new(
                    "E_STORAGE",
                    "secondary index component count does not match its catalog definition",
                ));
            }
            let row_key = encode_row_key(table.id, row_id);
            let physical_row_key = physical_generation_key(generation, &row_key)?;
            profile.point_lookups = profile.point_lookups.saturating_add(1);
            let stored_row = rows
                .get(physical_row_key.as_slice())
                .map_err(|error| storage_error("lookup indexed row", error))?
                .ok_or_else(|| {
                    Error::new("E_STORAGE", "secondary index references a missing row")
                })?;
            let row = metadata.decode_source_row(&table.name, row_id, stored_row.value())?;
            let indexed = metadata.source_index_value(&table.name, definition, &row)?;
            let expected = encode_index_key(
                metadata,
                definition.id,
                &indexed,
                row_id,
                PRODUCTION_INDEX_KEY_VERSION,
            )?;
            if key != expected.as_slice() {
                return Err(Error::new(
                    "E_STORAGE",
                    format!(
                        "secondary index '{} ({})' does not match row {row_id}",
                        table.name,
                        definition.display_shape()
                    ),
                ));
            }
            let unique = definition.kind.is_unique()
                || definition.is_primary_index(table.primary_key.as_deref());
            let prefix = &key[..key.len() - 8];
            if unique && previous_unique_prefix.as_deref() == Some(prefix) {
                return Err(Error::new(
                    "E_STORAGE",
                    format!(
                        "unique index '{} ({})' contains duplicate values",
                        table.name,
                        definition.display_shape()
                    ),
                ));
            }
            previous_unique_prefix = unique.then(|| prefix.to_vec());
            profile.index_entries_checked = profile.index_entries_checked.saturating_add(1);
            profile.index_key_bytes = profile
                .index_key_bytes
                .saturating_add(u64::try_from(physical_key.len()).unwrap_or(u64::MAX));
            profile.working_peak_bytes = profile.working_peak_bytes.max(
                physical_key
                    .len()
                    .saturating_add(stored_row.value().len())
                    .saturating_add(physical_row_key.len())
                    .saturating_add(expected.len()),
            );
        }
        if profile.index_entries_checked != expected_index_entries {
            return Err(Error::new(
                "E_STORAGE",
                format!(
                    "secondary index cardinality mismatch: expected {expected_index_entries}, found {}",
                    profile.index_entries_checked
                ),
            ));
        }
        Ok(profile)
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
        let (meta, layout, generation) = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open meta table", error))?;
            read_meta(&table)?
        };
        profile.meta_micros = elapsed_micros(meta_started);
        if !matches!(
            layout.format,
            LEGACY_BOUNDED_STORAGE_FORMAT_VERSION | PRODUCTION_STORAGE_FORMAT_VERSION
        ) {
            return Err(Error::new(
                "E_STORAGE",
                "bounded reads require storage format 5 or 6",
            ));
        }
        validate_generation_state(&transaction, &meta, layout, generation)?;

        let catalog_started = Instant::now();
        let (entries, catalog_canonical) = match generation.active {
            GenerationRef::Legacy0 => {
                let table = transaction
                    .open_table(CATALOG)
                    .map_err(|error| storage_error("open catalog table", error))?;
                read_catalog_table(&table, layout, None)?
            }
            GenerationRef::Generated(id) => {
                let table = transaction
                    .open_table(GENERATION_CATALOG)
                    .map_err(|error| storage_error("open generation catalog table", error))?;
                read_catalog_table(&table, layout, Some(id))?
            }
        };
        profile.catalog_micros = elapsed_micros(catalog_started);
        profile.catalog_entries = entries.len();

        // Opening the physical tables proves that the expected Legacy0 table
        // definitions exist without traversing their entries.
        let rows_started = Instant::now();
        match generation.active {
            GenerationRef::Legacy0 => {
                transaction
                    .open_table(ROWS)
                    .map_err(|error| storage_error("open rows table", error))?;
            }
            GenerationRef::Generated(_) => {
                transaction
                    .open_table(GENERATION_ROWS)
                    .map_err(|error| storage_error("open generation rows table", error))?;
            }
        }
        profile.rows_micros = elapsed_micros(rows_started);
        let indexes_started = Instant::now();
        match generation.active {
            GenerationRef::Legacy0 => {
                transaction
                    .open_table(SECONDARY_INDEX)
                    .map_err(|error| storage_error("open secondary index table", error))?;
            }
            GenerationRef::Generated(_) => {
                transaction
                    .open_table(GENERATION_INDEX)
                    .map_err(|error| storage_error("open generation index table", error))?;
            }
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
            generation,
        };
        let source: Arc<dyn TypedRowSource> = Arc::new(RedbReadSource::new(
            transaction,
            metadata.clone(),
            generation.active,
        ));
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
        let (meta, layout, generation) = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open meta table", error))?;
            read_meta(&table)?
        };
        profile.meta_micros = elapsed_micros(meta_started);
        let catalog_started = Instant::now();
        validate_generation_state(&transaction, &meta, layout, generation)?;
        let (entries, stored_catalog) = match generation.active {
            GenerationRef::Legacy0 => {
                let table = transaction
                    .open_table(CATALOG)
                    .map_err(|error| storage_error("open catalog table", error))?;
                read_complete_catalog_table(&table, layout, None)?
            }
            GenerationRef::Generated(id) => {
                let table = transaction
                    .open_table(GENERATION_CATALOG)
                    .map_err(|error| storage_error("open generation catalog table", error))?;
                read_complete_catalog_table(&table, layout, Some(id))?
            }
        };
        profile.catalog_micros = elapsed_micros(catalog_started);
        profile.catalog_entries = stored_catalog.len();
        let rows_started = Instant::now();
        let (rows, stored_rows) = match generation.active {
            GenerationRef::Legacy0 => {
                let table = transaction
                    .open_table(ROWS)
                    .map_err(|error| storage_error("open rows table", error))?;
                read_complete_rows_table(&table, None)?
            }
            GenerationRef::Generated(id) => {
                let table = transaction
                    .open_table(GENERATION_ROWS)
                    .map_err(|error| storage_error("open generation rows table", error))?;
                read_complete_rows_table(&table, Some(id))?
            }
        };
        profile.rows_micros = elapsed_micros(rows_started);
        profile.row_entries = stored_rows.len();
        profile.row_bytes = map_bytes(&stored_rows);
        let indexes_started = Instant::now();
        let stored_indexes = match generation.active {
            GenerationRef::Legacy0 => {
                let table = transaction
                    .open_table(SECONDARY_INDEX)
                    .map_err(|error| storage_error("open secondary index table", error))?;
                read_complete_index_table(&table, layout, None)?
            }
            GenerationRef::Generated(id) => {
                let table = transaction
                    .open_table(GENERATION_INDEX)
                    .map_err(|error| storage_error("open generation index table", error))?;
                read_complete_index_table(&table, layout, Some(id))?
            }
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
            generation,
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
    generation: GenerationState,
    meta: DurableMeta,
    catalog: BTreeMap<Vec<u8>, Vec<u8>>,
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    secondary_indexes: BTreeSet<Vec<u8>>,
    migrations: BTreeMap<u64, Vec<u8>>,
    receipts: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl PreparedState {
    fn new(database: &Database, receipts: &ReceiptMap, layout: StorageLayout) -> Result<Self> {
        let generation = if layout.format >= PRODUCTION_STORAGE_FORMAT_VERSION {
            GenerationState::initial()
        } else {
            GenerationState::legacy()
        };
        Self::new_in_generation(database, receipts, layout, generation)
    }

    fn new_in_generation(
        database: &Database,
        receipts: &ReceiptMap,
        layout: StorageLayout,
        generation: GenerationState,
    ) -> Result<Self> {
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
            generation,
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
    generation: GenerationState,
    expected_meta: Option<(DurableMeta, StorageLayout, GenerationState)>,
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
            generation: committed.generation,
            expected_meta: Some((
                committed.meta.clone(),
                committed.layout,
                committed.generation,
            )),
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
            generation: next.generation,
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

fn generation_bytes_delta(delta: &BytesDelta, generation: u64) -> Result<BytesDelta> {
    Ok(BytesDelta {
        deletes: delta
            .deletes
            .iter()
            .map(|entry| {
                Ok(BytesDelete {
                    key: encode_generation_key(generation, &entry.key)?,
                    expected: entry.expected.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        writes: delta
            .writes
            .iter()
            .map(|entry| {
                Ok(BytesWrite {
                    key: encode_generation_key(generation, &entry.key)?,
                    expected: entry.expected.clone(),
                    value: entry.value.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn generation_set_delta(delta: &SetDelta, generation: u64) -> Result<SetDelta> {
    Ok(SetDelta {
        deletes: delta
            .deletes
            .iter()
            .map(|key| encode_generation_key(generation, key))
            .collect::<Result<Vec<_>>>()?,
        inserts: delta
            .inserts
            .iter()
            .map(|key| encode_generation_key(generation, key))
            .collect::<Result<Vec<_>>>()?,
    })
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
    expected: Option<&(DurableMeta, StorageLayout, GenerationState)>,
    meta: &DurableMeta,
    layout: StorageLayout,
    generation: GenerationState,
) -> Result<()> {
    if let Some((expected_meta, expected_layout, expected_generation)) = expected {
        for (key, expected_value) in
            meta_entries(expected_meta, *expected_layout, *expected_generation)
        {
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
    for (key, value) in meta_entries(meta, layout, generation) {
        table
            .insert(key, value.as_slice())
            .map_err(|error| storage_error("write meta entry", error))?;
    }
    Ok(())
}

fn meta_entries(
    meta: &DurableMeta,
    layout: StorageLayout,
    generation: GenerationState,
) -> Vec<(&'static str, Vec<u8>)> {
    let mut entries = vec![
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
    ];
    if layout.format >= PRODUCTION_STORAGE_FORMAT_VERSION {
        entries.extend([
            (
                MAINTENANCE_CODEC_KEY,
                layout.maintenance.to_be_bytes().to_vec(),
            ),
            (
                ACTIVE_GENERATION_KEY,
                generation.active.encoded().to_be_bytes().to_vec(),
            ),
            (
                NEXT_GENERATION_ID_KEY,
                generation.next_id.to_be_bytes().to_vec(),
            ),
        ]);
    }
    entries
}

fn read_meta(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<(DurableMeta, StorageLayout, GenerationState)> {
    let format_version = u32::from_be_bytes(read_fixed::<4>(table, FORMAT_KEY)?);
    if !matches!(
        format_version,
        LEGACY_STORAGE_FORMAT_VERSION
            | RECEIPT_STORAGE_FORMAT_VERSION
            | CURSOR_STORAGE_FORMAT_VERSION
            | SCALAR_STORAGE_FORMAT_VERSION
            | LEGACY_BOUNDED_STORAGE_FORMAT_VERSION
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
    let maintenance = if format_version >= PRODUCTION_STORAGE_FORMAT_VERSION {
        u16::from_be_bytes(read_fixed::<2>(table, MAINTENANCE_CODEC_KEY)?)
    } else {
        0
    };
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
    if maintenance != expected.maintenance {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {MAINTENANCE_CODEC_KEY}"),
        ));
    }
    let layout = StorageLayout {
        format: format_version,
        catalog: catalog_version,
        value,
        index,
        migration,
        receipt,
        maintenance,
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
    let generation = if format_version >= PRODUCTION_STORAGE_FORMAT_VERSION {
        let active = GenerationRef::from_encoded(u64::from_be_bytes(read_fixed::<8>(
            table,
            ACTIVE_GENERATION_KEY,
        )?));
        let next_id = u64::from_be_bytes(read_fixed::<8>(table, NEXT_GENERATION_ID_KEY)?);
        if next_id == 0 || matches!(active, GenerationRef::Generated(id) if id >= next_id) {
            return Err(Error::new(
                "E_STORAGE",
                "active/next generation metadata is contradictory",
            ));
        }
        GenerationState { active, next_id }
    } else {
        GenerationState::legacy()
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
        generation,
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

fn generation_bounds(generation: u64) -> Result<OwnedKeyBounds> {
    let prefix = generation_prefix(generation)?;
    let upper = prefix_successor_bytes(&prefix)
        .map(Bound::Excluded)
        .ok_or_else(|| Error::new("E_STORAGE", "generation key prefix has no successor"))?;
    Ok((Bound::Included(prefix), upper))
}

fn generation_scan_bounds(generation: GenerationRef) -> Result<OwnedKeyBounds> {
    match generation {
        GenerationRef::Legacy0 => Ok((Bound::Unbounded, Bound::Unbounded)),
        GenerationRef::Generated(id) => generation_bounds(id),
    }
}

fn physical_generation_key(generation: GenerationRef, logical: &[u8]) -> Result<Vec<u8>> {
    match generation {
        GenerationRef::Legacy0 => Ok(logical.to_vec()),
        GenerationRef::Generated(id) => encode_generation_key(id, logical),
    }
}

fn logical_generation_key(generation: GenerationRef, physical: &[u8]) -> Result<&[u8]> {
    match generation {
        GenerationRef::Legacy0 => Ok(physical),
        GenerationRef::Generated(id) => decode_generation_key(physical, id),
    }
}

fn read_catalog_table(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    layout: StorageLayout,
    generation: Option<u64>,
) -> Result<(Vec<DurableCatalogEntry>, bool)> {
    let mut entries = Vec::new();
    let mut canonical = true;
    if let Some(generation) = generation {
        let (lower, upper) = generation_bounds(generation)?;
        let bounds = (borrowed_bound(&lower), borrowed_bound(&upper));
        for entry in table
            .range::<&[u8]>(bounds)
            .map_err(|error| storage_error("iterate generation catalog table", error))?
        {
            let (key, value) =
                entry.map_err(|error| storage_error("read generation catalog entry", error))?;
            let key = decode_generation_key(key.value(), generation)?;
            let value = value.value();
            entries.push(decode_catalog_entry(key, value, layout.catalog)?);
            canonical &= catalog_value_is_canonical(value, layout.catalog);
        }
    } else {
        for entry in table
            .iter()
            .map_err(|error| storage_error("iterate catalog table", error))?
        {
            let (key, value) = entry.map_err(|error| storage_error("read catalog entry", error))?;
            let value = value.value();
            entries.push(decode_catalog_entry(key.value(), value, layout.catalog)?);
            canonical &= catalog_value_is_canonical(value, layout.catalog);
        }
    }
    Ok((entries, canonical))
}

fn read_complete_catalog_table(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    layout: StorageLayout,
    generation: Option<u64>,
) -> Result<StoredCatalog> {
    let mut entries = Vec::new();
    let mut stored = BTreeMap::new();
    if let Some(generation) = generation {
        let (lower, upper) = generation_bounds(generation)?;
        let bounds = (borrowed_bound(&lower), borrowed_bound(&upper));
        for entry in table
            .range::<&[u8]>(bounds)
            .map_err(|error| storage_error("iterate generation catalog table", error))?
        {
            let (key, value) =
                entry.map_err(|error| storage_error("read generation catalog entry", error))?;
            let key = decode_generation_key(key.value(), generation)?.to_vec();
            let value = value.value().to_vec();
            entries.push(decode_catalog_entry(&key, &value, layout.catalog)?);
            stored.insert(key, value);
        }
    } else {
        for entry in table
            .iter()
            .map_err(|error| storage_error("iterate catalog table", error))?
        {
            let (key, value) = entry.map_err(|error| storage_error("read catalog entry", error))?;
            let key = key.value().to_vec();
            let value = value.value().to_vec();
            entries.push(decode_catalog_entry(&key, &value, layout.catalog)?);
            stored.insert(key, value);
        }
    }
    Ok((entries, stored))
}

type StoredRows = (Vec<(u64, RowId, Vec<u8>)>, BTreeMap<Vec<u8>, Vec<u8>>);

fn read_complete_rows_table(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    generation: Option<u64>,
) -> Result<StoredRows> {
    let mut rows = Vec::new();
    let mut stored = BTreeMap::new();
    if let Some(generation) = generation {
        let (lower, upper) = generation_bounds(generation)?;
        let bounds = (borrowed_bound(&lower), borrowed_bound(&upper));
        for entry in table
            .range::<&[u8]>(bounds)
            .map_err(|error| storage_error("iterate generation rows table", error))?
        {
            let (key, value) =
                entry.map_err(|error| storage_error("read generation row", error))?;
            let key = decode_generation_key(key.value(), generation)?.to_vec();
            let value = value.value().to_vec();
            let (table_id, row_id) = decode_row_key(&key)?;
            rows.push((table_id, row_id, value.clone()));
            stored.insert(key, value);
        }
    } else {
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
    }
    Ok((rows, stored))
}

fn read_complete_index_table(
    table: &impl ReadableTable<&'static [u8], u8>,
    layout: StorageLayout,
    generation: Option<u64>,
) -> Result<BTreeSet<Vec<u8>>> {
    let mut keys = BTreeSet::new();
    if let Some(generation) = generation {
        let (lower, upper) = generation_bounds(generation)?;
        let bounds = (borrowed_bound(&lower), borrowed_bound(&upper));
        for entry in table
            .range::<&[u8]>(bounds)
            .map_err(|error| storage_error("iterate generation index table", error))?
        {
            let (key, _) =
                entry.map_err(|error| storage_error("read generation index entry", error))?;
            let key = decode_generation_key(key.value(), generation)?.to_vec();
            validate_index_key(&key, layout.index)?;
            keys.insert(key);
        }
    } else {
        for entry in table
            .iter()
            .map_err(|error| storage_error("iterate secondary index table", error))?
        {
            let (key, _) =
                entry.map_err(|error| storage_error("read secondary index entry", error))?;
            validate_index_key(key.value(), layout.index)?;
            keys.insert(key.value().to_vec());
        }
    }
    Ok(keys)
}

fn catalog_value_is_canonical(value: &[u8], version: u16) -> bool {
    value
        .get(4..6)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes)
        == Some(version)
}

fn encode_maintenance_manifest(manifest: &MaintenanceManifest) -> Result<Vec<u8>> {
    let mut value = Vec::from(MAINTENANCE_MAGIC.as_slice());
    value.extend_from_slice(&MAINTENANCE_CODEC_VERSION.to_be_bytes());
    value.extend(serde_json::to_vec(manifest).map_err(|error| {
        Error::new(
            "E_STORAGE",
            format!("encode maintenance generation manifest: {error}"),
        )
    })?);
    Ok(value)
}

fn decode_maintenance_manifest(value: &[u8]) -> Result<MaintenanceManifest> {
    if value.len() < 6 || &value[..4] != MAINTENANCE_MAGIC {
        return Err(Error::new(
            "E_STORAGE",
            "invalid maintenance generation manifest codec magic",
        ));
    }
    let version = u16::from_be_bytes(value[4..6].try_into().unwrap());
    if version != MAINTENANCE_CODEC_VERSION {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported maintenance generation manifest codec version {version}"),
        ));
    }
    serde_json::from_slice(&value[6..]).map_err(|error| {
        Error::new(
            "E_STORAGE",
            format!("decode maintenance generation manifest: {error}"),
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GenerationSummary {
    catalog_digest: String,
    rows: u64,
    indexes: u64,
    logical_bytes: u64,
    rolling_digest: String,
}

fn read_maintenance_manifest(
    transaction: &redb::ReadTransaction,
) -> Result<Option<(u64, MaintenanceManifest)>> {
    let table = transaction
        .open_table(MAINTENANCE_GENERATION)
        .map_err(|error| storage_error("open maintenance generation table", error))?;
    let mut found = None;
    for entry in table
        .iter()
        .map_err(|error| storage_error("iterate maintenance generation table", error))?
    {
        let (id, value) =
            entry.map_err(|error| storage_error("read maintenance generation", error))?;
        if found.is_some() {
            return Err(Error::new(
                "E_STORAGE",
                "multiple maintenance generation manifests are not supported",
            ));
        }
        found = Some((id.value(), decode_maintenance_manifest(value.value())?));
    }
    Ok(found)
}

fn verify_manifest_file(manifest: &MaintenanceManifest, file: &MigrationFile) -> Result<()> {
    if manifest.migration_id != file.id
        || manifest.migration_parent != file.parent
        || manifest.migration_checksum != file.checksum
        || manifest.executor_version != MAINTENANCE_EXECUTOR_VERSION
    {
        return Err(Error::new(
            "E_MAINTENANCE_CONFLICT",
            "migration file or executor does not match the durable maintenance checkpoint",
        ));
    }
    Ok(())
}

fn verify_maintenance_file(info: &MaintenanceInfo, file: &MigrationFile) -> Result<()> {
    if info.migration_id != file.id || info.migration_checksum != file.checksum {
        return Err(Error::new(
            "E_MAINTENANCE_CONFLICT",
            "migration file does not match the durable maintenance checkpoint",
        ));
    }
    Ok(())
}

fn maintenance_time_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::new("E_TIME", error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| Error::new("E_LIMIT", "maintenance timestamp exceeds u64"))
}

fn digest_bytes_map(values: &BTreeMap<Vec<u8>, Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    for (key, value) in values {
        digest.update(u64::try_from(key.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(key);
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(value);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn empty_rolling_digest() -> String {
    format!("sha256:{:x}", Sha256::digest([]))
}

fn advance_rolling_digest(current: &str, rows: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<String> {
    let mut rolling = current.to_owned();
    for (key, value) in rows {
        rolling = advance_rolling_digest_entry(&rolling, key, value)?;
    }
    Ok(rolling)
}

fn advance_rolling_digest_entry(current: &str, key: &[u8], value: &[u8]) -> Result<String> {
    if !current.starts_with("sha256:") {
        return Err(Error::new(
            "E_STORAGE",
            "maintenance rolling digest is not canonical",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(current.as_bytes());
    digest.update(u64::try_from(key.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(key);
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn delete_bytes_entries(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    generation: GenerationRef,
    limit: usize,
) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }
    let (lower, upper) = generation_scan_bounds(generation)?;
    let keys = table
        .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
        .map_err(|error| storage_error("scan generation entries for reclaim", error))?
        .take(limit)
        .map(|entry| {
            entry
                .map(|(key, _)| key.value().to_vec())
                .map_err(|error| storage_error("read generation entry for reclaim", error))
        })
        .collect::<Result<Vec<_>>>()?;
    for key in &keys {
        table
            .remove(key.as_slice())
            .map_err(|error| storage_error("delete generation entry", error))?;
    }
    Ok(keys.len())
}

fn delete_set_entries(
    table: &mut redb::Table<'_, &[u8], u8>,
    generation: GenerationRef,
    limit: usize,
) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }
    let (lower, upper) = generation_scan_bounds(generation)?;
    let keys = table
        .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
        .map_err(|error| storage_error("scan generation indexes for reclaim", error))?
        .take(limit)
        .map(|entry| {
            entry
                .map(|(key, _)| key.value().to_vec())
                .map_err(|error| storage_error("read generation index for reclaim", error))
        })
        .collect::<Result<Vec<_>>>()?;
    for key in &keys {
        table
            .remove(key.as_slice())
            .map_err(|error| storage_error("delete generation index", error))?;
    }
    Ok(keys.len())
}

fn generation_tables_empty(
    transaction: &redb::WriteTransaction,
    generation: GenerationRef,
) -> Result<bool> {
    let (lower, upper) = generation_scan_bounds(generation)?;
    let catalog_empty = match generation {
        GenerationRef::Legacy0 => transaction
            .open_table(CATALOG)
            .map_err(|error| storage_error("open reclaim catalog", error))?
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("check reclaim catalog", error))?
            .next()
            .is_none(),
        GenerationRef::Generated(_) => transaction
            .open_table(GENERATION_CATALOG)
            .map_err(|error| storage_error("open reclaim catalog", error))?
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("check reclaim catalog", error))?
            .next()
            .is_none(),
    };
    let rows_empty = match generation {
        GenerationRef::Legacy0 => transaction
            .open_table(ROWS)
            .map_err(|error| storage_error("open reclaim rows", error))?
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("check reclaim rows", error))?
            .next()
            .is_none(),
        GenerationRef::Generated(_) => transaction
            .open_table(GENERATION_ROWS)
            .map_err(|error| storage_error("open reclaim rows", error))?
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("check reclaim rows", error))?
            .next()
            .is_none(),
    };
    let indexes_empty = match generation {
        GenerationRef::Legacy0 => transaction
            .open_table(SECONDARY_INDEX)
            .map_err(|error| storage_error("open reclaim indexes", error))?
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("check reclaim indexes", error))?
            .next()
            .is_none(),
        GenerationRef::Generated(_) => transaction
            .open_table(GENERATION_INDEX)
            .map_err(|error| storage_error("open reclaim indexes", error))?
            .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
            .map_err(|error| storage_error("check reclaim indexes", error))?
            .next()
            .is_none(),
    };
    Ok(catalog_empty && rows_empty && indexes_empty)
}

fn validate_generation_state(
    transaction: &redb::ReadTransaction,
    meta: &DurableMeta,
    layout: StorageLayout,
    generation: GenerationState,
) -> Result<()> {
    if layout.format < PRODUCTION_STORAGE_FORMAT_VERSION {
        if generation != GenerationState::legacy() {
            return Err(Error::new(
                "E_STORAGE",
                "pre-generation storage has generation metadata",
            ));
        }
        return Ok(());
    }
    if generation.next_id == 0
        || matches!(generation.active, GenerationRef::Generated(id) if id >= generation.next_id)
    {
        return Err(Error::new(
            "E_STORAGE",
            "active generation is not below the next monotonic generation ID",
        ));
    }
    transaction
        .open_table(GENERATION_CATALOG)
        .map_err(|error| storage_error("open generation catalog table", error))?;
    transaction
        .open_table(GENERATION_ROWS)
        .map_err(|error| storage_error("open generation rows table", error))?;
    transaction
        .open_table(GENERATION_INDEX)
        .map_err(|error| storage_error("open generation index table", error))?;
    let manifests = transaction
        .open_table(MAINTENANCE_GENERATION)
        .map_err(|error| storage_error("open maintenance generation table", error))?;
    let mut unfinished = 0_usize;
    let mut instance_mismatch = false;
    for entry in manifests
        .iter()
        .map_err(|error| storage_error("iterate maintenance generation table", error))?
    {
        let (id, value) =
            entry.map_err(|error| storage_error("read maintenance generation", error))?;
        let id = id.value();
        let manifest = decode_maintenance_manifest(value.value())?;
        if id == 0 || manifest.target != GenerationRef::Generated(id) || id >= generation.next_id {
            return Err(Error::new(
                "E_STORAGE",
                "maintenance manifest contradicts its generation ID allocation",
            ));
        }
        if let GenerationRef::Generated(source) = manifest.source
            && source >= generation.next_id
        {
            return Err(Error::new(
                "E_STORAGE",
                "maintenance manifest source generation was never allocated",
            ));
        }
        instance_mismatch |= manifest.database_instance != meta.cursor_instance_id;
        match manifest.state {
            MaintenanceState::Building | MaintenanceState::Ready | MaintenanceState::Aborting => {
                unfinished = unfinished.saturating_add(1);
                if manifest.source != generation.active || manifest.target == generation.active {
                    return Err(Error::new(
                        "E_STORAGE",
                        "unfinished maintenance manifest contradicts the active generation",
                    ));
                }
            }
            MaintenanceState::Reclaimable => {
                if manifest.target != generation.active || manifest.source == generation.active {
                    return Err(Error::new(
                        "E_STORAGE",
                        "reclaimable maintenance manifest contradicts the active generation",
                    ));
                }
            }
        }
    }
    if unfinished > 1 {
        return Err(Error::new(
            "E_STORAGE",
            "multiple unfinished maintenance generations are not supported",
        ));
    }
    if instance_mismatch {
        return Err(Error::new(
            "E_STORAGE",
            "maintenance manifest belongs to a different database instance",
        ));
    }
    Ok(())
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

fn encode_generation_key(generation: u64, inner: &[u8]) -> Result<Vec<u8>> {
    if generation == 0 {
        return Err(Error::new(
            "E_STORAGE",
            "generated key requires a nonzero generation ID",
        ));
    }
    let mut key = Vec::with_capacity(14_usize.saturating_add(inner.len()));
    key.extend_from_slice(GENERATION_KEY_MAGIC);
    key.extend_from_slice(&GENERATION_KEY_CODEC_VERSION.to_be_bytes());
    key.extend_from_slice(&generation.to_be_bytes());
    key.extend_from_slice(inner);
    Ok(key)
}

fn decode_generation_key(key: &[u8], expected_generation: u64) -> Result<&[u8]> {
    if key.len() < 14 || &key[..4] != GENERATION_KEY_MAGIC {
        return Err(Error::new(
            "E_STORAGE",
            "invalid generation key codec magic",
        ));
    }
    let version = u16::from_be_bytes(key[4..6].try_into().unwrap());
    if version != GENERATION_KEY_CODEC_VERSION {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported generation key codec version {version}"),
        ));
    }
    let generation = u64::from_be_bytes(key[6..14].try_into().unwrap());
    if generation == 0 || generation != expected_generation {
        return Err(Error::new(
            "E_STORAGE",
            "generation key does not match the active generation",
        ));
    }
    Ok(&key[14..])
}

fn generation_prefix(generation: u64) -> Result<Vec<u8>> {
    encode_generation_key(generation, &[])
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

    #[test]
    fn generation_key_codec_has_stable_prefixes_and_rejects_malformed_input() {
        let encoded = encode_generation_key(1, &[0xaa, 0xbb]).unwrap();
        assert_eq!(
            encoded,
            [
                b'U', b'I', b'D', b'G', 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0xaa, 0xbb,
            ]
        );
        assert_eq!(decode_generation_key(&encoded, 1).unwrap(), [0xaa, 0xbb]);
        assert!(generation_prefix(1).unwrap() < generation_prefix(2).unwrap());
        let (lower, upper) = generation_bounds(1).unwrap();
        assert_eq!(lower, Bound::Included(generation_prefix(1).unwrap()));
        assert_eq!(upper, Bound::Excluded(generation_prefix(2).unwrap()));

        assert!(encode_generation_key(0, b"row").is_err());
        assert!(decode_generation_key(&encoded[..13], 1).is_err());
        let mut unknown_version = encoded.clone();
        unknown_version[5] = 2;
        assert!(decode_generation_key(&unknown_version, 1).is_err());
        assert!(decode_generation_key(&encoded, 2).is_err());
    }

    #[test]
    fn maintenance_manifest_codec_round_trips_and_rejects_unknown_versions() {
        let manifest = MaintenanceManifest {
            state: MaintenanceState::Building,
            source: GenerationRef::Legacy0,
            target: GenerationRef::Generated(1),
            database_instance: [7; 16],
            migration_id: "m0001".into(),
            migration_parent: None,
            migration_checksum: "sha256:test".into(),
            source_schema_revision: 0,
            source_schema_hash: "sha256:source".into(),
            source_sequence: 0,
            target_schema_revision: 1,
            target_schema_hash: "sha256:target".into(),
            target_catalog_digest: "sha256:catalog".into(),
            checkpoint: None,
            source_rows_seen: 0,
            target_rows_written: 0,
            index_entries_written: 0,
            logical_bytes: 0,
            rolling_digest: empty_rolling_digest(),
            executor_version: MAINTENANCE_EXECUTOR_VERSION,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let encoded = encode_maintenance_manifest(&manifest).unwrap();
        assert_eq!(decode_maintenance_manifest(&encoded).unwrap(), manifest);
        let mut unknown_version = encoded;
        unknown_version[5] = 2;
        assert!(decode_maintenance_manifest(&unknown_version).is_err());
    }

    #[test]
    fn maintenance_checkpoint_reopens_conflicts_and_aborts_without_reusing_generation() {
        let path = std::env::temp_dir().join(format!(
            "unionid-maintenance-{}-{}.redb",
            std::process::id(),
            maintenance_time_ms().unwrap()
        ));
        let empty = Database::default();
        let mut source_database = Database::default();
        let mut source = String::from(
            "type Item =\n  id int\n  value int\ntable items Item\n  key id\ninsert many items [",
        );
        for id in 0..1_100 {
            if id > 0 {
                source.push_str(", ");
            }
            source.push_str(&format!("{{id = {id}, value = {id}}}"));
        }
        source.push(']');
        execute(&mut source_database, &source);
        let file =
            MigrationFile::parse("migration m0001_enabled\n  add field Item.enabled bool = true\n")
                .unwrap();
        let target = source_database
            .migration_target(&file.id, &file.steps)
            .unwrap();
        {
            let (mut store, _, _, _, _) = RedbStore::open(&path).unwrap();
            store
                .commit(&empty, &source_database, &ReceiptMap::new())
                .unwrap();
            let (_, row_source) = store.committed_view(&source_database).unwrap();
            let started = store
                .start_maintenance(&source_database, &target, &file)
                .unwrap();
            assert_eq!(started.target_generation, 2);
            let mut cursor = row_source.scan_rows("items").unwrap();
            let mut observation = ExecutionObservation::default();
            let batch = cursor.next_batch(None, &mut observation).unwrap().unwrap();
            assert_eq!(batch.rows.len(), SOURCE_BATCH_MAX_ROWS);
            let target_batch = source_database
                .migrate_table_batch(&file.id, &file.steps, "items", &batch.rows)
                .unwrap();
            let table_id = match source_database
                .durable_table_catalog_entry("items")
                .unwrap()
            {
                DurableCatalogEntry::Table(table) => table.id,
                _ => unreachable!(),
            };
            let checkpoint = (table_id, batch.rows.last().unwrap().id);
            let progress = store
                .append_maintenance_batch(&file, &target_batch, checkpoint, batch.rows.len())
                .unwrap();
            assert_eq!(progress.source_rows_seen, SOURCE_BATCH_MAX_ROWS as u64);
            assert_eq!(progress.checkpoint, Some(checkpoint));
            drop(cursor);
            drop(row_source);
        }

        {
            let (mut store, _, _, _, _) = RedbStore::open(&path).unwrap();
            let progress = store.maintenance_info().unwrap().unwrap();
            assert_eq!(progress.state, MaintenanceState::Building);
            assert_eq!(progress.source_rows_seen, SOURCE_BATCH_MAX_ROWS as u64);
            let changed = MigrationFile::parse(
                "migration m0001_enabled\n  add field Item.enabled bool = false\n",
            )
            .unwrap();
            let error = store
                .append_maintenance_batch(&changed, &target, (1, 1), 1)
                .unwrap_err()
                .into_error();
            assert_eq!(error.code, "E_MAINTENANCE_CONFLICT");
        }

        {
            let mut engine = crate::Engine::open_redb(&path).unwrap();
            let progress = engine.introspection().maintenance.unwrap();
            assert_eq!(progress.phase, crate::MigrationMaintenancePhase::Building);
            assert_eq!(progress.source_rows_seen, SOURCE_BATCH_MAX_ROWS as u64);
            let timeout = engine
                .apply_migrations_until(std::slice::from_ref(&file), std::time::Instant::now())
                .unwrap_err();
            assert_eq!(timeout.code, "E_TIMEOUT");
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let control = crate::control::ExecutionControl::cancellable(
                std::time::Instant::now() + std::time::Duration::from_secs(1),
                cancelled,
                None,
            );
            let cancelled = engine
                .apply_migrations_controlled(std::slice::from_ref(&file), Some(&control))
                .unwrap_err();
            assert_eq!(cancelled.code, "E_CANCELLED");
            assert_eq!(
                engine.introspection().maintenance.unwrap().source_rows_seen,
                SOURCE_BATCH_MAX_ROWS as u64
            );
            let changed = MigrationFile::parse(
                "migration m0001_enabled\n  add field Item.enabled bool = false\n",
            )
            .unwrap();
            let conflict = engine.apply_migrations(&[changed]).unwrap_err();
            assert_eq!(conflict.code, "E_MAINTENANCE_CONFLICT");
            assert_eq!(
                engine.introspection().maintenance.unwrap().source_rows_seen,
                SOURCE_BATCH_MAX_ROWS as u64
            );
            let rows = engine.execute("from items | take 1");
            assert!(rows.ok, "{}", rows.message);
            let rejected = engine.execute("insert items {id = 2000, value = 2000}");
            assert_eq!(rejected.error.unwrap().code, "E_MAINTENANCE_REQUIRED");
            let applied = engine
                .apply_migrations(std::slice::from_ref(&file))
                .unwrap();
            assert_eq!(applied.applied, ["m0001_enabled"]);
            assert!(engine.introspection().maintenance.is_none());
            let rows = engine.execute("from items | filter enabled == true");
            assert!(rows.ok, "{}", rows.message);
            assert_eq!(rows.rows.len(), 1_100);
        }

        {
            let (mut store, loaded, _, _, _) = RedbStore::open(&path).unwrap();
            let next = MigrationFile::parse(
                "migration m0002_tag\n  parent m0001_enabled\n  add field Item.tag int = 0\n",
            )
            .unwrap();
            let target = loaded.migration_target(&next.id, &next.steps).unwrap();
            let restarted = store.start_maintenance(&loaded, &target, &next).unwrap();
            assert_eq!(restarted.target_generation, 3);
        }
        {
            let mut read_only = crate::Engine::open_redb_read_only(&path).unwrap();
            assert_eq!(read_only.abort_migration().unwrap_err().code, "E_READ_ONLY");
            assert_eq!(
                read_only.introspection().maintenance.unwrap().migration_id,
                "m0002_tag"
            );
        }
        {
            let mut engine = crate::Engine::open_redb(&path).unwrap();
            let aborted = engine.abort_migration().unwrap();
            assert_eq!(aborted.migration_id.as_deref(), Some("m0002_tag"));
        }
        {
            let (mut store, loaded, _, _, _) = RedbStore::open(&path).unwrap();
            let next = MigrationFile::parse(
                "migration m0002_tag\n  parent m0001_enabled\n  add field Item.tag int = 0\n",
            )
            .unwrap();
            let target = loaded.migration_target(&next.id, &next.steps).unwrap();
            let restarted = store.start_maintenance(&loaded, &target, &next).unwrap();
            assert_eq!(restarted.target_generation, 4);
            store.begin_maintenance_abort().unwrap();
            while !store.reclaim_maintenance_step().unwrap() {}
        }
        let _ = std::fs::remove_file(path);
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
