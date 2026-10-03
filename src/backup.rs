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

pub mod incremental;

const LEGACY_BACKUP_FORMAT_VERSION: u32 = 1;
const RECEIPT_BACKUP_FORMAT_VERSION: u32 = 2;
const SCALAR_BACKUP_FORMAT_VERSION: u32 = 3;
pub const PRODUCTION_BACKUP_FORMAT_VERSION: u32 = 4;
pub const MAP_BACKUP_FORMAT_VERSION: u32 = 5;
pub const PARTIAL_BACKUP_FORMAT_VERSION: u32 = 6;
pub const REFERENCE_BACKUP_FORMAT_VERSION: u32 = 7;
pub const CAPABILITY_BACKUP_FORMAT_VERSION: u32 = 8;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage_header: Option<String>,
}

#[derive(Deserialize)]
struct RawBackupEnvelope {
    format_version: u32,
    checksum: String,
    database: Box<serde_json::value::RawValue>,
    #[serde(default)]
    receipts: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    storage_header: Option<String>,
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
    let format = engine.logical_backup_format();
    let header = engine.stored_header()?;
    write_database_view_with_header(
        database,
        source,
        receipts,
        output.as_ref(),
        format,
        header.as_deref(),
    )
}

pub fn restore(backup: impl AsRef<Path>, db: impl Into<PathBuf>) -> Result<BackupInfo> {
    let (database, receipts, info, header) = read_database_with_header(backup.as_ref())?;
    drop(Engine::restore_redb_with_header(
        db.into(),
        database,
        receipts,
        header,
    )?);
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
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_header: Option<&'a str>,
}

#[derive(Serialize)]
struct StreamingBackupEnvelope<'a> {
    format_version: u32,
    checksum: &'a str,
    schema: &'a SchemaInfo,
    database: StreamingDatabase<'a>,
    receipts: &'a ReceiptMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_header: Option<&'a str>,
}

struct StreamingDatabase<'a> {
    database: &'a Database,
    source: &'a dyn TypedRowSource,
    tables: BTreeMap<String, DurableTable>,
    indexes: BTreeMap<String, BTreeMap<String, IndexDefinition>>,
    references: BTreeMap<u64, crate::db::ReferenceDefinition>,
    sequences: BTreeMap<String, crate::db::generated::Sequence>,
}

impl<'a> StreamingDatabase<'a> {
    fn new(database: &'a Database, source: &'a dyn TypedRowSource) -> Self {
        let mut tables = BTreeMap::new();
        let mut references = BTreeMap::new();
        let mut sequences = BTreeMap::new();
        let mut indexes = BTreeMap::<String, BTreeMap<String, IndexDefinition>>::new();
        for entry in database.durable_catalog_entries() {
            match entry {
                DurableCatalogEntry::Sequence(sequence) => {
                    sequences.insert(sequence.name.clone(), sequence);
                }
                DurableCatalogEntry::Reference(definition) => {
                    references.insert(definition.id, definition);
                }
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
            references,
            sequences,
        }
    }
}

impl Serialize for StreamingDatabase<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct(
            "Database",
            6 + usize::from(!self.references.is_empty()) + usize::from(!self.sequences.is_empty()),
        )?;
        if !self.references.is_empty() {
            state.serialize_field("reference_definitions", &self.references)?;
        }
        if !self.sequences.is_empty() {
            state.serialize_field("sequences", &self.sequences)?;
        }
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
        let mut state = serializer.serialize_struct(
            "Table",
            8 + usize::from(!self.table.generated_defaults.is_empty()),
        )?;
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
        if !self.table.generated_defaults.is_empty() {
            state.serialize_field("generated_defaults", &self.table.generated_defaults)?;
        }
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

#[cfg(test)]
fn write_database_view(
    database: &Database,
    source: &dyn TypedRowSource,
    receipts: &ReceiptMap,
    output: &Path,
    format_version: u32,
) -> Result<BackupInfo> {
    write_database_view_with_header(database, source, receipts, output, format_version, None)
}

fn write_database_view_with_header(
    database: &Database,
    source: &dyn TypedRowSource,
    receipts: &ReceiptMap,
    output: &Path,
    format_version: u32,
    storage_header: Option<&str>,
) -> Result<BackupInfo> {
    if database.has_generated_defaults() && storage_header.is_none() {
        return Err(Error::new(
            "E_BACKUP",
            "generated defaults require a native storage header in logical backup",
        ));
    }
    validate_backup_header(format_version, storage_header)?;
    if let Some(header) = storage_header {
        crate::redb_storage::StorageHeader::decode_transport(header)?
            .validate_required_state(database, receipts)
            .map_err(|error| Error::new("E_BACKUP", error.message))?;
    }
    if database.has_references() && format_version < REFERENCE_BACKUP_FORMAT_VERSION {
        return Err(Error::new(
            "E_BACKUP",
            "typed references require backup format 7",
        ));
    }
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
            storage_header,
        },
    )
    .map_err(|error| Error::new("E_BACKUP", format!("encode backup payload: {error}")))?;
    let checksum = format!("sha256:{:x}", payload_hash.hash.finalize());
    let info = BackupInfo {
        format_version,
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
            storage_header,
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

#[cfg(test)]
fn read_database(path: &Path) -> Result<(Database, ReceiptMap, BackupInfo)> {
    read_database_with_header(path).map(|(database, receipts, info, _)| (database, receipts, info))
}

fn validate_backup_header(format: u32, encoded: Option<&str>) -> Result<()> {
    if !(1..=CAPABILITY_BACKUP_FORMAT_VERSION).contains(&format) {
        return Err(Error::new(
            "E_BACKUP",
            format!("unsupported backup format version {format}"),
        ));
    }
    if (format == CAPABILITY_BACKUP_FORMAT_VERSION) != encoded.is_some() {
        return Err(Error::new(
            "E_BACKUP",
            "capability backup requires a storage header; legacy backups must omit it",
        ));
    }
    if let Some(encoded) = encoded {
        let layout = crate::redb_storage::StorageHeader::decode_transport(encoded)
            .map_err(|error| Error::new("E_BACKUP", error.message))?;
        if !layout.has_header() {
            return Err(Error::new(
                "E_BACKUP",
                "capability backup requires the native physical layout",
            ));
        }
    }
    Ok(())
}

fn read_database_with_header(
    path: &Path,
) -> Result<(Database, ReceiptMap, BackupInfo, Option<String>)> {
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
    validate_backup_header(raw.format_version, raw.storage_header.as_deref())?;
    let envelope: BackupEnvelope = serde_json::from_slice(&encoded)
        .map_err(|error| Error::new("E_BACKUP", format!("decode backup: {error}")))?;
    if !matches!(
        envelope.format_version,
        LEGACY_BACKUP_FORMAT_VERSION
            | RECEIPT_BACKUP_FORMAT_VERSION
            | SCALAR_BACKUP_FORMAT_VERSION
            | PRODUCTION_BACKUP_FORMAT_VERSION
            | MAP_BACKUP_FORMAT_VERSION
            | PARTIAL_BACKUP_FORMAT_VERSION
            | REFERENCE_BACKUP_FORMAT_VERSION
            | CAPABILITY_BACKUP_FORMAT_VERSION
    ) {
        return Err(Error::new(
            "E_BACKUP",
            format!(
                "unsupported backup format version {}",
                envelope.format_version
            ),
        ));
    }
    if envelope.database.has_generated_defaults() && envelope.storage_header.is_none() {
        return Err(Error::new(
            "E_BACKUP",
            "generated defaults require a native storage header in logical backup",
        ));
    }
    if raw.format_version != envelope.format_version
        || raw.checksum != envelope.checksum
        || raw.storage_header != envelope.storage_header
    {
        return Err(Error::new(
            "E_BACKUP",
            "backup envelope metadata is inconsistent",
        ));
    }
    let payload = if raw.format_version == LEGACY_BACKUP_FORMAT_VERSION {
        raw.database.get().as_bytes().to_vec()
    } else {
        let receipts = raw.receipts.as_ref().map_or("{}", |value| value.get());
        if let Some(header) = &raw.storage_header {
            let header = serde_json::to_string(header)
                .map_err(|error| Error::new("E_BACKUP", error.to_string()))?;
            format!(
                "{{\"database\":{},\"receipts\":{receipts},\"storage_header\":{header}}}",
                raw.database.get()
            )
            .into_bytes()
        } else {
            format!(
                "{{\"database\":{},\"receipts\":{receipts}}}",
                raw.database.get()
            )
            .into_bytes()
        }
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
    if envelope.format_version < REFERENCE_BACKUP_FORMAT_VERSION
        && envelope.database.has_references()
    {
        return Err(Error::new(
            "E_BACKUP",
            "typed references require backup format 7",
        ));
    }
    let database = envelope.database.validate_logical_backup()?;
    if envelope.format_version < MAP_BACKUP_FORMAT_VERSION && database.requires_map_storage() {
        return Err(Error::new("E_BACKUP", "typed maps require backup format 5"));
    }
    if envelope.format_version < PARTIAL_BACKUP_FORMAT_VERSION
        && database.requires_partial_index_storage()
    {
        return Err(Error::new(
            "E_BACKUP",
            "partial unique indexes require backup format 6",
        ));
    }
    if envelope.format_version == LEGACY_BACKUP_FORMAT_VERSION && !envelope.receipts.is_empty() {
        return Err(Error::new(
            "E_BACKUP",
            "backup format 1 must not contain idempotency receipts",
        ));
    }
    validate_receipts(&envelope.receipts, database.sequence)?;
    if let Some(header) = &envelope.storage_header {
        crate::redb_storage::StorageHeader::decode_transport(header)?
            .validate_required_state(&database, &envelope.receipts)
            .map_err(|error| Error::new("E_BACKUP", error.message))?;
    }
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
    Ok((database, envelope.receipts, info, envelope.storage_header))
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

#[cfg(test)]
mod reference_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temporary(std::path::PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "unionid-reference-backup-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn referenced_database() -> Database {
        let mut engine = Engine::memory();
        let result = engine.execute("struct Parent {id: int}\nstruct Child {id: int, parent: Option<int>}\ntable parents: Parent {key id}\ntable children: Child {key id}\ninsert parents {id: 1}\ninsert many children [{id: 1, parent: Some(1)}, {id: 2, parent: None}]\ncreate reference children (parent) references parents (id)");
        assert!(result.ok, "{}", result.message);
        engine.database_snapshot().unwrap()
    }

    #[test]
    fn capability_backup_requires_maps_retained_only_in_a_nested_receipt() {
        let temp = Temporary::new();
        let database = Engine::memory().database_snapshot().unwrap();
        let mut response = crate::db::QueryResponse::ok_message("retained result");
        response.rows.push(BTreeMap::from([(
            "old".into(),
            crate::Value::Option(Some(Box::new(crate::Value::Named {
                type_id: 1,
                value: Box::new(crate::Value::Map(BTreeMap::from([(
                    "key".into(),
                    crate::Value::Int(1),
                )]))),
            }))),
        )]));
        let receipts = ReceiptMap::from_iter([(
            String::from("retained"),
            crate::idempotency::IdempotencyReceipt {
                digest: format!("sha256:{}", "0".repeat(64)),
                committed_sequence: database.sequence,
                completed_at_unix_ms: 1,
                response,
            },
        )]);
        assert!(!database.requires_map_storage());
        let base_header = crate::redb_storage::StorageHeader::encode_transport(
            crate::redb_storage::StorageLayout::from_header_profile(false, false, false, false),
        )
        .unwrap();
        let map_header = crate::redb_storage::StorageHeader::encode_transport(
            crate::redb_storage::StorageLayout::from_header_profile(true, false, false, false),
        )
        .unwrap();
        let output = temp.0.join("backup.json");
        let error = write_database_view_with_header(
            &database,
            &database,
            &receipts,
            &output,
            CAPABILITY_BACKUP_FORMAT_VERSION,
            Some(&base_header),
        )
        .unwrap_err();
        assert_eq!(error.code, "E_BACKUP");
        assert!(!output.exists());
        write_database_view_with_header(
            &database,
            &database,
            &receipts,
            &output,
            CAPABILITY_BACKUP_FORMAT_VERSION,
            Some(&map_header),
        )
        .unwrap();
        let mut envelope: BackupEnvelope =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        envelope.storage_header = Some(base_header);
        let payload = StreamingBackupPayload {
            database: StreamingDatabase::new(&envelope.database, &envelope.database),
            receipts: &envelope.receipts,
            storage_header: envelope.storage_header.as_deref(),
        };
        envelope.checksum = format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&payload).unwrap())
        );
        std::fs::write(&output, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let target = temp.0.join("rejected.redb");
        let error = restore(&output, &target).unwrap_err();
        assert_eq!(error.code, "E_BACKUP");
        assert!(error.message.contains("retained receipts"), "{error}");
        assert!(!target.exists());
    }

    #[test]
    fn capability_backup_round_trips_header_and_rejects_unrecognized_metadata_before_data() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        for references in [false, true] {
            let temp = Temporary::new();
            let path = temp.0.join("source.redb");
            let output = temp.0.join("backup.json");
            let destination = temp.0.join("restored.redb");
            let mut engine = Engine::open_redb(&path).unwrap();
            assert!(engine.execute("struct Item {id: int, label: text}\ntable items: Item {key id}\ninsert items {id: 1, label: \"one\"}").ok);
            if references {
                engine.upgrade_storage(12).unwrap();
            }
            engine.upgrade_storage(14).unwrap();
            let header = engine.stored_header().unwrap();
            let schema = engine.schema_info();
            let sequence = engine.backup_journal_status().unwrap().head_sequence;
            drop(engine);
            let info = create(&path, &output).unwrap();
            assert_eq!(info.format_version, CAPABILITY_BACKUP_FORMAT_VERSION);
            assert_eq!(restore(&output, &destination).unwrap(), info);
            let mut restored = Engine::open_redb(&destination).unwrap();
            restored.check_integrity().unwrap();
            assert_eq!(restored.stored_header().unwrap(), header);
            assert_eq!(restored.schema_info(), schema);
            assert_eq!(
                restored.backup_journal_status().unwrap().head_sequence,
                sequence
            );
            assert_eq!(restored.execute("from items").rows.len(), 1);
            drop(restored);

            let original: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
            let mut unknown = original.clone();
            let encoded = unknown["storage_header"].as_str().unwrap();
            let bytes = URL_SAFE_NO_PAD.decode(encoded).unwrap();
            let json = std::str::from_utf8(&bytes[6..])
                .unwrap()
                .replace("typed_map", "unknownxx");
            unknown["storage_header"] = serde_json::json!(
                URL_SAFE_NO_PAD.encode([bytes[..6].to_vec(), json.into_bytes()].concat())
            );
            unknown["database"] = serde_json::json!("invalid business data");
            let corrupt = temp.0.join("corrupt.json");
            std::fs::write(&corrupt, serde_json::to_vec(&unknown).unwrap()).unwrap();
            let target = temp.0.join("rejected.redb");
            let error = restore(&corrupt, &target).unwrap_err();
            assert_eq!(error.code, "E_BACKUP");
            assert!(error.message.contains("capability"), "{error}");
            assert!(!target.exists());

            let mut tampered = original;
            tampered["storage_header"] = serde_json::json!(
                crate::redb_storage::StorageHeader::encode_transport(
                    crate::redb_storage::StorageLayout::from_header_profile(
                        false, false, false, false
                    )
                )
                .unwrap()
            );
            std::fs::write(&corrupt, serde_json::to_vec(&tampered).unwrap()).unwrap();
            let error = restore(&corrupt, &target).unwrap_err();
            assert!(error.message.contains("checksum"), "{error}");
            assert!(!target.exists());
        }
    }

    #[test]
    fn checksummed_reference_corruption_is_rejected_before_restore() {
        let temp = Temporary::new();
        let db = referenced_database();
        let original = temp.0.join("original.json");
        write_database_view(
            &db,
            &db,
            &ReceiptMap::new(),
            &original,
            REFERENCE_BACKUP_FORMAT_VERSION,
        )
        .unwrap();
        let baseline: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&original).unwrap()).unwrap();
        for corruption in ["orphan", "mode", "cached_path", "duplicate"] {
            let mut value = baseline.clone();
            if corruption == "orphan" {
                assert!(value["database"]["objects"]["parents"]["rows"].is_array());
                value["database"]["objects"]["parents"]["rows"] = serde_json::json!([]);
            } else {
                let references = value["database"]["reference_definitions"]
                    .as_object_mut()
                    .unwrap();
                let first = references.keys().next().unwrap().clone();
                match corruption {
                    "mode" => {
                        references[&first]["components"][0]["mode"] = serde_json::json!("exact")
                    }
                    "cached_path" => {
                        references[&first]["components"][0]["column"] = serde_json::json!("wrong")
                    }
                    "duplicate" => {
                        let mut duplicate = references[&first].clone();
                        duplicate["id"] = serde_json::json!(100_000);
                        references.insert("100000".into(), duplicate);
                        value["database"]["catalog"]["next_id"] = serde_json::json!(100_001);
                    }
                    _ => unreachable!(),
                }
            }
            let mut envelope: BackupEnvelope = serde_json::from_value(value).unwrap();
            envelope.checksum = info(
                &envelope.database,
                &envelope.receipts,
                REFERENCE_BACKUP_FORMAT_VERSION,
            )
            .unwrap()
            .checksum;
            let path = temp.0.join(format!("{corruption}.json"));
            std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
            let error = read_database(&path).unwrap_err();
            assert_eq!(error.code, "E_STORAGE", "{corruption}: {error}");
            assert!(!error.message.contains("checksum"), "{corruption}: {error}");
        }
    }

    #[test]
    fn reference_backup_preserves_metadata_rows_and_enforcement() {
        let temp = Temporary::new();
        let db = referenced_database();
        let output = temp.0.join("backup.json");
        let receipt = ReceiptMap::new();
        assert_eq!(
            write_database_view(&db, &db, &receipt, &output, PARTIAL_BACKUP_FORMAT_VERSION)
                .unwrap_err()
                .code,
            "E_BACKUP"
        );
        assert!(!output.exists());
        let written =
            write_database_view(&db, &db, &receipt, &output, REFERENCE_BACKUP_FORMAT_VERSION)
                .unwrap();
        let (mut restored, _, read) = read_database(&output).unwrap();
        assert_eq!(written.schema, read.schema);
        assert_eq!(db.schema_text(), restored.schema_text());
        let statement = crate::syntax::parse("delete parents")
            .unwrap()
            .remove(0)
            .statement;
        assert_eq!(
            restored.execute(statement).unwrap_err().constraint,
            Some(crate::error::ConstraintKind::ReferenceRestricted)
        );
        // A forged old format header must not make references disappear.
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        envelope["format_version"] = serde_json::json!(6);
        std::fs::write(&output, serde_json::to_vec(&envelope).unwrap()).unwrap();
        // Re-serialization changes payload bytes, so recompute a valid checksum
        // to prove the version gate, not the checksum check, rejects the file.
        let mut envelope: BackupEnvelope =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        envelope.checksum = info(&envelope.database, &envelope.receipts, 6)
            .unwrap()
            .checksum;
        std::fs::write(&output, serde_json::to_vec(&envelope).unwrap()).unwrap();
        let error = read_database(&output).unwrap_err();
        assert_eq!(error.code, "E_BACKUP");
        assert!(error.message.contains("format 7"));
    }
}
