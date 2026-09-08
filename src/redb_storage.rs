//! Transactional redb storage for the durable Engine mode.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

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

const LEGACY_STORAGE_FORMAT_VERSION: u32 = 1;
const RECEIPT_STORAGE_FORMAT_VERSION: u32 = 2;
const CURSOR_STORAGE_FORMAT_VERSION: u32 = 3;
pub(crate) const PRODUCTION_STORAGE_FORMAT_VERSION: u32 = 4;
const CATALOG_CODEC_VERSION: u16 = 2;
const PRODUCTION_CATALOG_CODEC_VERSION: u16 = 3;
const LEGACY_CATALOG_CODEC_VERSION: u16 = 1;
const INDEX_KEY_VERSION: u16 = 1;
const PRODUCTION_INDEX_KEY_VERSION: u16 = 2;
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

    const fn supports_production_scalars(self) -> bool {
        self.format >= PRODUCTION_STORAGE_FORMAT_VERSION
    }
}

pub(crate) enum CommitFailure {
    Definite(Error),
    Uncertain(Error),
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
    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<(Self, Database, ReceiptMap)> {
        let path = resolve_path(path.into())?;
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::new("E_IO", format!("create database directory: {error}"))
            })?;
        }
        let fresh = std::fs::metadata(&path).map_or(true, |metadata| metadata.len() == 0);
        let database = RedbDatabase::create(&path).map_err(open_error)?;
        let empty = Database::default();
        let initial = PreparedState::new(&empty, &ReceiptMap::new(), StorageLayout::production())?;
        let mut store = Self {
            database,
            committed: DurableHead::from_prepared(&initial),
        };
        if fresh {
            let mut bootstrap = PreparedDelta::between(&initial, &initial);
            bootstrap.expected_meta = None;
            store
                .commit_prepared(&bootstrap)
                .map_err(CommitFailure::into_error)?;
        }
        let (loaded, receipts, committed) = store.load()?;
        let needs_cursor_upgrade = committed.layout.format < CURSOR_STORAGE_FORMAT_VERSION;
        store.committed = DurableHead::from_prepared(&committed);
        if needs_cursor_upgrade {
            store.committed.layout = StorageLayout::legacy(CURSOR_STORAGE_FORMAT_VERSION);
            store
                .commit(&loaded, &loaded, &receipts)
                .map_err(CommitFailure::into_error)?;
        }
        Ok((store, loaded, receipts))
    }

    pub(crate) fn supports_production_scalars(&self) -> bool {
        self.committed.layout.supports_production_scalars()
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
        if target != PRODUCTION_STORAGE_FORMAT_VERSION {
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
        self.committed.layout = StorageLayout::production();
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
    ) -> std::result::Result<(), CommitFailure> {
        validate_receipts(receipts, database.sequence).map_err(CommitFailure::Definite)?;
        let layout = self.committed.layout;
        let (_, _, previous) = self.load().map_err(CommitFailure::Definite)?;
        let next =
            PreparedState::new(database, receipts, layout).map_err(CommitFailure::Definite)?;
        let prepared = PreparedDelta::between(&previous, &next);
        self.commit_prepared(&prepared)?;
        self.committed = DurableHead::from_prepared(&next);
        Ok(())
    }

    pub(crate) fn commit_incremental(
        &mut self,
        previous: &Database,
        previous_receipts: &ReceiptMap,
        database: &Database,
        receipts: &ReceiptMap,
        write_set: &LogicalWriteSet,
    ) -> std::result::Result<(), CommitFailure> {
        if !self.committed.catalog_canonical {
            // Opening a legacy catalog can schedule an in-place codec
            // normalization for the next successful write. That maintenance
            // rewrite intentionally uses the full-state path once.
            return self.commit(previous, database, receipts);
        }
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
        self.commit_prepared(&prepared)?;
        self.committed.layout = prepared.layout;
        self.committed.meta = prepared.meta.clone();
        self.committed.catalog_canonical = true;
        Ok(())
    }

    fn commit_prepared(
        &mut self,
        prepared: &PreparedDelta,
    ) -> std::result::Result<(), CommitFailure> {
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
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit redb transaction", error))?;
        Ok(())
    }

    pub(crate) fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap)> {
        let backend_clean = self
            .database
            .check_integrity()
            .map_err(|error| storage_error("check redb integrity", error))?;
        let (database, receipts, committed) = self.load()?;
        self.committed = DurableHead::from_prepared(&committed);
        Ok((backend_clean, database, receipts))
    }

    fn load(&self) -> Result<(Database, ReceiptMap, PreparedState)> {
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
        let database = Database::from_durable(meta.clone(), entries, rows, migrations)?;
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
                encode_index_key(index_id, &value, row_id, layout.index)
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if stored_indexes != expected_indexes {
            return Err(Error::new(
                "E_STORAGE",
                "durable secondary indexes do not match the stored rows and catalog",
            ));
        }
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
        Ok((database, receipts, committed))
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
                encode_index_key(index_id, &value, row_id, layout.index)
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
            let key = encode_index_key(*index_id, &change.value, *row_id, layout.index)?;
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
            | PRODUCTION_STORAGE_FORMAT_VERSION
    ) {
        return Err(Error::new("E_STORAGE", format!("unsupported {FORMAT_KEY}")));
    }
    let catalog_version = u16::from_be_bytes(read_fixed::<2>(table, CATALOG_CODEC_KEY)?);
    let expected = if format_version >= PRODUCTION_STORAGE_FORMAT_VERSION {
        StorageLayout::production()
    } else {
        StorageLayout::legacy(format_version)
    };
    if !matches!(
        catalog_version,
        LEGACY_CATALOG_CODEC_VERSION | CATALOG_CODEC_VERSION | PRODUCTION_CATALOG_CODEC_VERSION
    ) || (format_version >= PRODUCTION_STORAGE_FORMAT_VERSION
        && catalog_version != expected.catalog)
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
        LEGACY_CATALOG_CODEC_VERSION | CATALOG_CODEC_VERSION | PRODUCTION_CATALOG_CODEC_VERSION
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

fn encode_index_key(index_id: u64, value: &Value, row_id: u64, version: u16) -> Result<Vec<u8>> {
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
        PRODUCTION_INDEX_KEY_VERSION => encode_ordered_value(value, &mut key, 0)?,
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
    fn production_index_keys_preserve_scalar_order_and_zero_bytes() {
        fn payload(value: Value) -> Vec<u8> {
            let key = encode_index_key(7, &value, 11, PRODUCTION_INDEX_KEY_VERSION).unwrap();
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
