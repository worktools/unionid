use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::ser::{Error as _, SerializeMap, SerializeSeq, SerializeStruct};
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::Engine;
use crate::db::{Database, DurableCatalogEntry, DurableTable, IndexDefinition, SchemaInfo};
use crate::error::{Error, Result};
use crate::idempotency::{ReceiptMap, ensure_legacy_receipts, validate_receipts};
use crate::profile::ExecutionObservation;
use crate::row_source::TypedRowSource;

const LEGACY_BACKUP_FORMAT_VERSION: u32 = 1;
const RECEIPT_BACKUP_FORMAT_VERSION: u32 = 2;
const SCALAR_BACKUP_FORMAT_VERSION: u32 = 3;
pub const PRODUCTION_BACKUP_FORMAT_VERSION: u32 = 4;
const MAX_BACKUP_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupInfo {
    pub format_version: u32,
    pub checksum: String,
    pub schema: SchemaInfo,
    pub migration_count: usize,
    pub receipt_count: usize,
}

#[derive(Serialize, Deserialize)]
struct BackupEnvelope {
    format_version: u32,
    checksum: String,
    schema: SchemaInfo,
    database: Database,
    #[serde(default, skip_serializing_if = "ReceiptMap::is_empty")]
    receipts: ReceiptMap,
}

#[derive(Deserialize)]
struct RawBackupEnvelope {
    format_version: u32,
    checksum: String,
    database: Box<serde_json::value::RawValue>,
    #[serde(default)]
    receipts: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Serialize)]
struct BackupPayload<'a> {
    database: &'a Database,
    receipts: &'a ReceiptMap,
}

pub fn create(db: impl Into<PathBuf>, output: impl AsRef<Path>) -> Result<BackupInfo> {
    let mut engine = Engine::open_redb(db)?;
    engine.check_integrity()?;
    let (database, source, receipts) = engine.logical_backup_view();
    write_database_view(database, source, receipts, output.as_ref())
}

pub fn restore(backup: impl AsRef<Path>, db: impl Into<PathBuf>) -> Result<BackupInfo> {
    let (database, receipts, info) = read_database(backup.as_ref())?;
    drop(Engine::restore_redb(db.into(), database, receipts)?);
    Ok(info)
}

pub fn import_legacy(
    snapshot: Option<PathBuf>,
    wal: Option<PathBuf>,
    db: impl Into<PathBuf>,
) -> Result<BackupInfo> {
    if snapshot.is_none() && wal.is_none() {
        return Err(Error::new(
            "E_CONFIG",
            "legacy import requires --snapshot or --wal",
        ));
    }
    let engine = Engine::open(wal, snapshot, 0)?;
    let database = engine.database_snapshot()?.validate_logical_backup()?;
    let receipts = ReceiptMap::new();
    let info = info(&database, &receipts, LEGACY_BACKUP_FORMAT_VERSION)?;
    drop(Engine::restore_redb(db.into(), database, receipts)?);
    Ok(info)
}

#[derive(Serialize)]
struct StreamingBackupPayload<'a> {
    database: StreamingDatabase<'a>,
    receipts: &'a ReceiptMap,
}

#[derive(Serialize)]
struct StreamingBackupEnvelope<'a> {
    format_version: u32,
    checksum: &'a str,
    schema: &'a SchemaInfo,
    database: StreamingDatabase<'a>,
    receipts: &'a ReceiptMap,
}

struct StreamingDatabase<'a> {
    database: &'a Database,
    source: &'a dyn TypedRowSource,
    tables: BTreeMap<String, DurableTable>,
    indexes: BTreeMap<String, BTreeMap<String, IndexDefinition>>,
}

impl<'a> StreamingDatabase<'a> {
    fn new(database: &'a Database, source: &'a dyn TypedRowSource) -> Self {
        let mut tables = BTreeMap::new();
        let mut indexes = BTreeMap::<String, BTreeMap<String, IndexDefinition>>::new();
        for entry in database.durable_catalog_entries() {
            match entry {
                DurableCatalogEntry::Type(_) => {}
                DurableCatalogEntry::Table(table) => {
                    tables.insert(table.name.clone(), table);
                }
                DurableCatalogEntry::Index { table, definition } => {
                    indexes
                        .entry(table)
                        .or_default()
                        .insert(definition.shape_key(), definition);
                }
            }
        }
        Self {
            database,
            source,
            tables,
            indexes,
        }
    }
}

impl Serialize for StreamingDatabase<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Database", 6)?;
        state.serialize_field(
            "objects",
            &StreamingObjects {
                tables: &self.tables,
                source: self.source,
            },
        )?;
        state.serialize_field("index_definitions", &self.indexes)?;
        state.serialize_field("catalog", &self.database.catalog)?;
        state.serialize_field("sequence", &self.database.sequence)?;
        state.serialize_field("schema_revision", &self.database.schema_info().revision)?;
        state.serialize_field("migration_history", self.database.migration_history())?;
        state.end()
    }
}

struct StreamingObjects<'a> {
    tables: &'a BTreeMap<String, DurableTable>,
    source: &'a dyn TypedRowSource,
}

impl Serialize for StreamingObjects<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.tables.len()))?;
        for (name, table) in self.tables {
            map.serialize_entry(
                name,
                &StreamingTable {
                    table,
                    source: self.source,
                },
            )?;
        }
        map.end()
    }
}

struct StreamingTable<'a> {
    table: &'a DurableTable,
    source: &'a dyn TypedRowSource,
}

impl Serialize for StreamingTable<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Table", 8)?;
        state.serialize_field("object", "Table")?;
        state.serialize_field("id", &self.table.id)?;
        state.serialize_field("name", &self.table.name)?;
        state.serialize_field("schema", &self.table.schema)?;
        state.serialize_field(
            "rows",
            &StreamingRows {
                table: &self.table.name,
                source: self.source,
            },
        )?;
        state.serialize_field("next_row_id", &self.table.next_row_id)?;
        state.serialize_field("row_type", &self.table.row_type)?;
        state.serialize_field("primary_key", &self.table.primary_key)?;
        state.end()
    }
}

struct StreamingRows<'a> {
    table: &'a str,
    source: &'a dyn TypedRowSource,
}

impl Serialize for StreamingRows<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let stats = self
            .source
            .table_stats(self.table)
            .map_err(S::Error::custom)?;
        let length = stats.rows_exact.then_some(stats.rows);
        let mut sequence = serializer.serialize_seq(length)?;
        let mut cursor = self
            .source
            .scan_rows(self.table)
            .map_err(S::Error::custom)?;
        let mut observation = ExecutionObservation::default();
        while let Some(batch) = cursor
            .next_batch(None, &mut observation)
            .map_err(S::Error::custom)?
        {
            for row in batch.rows {
                sequence.serialize_element(row.as_ref())?;
            }
        }
        sequence.end()
    }
}

struct LimitedHashWriter {
    hash: Sha256,
    bytes: u64,
}

impl LimitedHashWriter {
    fn new() -> Self {
        Self {
            hash: Sha256::new(),
            bytes: 0,
        }
    }
}

impl Write for LimitedHashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.saturating_add(bytes.len() as u64);
        if next > MAX_BACKUP_BYTES {
            return Err(std::io::Error::other("backup exceeds 1 GiB limit"));
        }
        self.hash.update(bytes);
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct LimitedWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = MAX_BACKUP_BYTES.saturating_sub(self.bytes);
        if remaining == 0 {
            return Err(std::io::Error::other("backup exceeds 1 GiB limit"));
        }
        let allowed = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let written = self.inner.write(&bytes[..allowed])?;
        self.bytes = self.bytes.saturating_add(written as u64);
        if written < bytes.len() && self.bytes == MAX_BACKUP_BYTES {
            return Err(std::io::Error::other("backup exceeds 1 GiB limit"));
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn write_database_view(
    database: &Database,
    source: &dyn TypedRowSource,
    receipts: &ReceiptMap,
    output: &Path,
) -> Result<BackupInfo> {
    if std::fs::symlink_metadata(output).is_ok() {
        return Err(Error::new(
            "E_BACKUP",
            format!("backup output '{}' already exists", output.display()),
        ));
    }
    validate_receipts(receipts, database.sequence)?;
    let mut payload_hash = LimitedHashWriter::new();
    serde_json::to_writer(
        &mut payload_hash,
        &StreamingBackupPayload {
            database: StreamingDatabase::new(database, source),
            receipts,
        },
    )
    .map_err(|error| Error::new("E_BACKUP", format!("encode backup payload: {error}")))?;
    let checksum = format!("sha256:{:x}", payload_hash.hash.finalize());
    let info = BackupInfo {
        format_version: PRODUCTION_BACKUP_FORMAT_VERSION,
        checksum,
        schema: database.schema_info(),
        migration_count: database.migration_history().len(),
        receipt_count: receipts.len(),
    };
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| Error::new("E_IO", error.to_string()))?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| Error::new("E_IO", format!("create backup: {error}")))?;
    let mut writer = LimitedWriter {
        inner: BufWriter::new(file),
        bytes: 0,
    };
    let result = serde_json::to_writer(
        &mut writer,
        &StreamingBackupEnvelope {
            format_version: info.format_version,
            checksum: &info.checksum,
            schema: &info.schema,
            database: StreamingDatabase::new(database, source),
            receipts,
        },
    )
    .map_err(|error| Error::new("E_BACKUP", format!("encode backup: {error}")))
    .and_then(|()| {
        writer
            .flush()
            .and_then(|_| writer.inner.get_ref().sync_all())
            .map_err(|error| Error::new("E_IO", format!("write backup: {error}")))
    });
    if let Err(error) = result {
        drop(writer);
        let _ = std::fs::remove_file(output);
        return Err(error);
    }
    Ok(info)
}

fn read_database(path: &Path) -> Result<(Database, ReceiptMap, BackupInfo)> {
    let file =
        File::open(path).map_err(|error| Error::new("E_IO", format!("open backup: {error}")))?;
    if file
        .metadata()
        .map_err(|error| Error::new("E_IO", error.to_string()))?
        .len()
        > MAX_BACKUP_BYTES
    {
        return Err(Error::new("E_LIMIT", "backup exceeds 1 GiB limit"));
    }
    let mut encoded = Vec::new();
    file.take(MAX_BACKUP_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|error| Error::new("E_IO", format!("read backup: {error}")))?;
    if encoded.len() as u64 > MAX_BACKUP_BYTES {
        return Err(Error::new("E_LIMIT", "backup exceeds 1 GiB limit"));
    }
    let raw: RawBackupEnvelope = serde_json::from_slice(&encoded)
        .map_err(|error| Error::new("E_BACKUP", format!("decode backup: {error}")))?;
    let envelope: BackupEnvelope = serde_json::from_slice(&encoded)
        .map_err(|error| Error::new("E_BACKUP", format!("decode backup: {error}")))?;
    if !matches!(
        envelope.format_version,
        LEGACY_BACKUP_FORMAT_VERSION
            | RECEIPT_BACKUP_FORMAT_VERSION
            | SCALAR_BACKUP_FORMAT_VERSION
            | PRODUCTION_BACKUP_FORMAT_VERSION
    ) {
        return Err(Error::new(
            "E_BACKUP",
            format!(
                "unsupported backup format version {}",
                envelope.format_version
            ),
        ));
    }
    if raw.format_version != envelope.format_version || raw.checksum != envelope.checksum {
        return Err(Error::new(
            "E_BACKUP",
            "backup envelope metadata is inconsistent",
        ));
    }
    let payload = if raw.format_version == LEGACY_BACKUP_FORMAT_VERSION {
        raw.database.get().as_bytes().to_vec()
    } else {
        let receipts = raw.receipts.as_ref().map_or("{}", |value| value.get());
        format!(
            "{{\"database\":{},\"receipts\":{receipts}}}",
            raw.database.get()
        )
        .into_bytes()
    };
    let checksum = format!("sha256:{:x}", Sha256::digest(payload));
    if checksum != envelope.checksum {
        return Err(Error::new(
            "E_BACKUP",
            "backup checksum does not match its payload",
        ));
    }
    if envelope.format_version < SCALAR_BACKUP_FORMAT_VERSION {
        envelope.database.ensure_legacy_scalars()?;
        ensure_legacy_receipts(&envelope.receipts)?;
    }
    let database = envelope.database.validate_logical_backup()?;
    if envelope.format_version == LEGACY_BACKUP_FORMAT_VERSION && !envelope.receipts.is_empty() {
        return Err(Error::new(
            "E_BACKUP",
            "backup format 1 must not contain idempotency receipts",
        ));
    }
    validate_receipts(&envelope.receipts, database.sequence)?;
    let schema = database.schema_info();
    if schema != envelope.schema {
        return Err(Error::new(
            "E_BACKUP",
            "backup schema metadata does not match",
        ));
    }
    let info = BackupInfo {
        format_version: envelope.format_version,
        checksum: envelope.checksum,
        schema,
        migration_count: database.migration_history().len(),
        receipt_count: envelope.receipts.len(),
    };
    Ok((database, envelope.receipts, info))
}

fn info(database: &Database, receipts: &ReceiptMap, format_version: u32) -> Result<BackupInfo> {
    let encoded = if format_version == LEGACY_BACKUP_FORMAT_VERSION {
        serde_json::to_vec(database)
    } else {
        serde_json::to_vec(&BackupPayload { database, receipts })
    }
    .map_err(|error| Error::new("E_BACKUP", format!("encode backup payload: {error}")))?;
    Ok(BackupInfo {
        format_version,
        checksum: format!("sha256:{:x}", Sha256::digest(encoded)),
        schema: database.schema_info(),
        migration_count: database.migration_history().len(),
        receipt_count: receipts.len(),
    })
}
