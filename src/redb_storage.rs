//! Transactional redb storage for the durable Engine mode.

use std::path::PathBuf;

use redb::{
    Database as RedbDatabase, Durability, ReadableDatabase, ReadableTable, TableDefinition,
};

use crate::codec::VALUE_CODEC_VERSION;
use crate::db::{Database, DurableCatalogEntry, DurableMeta};
use crate::error::{Error, Result};

const STORAGE_FORMAT_VERSION: u32 = 1;
const CATALOG_CODEC_VERSION: u16 = 1;
const INDEX_KEY_VERSION: u16 = 1;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const CATALOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("catalog");
const ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("rows");
const SECONDARY_INDEX: TableDefinition<&[u8], u8> = TableDefinition::new("secondary_index");
const MIGRATION_LEDGER: TableDefinition<u64, &[u8]> = TableDefinition::new("migration_ledger");

const FORMAT_KEY: &str = "storage_format_version";
const CATALOG_CODEC_KEY: &str = "catalog_codec_version";
const VALUE_CODEC_KEY: &str = "value_codec_version";
const INDEX_KEY_CODEC_KEY: &str = "index_key_version";
const SEQUENCE_KEY: &str = "commit_sequence";
const SCHEMA_REVISION_KEY: &str = "schema_revision";
const NEXT_CATALOG_ID_KEY: &str = "next_catalog_id";
const SCHEMA_HASH_KEY: &str = "schema_hash";
const CATALOG_MAGIC: &[u8; 4] = b"UIDC";
const INDEX_MAGIC: &[u8; 4] = b"UIDI";

pub(crate) struct RedbStore {
    database: RedbDatabase,
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
    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<(Self, Database)> {
        let path = resolve_path(path.into())?;
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::new("E_IO", format!("create database directory: {error}"))
            })?;
        }
        let fresh = std::fs::metadata(&path).map_or(true, |metadata| metadata.len() == 0);
        let database = RedbDatabase::create(&path).map_err(open_error)?;
        let store = Self { database };
        if fresh {
            store
                .commit(&Database::default())
                .map_err(CommitFailure::into_error)?;
        }
        let loaded = store.load()?;
        Ok((store, loaded))
    }

    pub(crate) fn commit(&self, database: &Database) -> std::result::Result<(), CommitFailure> {
        let prepared = PreparedCommit::new(database).map_err(CommitFailure::Definite)?;
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
            table
                .retain(|_, _| false)
                .map_err(|error| CommitFailure::definite("clear meta table", error))?;
            write_meta(&mut table, &prepared.meta).map_err(CommitFailure::Definite)?;
        }
        {
            let mut table = transaction
                .open_table(CATALOG)
                .map_err(|error| CommitFailure::definite("open catalog table", error))?;
            table
                .retain(|_, _| false)
                .map_err(|error| CommitFailure::definite("clear catalog table", error))?;
            for (key, value) in &prepared.catalog {
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|error| CommitFailure::definite("write catalog entry", error))?;
            }
        }
        {
            let mut table = transaction
                .open_table(ROWS)
                .map_err(|error| CommitFailure::definite("open rows table", error))?;
            table
                .retain(|_, _| false)
                .map_err(|error| CommitFailure::definite("clear rows table", error))?;
            for (key, value) in &prepared.rows {
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|error| CommitFailure::definite("write row", error))?;
            }
        }
        {
            let mut table = transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| CommitFailure::definite("open secondary index table", error))?;
            table
                .retain(|_, _| false)
                .map_err(|error| CommitFailure::definite("clear secondary index table", error))?;
            for key in &prepared.secondary_indexes {
                table.insert(key.as_slice(), 0).map_err(|error| {
                    CommitFailure::definite("write secondary index entry", error)
                })?;
            }
        }
        transaction
            .open_table(MIGRATION_LEDGER)
            .map_err(|error| CommitFailure::definite("open migration ledger table", error))?;
        transaction
            .commit()
            .map_err(|error| CommitFailure::uncertain("commit redb transaction", error))
    }

    pub(crate) fn check_integrity(&mut self) -> Result<(bool, Database)> {
        let backend_clean = self
            .database
            .check_integrity()
            .map_err(|error| storage_error("check redb integrity", error))?;
        let database = self.load()?;
        Ok((backend_clean, database))
    }

    fn load(&self) -> Result<Database> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| storage_error("begin redb read transaction", error))?;
        let meta = {
            let table = transaction
                .open_table(META)
                .map_err(|error| storage_error("open meta table", error))?;
            read_meta(&table)?
        };
        let entries = {
            let table = transaction
                .open_table(CATALOG)
                .map_err(|error| storage_error("open catalog table", error))?;
            let mut entries = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate catalog table", error))?
            {
                let (key, value) =
                    entry.map_err(|error| storage_error("read catalog entry", error))?;
                entries.push(decode_catalog_entry(key.value(), value.value())?);
            }
            entries
        };
        let rows = {
            let table = transaction
                .open_table(ROWS)
                .map_err(|error| storage_error("open rows table", error))?;
            let mut rows = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate rows table", error))?
            {
                let (key, value) = entry.map_err(|error| storage_error("read row", error))?;
                let (table_id, row_id) = decode_row_key(key.value())?;
                rows.push((table_id, row_id, value.value().to_vec()));
            }
            rows
        };
        let stored_indexes = {
            let table = transaction
                .open_table(SECONDARY_INDEX)
                .map_err(|error| storage_error("open secondary index table", error))?;
            let mut keys = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| storage_error("iterate secondary index table", error))?
            {
                let (key, _) =
                    entry.map_err(|error| storage_error("read secondary index entry", error))?;
                validate_index_key(key.value())?;
                keys.push(key.value().to_vec());
            }
            keys
        };
        transaction
            .open_table(MIGRATION_LEDGER)
            .map_err(|error| storage_error("open migration ledger table", error))?;
        let database = Database::from_durable(meta, entries, rows)?;
        let mut expected_indexes = database
            .durable_secondary_indexes()?
            .into_iter()
            .map(|(index_id, value, row_id)| encode_index_key(index_id, &value, row_id))
            .collect::<Result<Vec<_>>>()?;
        expected_indexes.sort();
        if stored_indexes != expected_indexes {
            return Err(Error::new(
                "E_STORAGE",
                "durable secondary indexes do not match the stored rows and catalog",
            ));
        }
        Ok(database)
    }
}

struct PreparedCommit {
    meta: DurableMeta,
    catalog: Vec<(Vec<u8>, Vec<u8>)>,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    secondary_indexes: Vec<Vec<u8>>,
}

impl PreparedCommit {
    fn new(database: &Database) -> Result<Self> {
        let mut catalog = Vec::new();
        for entry in database.durable_catalog_entries() {
            catalog.push((
                encode_catalog_key(entry.kind_tag(), entry.stable_id()),
                encode_catalog_entry(&entry)?,
            ));
        }
        let rows = database
            .durable_rows()?
            .into_iter()
            .map(|(table_id, row_id, value)| (encode_row_key(table_id, row_id), value))
            .collect();
        let secondary_indexes = database
            .durable_secondary_indexes()?
            .into_iter()
            .map(|(index_id, value, row_id)| encode_index_key(index_id, &value, row_id))
            .collect::<Result<_>>()?;
        Ok(Self {
            meta: database.durable_meta(),
            catalog,
            rows,
            secondary_indexes,
        })
    }
}

fn write_meta(table: &mut redb::Table<'_, &str, &[u8]>, meta: &DurableMeta) -> Result<()> {
    for (key, value) in [
        (FORMAT_KEY, STORAGE_FORMAT_VERSION.to_be_bytes().to_vec()),
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
    ] {
        table
            .insert(key, value.as_slice())
            .map_err(|error| storage_error("write meta entry", error))?;
    }
    Ok(())
}

fn read_meta(table: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<DurableMeta> {
    expect_version(
        FORMAT_KEY,
        read_fixed::<4>(table, FORMAT_KEY)?,
        STORAGE_FORMAT_VERSION.to_be_bytes(),
    )?;
    expect_version(
        CATALOG_CODEC_KEY,
        read_fixed::<2>(table, CATALOG_CODEC_KEY)?,
        CATALOG_CODEC_VERSION.to_be_bytes(),
    )?;
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
    Ok(DurableMeta {
        sequence,
        schema_revision,
        next_catalog_id,
        schema_hash,
    })
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

fn encode_catalog_entry(entry: &DurableCatalogEntry) -> Result<Vec<u8>> {
    let mut value = Vec::from(CATALOG_MAGIC.as_slice());
    value.extend_from_slice(&CATALOG_CODEC_VERSION.to_be_bytes());
    value.extend(
        serde_json::to_vec(entry)
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
    if version != CATALOG_CODEC_VERSION {
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
