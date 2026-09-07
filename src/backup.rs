use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Engine;
use crate::db::{Database, SchemaInfo};
use crate::error::{Error, Result};
use crate::idempotency::{ReceiptMap, validate_receipts};

const LEGACY_BACKUP_FORMAT_VERSION: u32 = 1;
const RECEIPT_BACKUP_FORMAT_VERSION: u32 = 2;
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

#[derive(Serialize)]
struct BackupPayload<'a> {
    database: &'a Database,
    receipts: &'a ReceiptMap,
}

pub fn create(db: impl Into<PathBuf>, output: impl AsRef<Path>) -> Result<BackupInfo> {
    let engine = Engine::open_redb(db)?;
    let (database, receipts) = engine.logical_snapshot();
    write_database(database, receipts, output.as_ref())
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
    let database = engine.database_snapshot().validate_logical_backup()?;
    let receipts = ReceiptMap::new();
    let info = info(&database, &receipts, LEGACY_BACKUP_FORMAT_VERSION)?;
    drop(Engine::restore_redb(db.into(), database, receipts)?);
    Ok(info)
}

fn write_database(database: Database, receipts: ReceiptMap, output: &Path) -> Result<BackupInfo> {
    if std::fs::symlink_metadata(output).is_ok() {
        return Err(Error::new(
            "E_BACKUP",
            format!("backup output '{}' already exists", output.display()),
        ));
    }
    let database = database.validate_logical_backup()?;
    validate_receipts(&receipts, database.sequence)?;
    let format_version = if receipts.is_empty() {
        LEGACY_BACKUP_FORMAT_VERSION
    } else {
        RECEIPT_BACKUP_FORMAT_VERSION
    };
    let info = info(&database, &receipts, format_version)?;
    let envelope = BackupEnvelope {
        format_version: info.format_version,
        checksum: info.checksum.clone(),
        schema: info.schema.clone(),
        database,
        receipts,
    };
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| Error::new("E_IO", error.to_string()))?;
    }
    let encoded = serde_json::to_vec(&envelope)
        .map_err(|error| Error::new("E_BACKUP", format!("encode backup: {error}")))?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| Error::new("E_IO", format!("create backup: {error}")))?;
    let mut writer = BufWriter::new(file);
    if let Err(error) = writer
        .write_all(&encoded)
        .and_then(|_| writer.flush())
        .and_then(|_| writer.get_ref().sync_all())
    {
        drop(writer);
        let _ = std::fs::remove_file(output);
        return Err(Error::new("E_IO", format!("write backup: {error}")));
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
    let envelope: BackupEnvelope = serde_json::from_reader(BufReader::new(file))
        .map_err(|error| Error::new("E_BACKUP", format!("decode backup: {error}")))?;
    if !matches!(
        envelope.format_version,
        LEGACY_BACKUP_FORMAT_VERSION | RECEIPT_BACKUP_FORMAT_VERSION
    ) {
        return Err(Error::new(
            "E_BACKUP",
            format!(
                "unsupported backup format version {}",
                envelope.format_version
            ),
        ));
    }
    let database = envelope.database.validate_logical_backup()?;
    if envelope.format_version == LEGACY_BACKUP_FORMAT_VERSION && !envelope.receipts.is_empty() {
        return Err(Error::new(
            "E_BACKUP",
            "backup format 1 must not contain idempotency receipts",
        ));
    }
    validate_receipts(&envelope.receipts, database.sequence)?;
    let actual = info(&database, &envelope.receipts, envelope.format_version)?;
    if actual.checksum != envelope.checksum || actual.schema != envelope.schema {
        return Err(Error::new(
            "E_BACKUP",
            "backup checksum or schema metadata does not match",
        ));
    }
    Ok((database, envelope.receipts, actual))
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
