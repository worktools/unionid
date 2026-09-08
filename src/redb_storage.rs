//! Transactional redb storage for the durable Engine mode.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use redb::{
    Database as RedbDatabase, Durability, ReadableDatabase, ReadableTable, TableDefinition,
    TableHandle,
};

use crate::codec::VALUE_CODEC_VERSION;
use crate::db::{Database, DurableCatalogEntry, DurableMeta};
use crate::error::{Error, Result};
use crate::idempotency::{
    IdempotencyReceipt, MAX_IDEMPOTENCY_RECEIPT_BYTES, MAX_IDEMPOTENCY_TOTAL_BYTES, ReceiptMap,
    encoded_receipt, ensure_legacy_receipts, validate_receipts,
};
use crate::migration::MigrationEntry;

const LEGACY_STORAGE_FORMAT_VERSION: u32 = 1;
const RECEIPT_STORAGE_FORMAT_VERSION: u32 = 2;
const CURSOR_STORAGE_FORMAT_VERSION: u32 = 3;
const CATALOG_CODEC_VERSION: u16 = 2;
const LEGACY_CATALOG_CODEC_VERSION: u16 = 1;
const INDEX_KEY_VERSION: u16 = 1;
const MIGRATION_CODEC_VERSION: u16 = 1;
const RECEIPT_CODEC_VERSION: u16 = 1;

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
    committed: PreparedState,
}

pub(crate) enum CommitFailure {
    Definite(Error),
    Uncertain(Error),
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
        let mut store = Self {
            database,
            committed: PreparedState::new(
                &empty,
                &ReceiptMap::new(),
                CURSOR_STORAGE_FORMAT_VERSION,
            )?,
        };
        if fresh {
            store
                .commit(&empty, &empty, &ReceiptMap::new())
                .map_err(CommitFailure::into_error)?;
        }
        let (loaded, receipts, committed) = store.load()?;
        let needs_cursor_upgrade = committed.format_version < CURSOR_STORAGE_FORMAT_VERSION;
        store.committed = committed;
        if needs_cursor_upgrade {
            store
                .commit(&loaded, &loaded, &receipts)
                .map_err(CommitFailure::into_error)?;
        }
        Ok((store, loaded, receipts))
    }

    pub(crate) fn commit(
        &mut self,
        _previous: &Database,
        database: &Database,
        receipts: &ReceiptMap,
    ) -> std::result::Result<(), CommitFailure> {
        validate_receipts(receipts, database.sequence).map_err(CommitFailure::Definite)?;
        let format_version = CURSOR_STORAGE_FORMAT_VERSION;
        let next = PreparedState::new(database, receipts, format_version)
            .map_err(CommitFailure::Definite)?;
        let prepared = PreparedDelta::between(&self.committed, &next);
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
            write_meta(&mut table, &prepared.meta, prepared.format_version)
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
        self.committed = next;
        Ok(())
    }

    pub(crate) fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap)> {
        let backend_clean = self
            .database
            .check_integrity()
            .map_err(|error| storage_error("check redb integrity", error))?;
        let (database, receipts, committed) = self.load()?;
        self.committed = committed;
        Ok((backend_clean, database, receipts))
    }

    fn load(&self) -> Result<(Database, ReceiptMap, PreparedState)> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin redb read transaction", error))?;
        let (meta, format_version) = {
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
                entries.push(decode_catalog_entry(&key, &value)?);
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
                validate_index_key(key.value())?;
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
                let receipt = decode_receipt(&value)?;
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
        database.ensure_legacy_scalars()?;
        ensure_legacy_receipts(&receipts)?;
        validate_receipts(&receipts, database.sequence)?;
        if format_version == LEGACY_STORAGE_FORMAT_VERSION && !receipts.is_empty() {
            return Err(Error::new(
                "E_STORAGE",
                "storage format 1 must not contain idempotency receipts",
            ));
        }
        let expected_indexes = database
            .durable_secondary_indexes()?
            .into_iter()
            .map(|(index_id, value, row_id)| encode_index_key(index_id, &value, row_id))
            .collect::<Result<BTreeSet<_>>>()?;
        if stored_indexes != expected_indexes {
            return Err(Error::new(
                "E_STORAGE",
                "durable secondary indexes do not match the stored rows and catalog",
            ));
        }
        let committed = PreparedState {
            format_version,
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
    format_version: u32,
    meta: DurableMeta,
    catalog: BTreeMap<Vec<u8>, Vec<u8>>,
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    secondary_indexes: BTreeSet<Vec<u8>>,
    migrations: BTreeMap<u64, Vec<u8>>,
    receipts: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl PreparedState {
    fn new(database: &Database, receipts: &ReceiptMap, format_version: u32) -> Result<Self> {
        database.ensure_legacy_scalars()?;
        ensure_legacy_receipts(receipts)?;
        let mut catalog = BTreeMap::new();
        for entry in database.durable_catalog_entries() {
            catalog.insert(
                encode_catalog_key(entry.kind_tag(), entry.stable_id()),
                encode_catalog_entry(&entry)?,
            );
        }
        let rows = database
            .durable_rows()?
            .into_iter()
            .map(|(table_id, row_id, value)| (encode_row_key(table_id, row_id), value))
            .collect::<BTreeMap<_, _>>();
        let secondary_indexes = database
            .durable_secondary_indexes()?
            .into_iter()
            .map(|(index_id, value, row_id)| encode_index_key(index_id, &value, row_id))
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
            .map(|(key, receipt)| Ok((key.as_bytes().to_vec(), encode_receipt(receipt)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            format_version,
            meta: database.durable_meta(),
            catalog,
            rows,
            secondary_indexes,
            migrations,
            receipts,
        })
    }
}

struct PreparedDelta {
    format_version: u32,
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
        let previous =
            PreparedState::new(previous, &ReceiptMap::new(), LEGACY_STORAGE_FORMAT_VERSION)?;
        let next = PreparedState::new(database, &ReceiptMap::new(), LEGACY_STORAGE_FORMAT_VERSION)?;
        Ok(Self::between(&previous, &next))
    }

    fn between(previous: &PreparedState, next: &PreparedState) -> Self {
        Self {
            format_version: next.format_version,
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

struct BytesDelta {
    deletes: Vec<BytesDelete>,
    writes: Vec<BytesWrite>,
}

struct BytesDelete {
    key: Vec<u8>,
    expected: Vec<u8>,
}

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
    meta: &DurableMeta,
    format_version: u32,
) -> Result<()> {
    for (key, value) in [
        (FORMAT_KEY, format_version.to_be_bytes().to_vec()),
        (
            CATALOG_CODEC_KEY,
            CATALOG_CODEC_VERSION.to_be_bytes().to_vec(),
        ),
        (VALUE_CODEC_KEY, VALUE_CODEC_VERSION.to_be_bytes().to_vec()),
        (
            INDEX_KEY_CODEC_KEY,
            INDEX_KEY_VERSION.to_be_bytes().to_vec(),
        ),
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
    ] {
        table
            .insert(key, value.as_slice())
            .map_err(|error| storage_error("write meta entry", error))?;
    }
    Ok(())
}

fn read_meta(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<(DurableMeta, u32)> {
    let format_version = u32::from_be_bytes(read_fixed::<4>(table, FORMAT_KEY)?);
    if !matches!(
        format_version,
        LEGACY_STORAGE_FORMAT_VERSION
            | RECEIPT_STORAGE_FORMAT_VERSION
            | CURSOR_STORAGE_FORMAT_VERSION
    ) {
        return Err(Error::new("E_STORAGE", format!("unsupported {FORMAT_KEY}")));
    }
    let catalog_version = u16::from_be_bytes(read_fixed::<2>(table, CATALOG_CODEC_KEY)?);
    if !matches!(
        catalog_version,
        LEGACY_CATALOG_CODEC_VERSION | CATALOG_CODEC_VERSION
    ) {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported {CATALOG_CODEC_KEY}"),
        ));
    }
    expect_version(
        VALUE_CODEC_KEY,
        read_fixed::<2>(table, VALUE_CODEC_KEY)?,
        VALUE_CODEC_VERSION.to_be_bytes(),
    )?;
    expect_version(
        INDEX_KEY_CODEC_KEY,
        read_fixed::<2>(table, INDEX_KEY_CODEC_KEY)?,
        INDEX_KEY_VERSION.to_be_bytes(),
    )?;
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
        format_version,
    ))
}

fn encode_receipt(receipt: &IdempotencyReceipt) -> Result<Vec<u8>> {
    let mut value = Vec::from(RECEIPT_MAGIC.as_slice());
    value.extend_from_slice(&RECEIPT_CODEC_VERSION.to_be_bytes());
    value.extend(encoded_receipt(receipt)?);
    Ok(value)
}

fn decode_receipt(value: &[u8]) -> Result<IdempotencyReceipt> {
    if value.len() < 6 || &value[..4] != RECEIPT_MAGIC {
        return Err(Error::new(
            "E_STORAGE",
            "invalid idempotency receipt codec magic",
        ));
    }
    let version = u16::from_be_bytes(value[4..6].try_into().unwrap());
    if version != RECEIPT_CODEC_VERSION {
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

fn expect_version<const N: usize>(key: &str, actual: [u8; N], expected: [u8; N]) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::new("E_STORAGE", format!("unsupported {key}")))
    }
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

fn encode_catalog_entry(entry: &DurableCatalogEntry) -> Result<Vec<u8>> {
    let mut value = Vec::from(CATALOG_MAGIC.as_slice());
    value.extend_from_slice(&CATALOG_CODEC_VERSION.to_be_bytes());
    let mut json = serde_json::to_value(entry)
        .map_err(|error| Error::new("E_STORAGE", format!("encode catalog entry: {error}")))?;
    if let DurableCatalogEntry::Index { definition, .. } = entry {
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

fn decode_catalog_entry(key: &[u8], value: &[u8]) -> Result<DurableCatalogEntry> {
    if key.len() != 9 {
        return Err(Error::new("E_STORAGE", "invalid durable catalog key"));
    }
    if value.len() < 6 || &value[..4] != CATALOG_MAGIC {
        return Err(Error::new("E_STORAGE", "invalid catalog codec magic"));
    }
    let version = u16::from_be_bytes(value[4..6].try_into().unwrap());
    if !matches!(
        version,
        LEGACY_CATALOG_CODEC_VERSION | CATALOG_CODEC_VERSION
    ) {
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

fn encode_index_key(index_id: u64, value: &str, row_id: u64) -> Result<Vec<u8>> {
    let value_len = u32::try_from(value.len())
        .map_err(|_| Error::new("E_LIMIT", "secondary index key exceeds u32 length"))?;
    let mut key = Vec::with_capacity(26 + value.len());
    key.extend_from_slice(INDEX_MAGIC);
    key.extend_from_slice(&INDEX_KEY_VERSION.to_be_bytes());
    key.extend_from_slice(&index_id.to_be_bytes());
    key.extend_from_slice(&value_len.to_be_bytes());
    key.extend_from_slice(value.as_bytes());
    key.extend_from_slice(&row_id.to_be_bytes());
    Ok(key)
}

fn validate_index_key(key: &[u8]) -> Result<()> {
    if key.len() < 26 || &key[..4] != INDEX_MAGIC {
        return Err(Error::new("E_STORAGE", "invalid secondary index key"));
    }
    let version = u16::from_be_bytes(key[4..6].try_into().unwrap());
    if version != INDEX_KEY_VERSION {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported secondary index key version {version}"),
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
    Ok(())
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
}
