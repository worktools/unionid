use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ARCHIVE_CODEC_VERSION, ARTIFACT_MANIFEST_FORMAT_VERSION, ArchiveKind, ArchiveLimits,
    ArchiveManifest, BackupJournalConfig, BackupJournalState, Compression, MANIFEST_FORMAT_VERSION,
    ManifestArtifact, ManifestState, decode_archive, decode_manifest, encode_archive,
    encode_manifest, validate_archive_entry, write_new_archive_entry,
};
use crate::Engine;
use crate::db::{Database, DurableMeta};
use crate::error::{Error, Result};
use crate::idempotency::ReceiptMap;
use crate::migration::MigrationEntry;
use crate::redb_storage::{
    decode_catalog_entry, decode_migration_entry, decode_receipt, decode_row_key,
};

pub const INCREMENTAL_BACKUP_REPORT_VERSION: u16 = 1;
const RECORD_CODEC_VERSION: u16 = 1;
const DEFAULT_SEGMENT_COMMITS: u64 = 1_000;
const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
const MANIFEST_NAME: &str = "manifest.json";
const COMMIT_DOMAIN: &[u8] = b"unionid-backup-journal-commit-v1\0";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineMeta {
    sequence: u64,
    schema_revision: u64,
    next_catalog_id: u64,
    schema_hash: String,
}

struct SchemaTransition {
    storage_layout: Option<crate::redb_storage::StorageLayout>,
    revision: u64,
    hash: String,
}

struct ReplayState {
    storage_layout: Option<crate::redb_storage::StorageLayout>,
    sequence: u64,
    schema_revision: u64,
    next_catalog_id: u64,
    schema_hash: String,
    catalog: BTreeMap<Vec<u8>, Vec<u8>>,
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    migrations: BTreeMap<Vec<u8>, Vec<u8>>,
    receipts: BTreeMap<Vec<u8>, Vec<u8>>,
}

struct CommitMeta {
    storage_layout: Option<crate::redb_storage::StorageLayout>,
    sequence: u64,
    schema_revision: u64,
    next_catalog_id: u64,
    schema_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalInitOptions {
    pub compression: Compression,
    pub journal_max_commits: u64,
    pub journal_max_bytes: u64,
    pub limits: ArchiveLimits,
}

impl Default for IncrementalInitOptions {
    fn default() -> Self {
        Self {
            compression: Compression::Zstd,
            journal_max_commits: super::DEFAULT_JOURNAL_MAX_COMMITS,
            journal_max_bytes: super::DEFAULT_JOURNAL_MAX_BYTES,
            limits: ArchiveLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalExportOptions {
    pub compression: Compression,
    pub through_sequence: Option<u64>,
    pub max_commits_per_segment: u64,
    pub max_expanded_bytes_per_segment: u64,
    pub limits: ArchiveLimits,
}

impl Default for IncrementalExportOptions {
    fn default() -> Self {
        Self {
            compression: Compression::Zstd,
            through_sequence: None,
            max_commits_per_segment: DEFAULT_SEGMENT_COMMITS,
            max_expanded_bytes_per_segment: DEFAULT_SEGMENT_BYTES,
            limits: ArchiveLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalInitReport {
    pub version: u16,
    pub chain_id: String,
    pub baseline_sequence: u64,
    pub previous_storage_format: u32,
    pub current_storage_format: u32,
    pub manifest_checksum: String,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalExportReport {
    pub version: u16,
    pub chain_id: String,
    pub first_sequence: Option<u64>,
    pub last_sequence: u64,
    pub exported_commits: u64,
    pub created_segments: u64,
    pub stored_bytes: u64,
    pub expanded_bytes: u64,
    pub manifest_checksum: String,
    pub no_op: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalListReport {
    pub version: u16,
    pub chain_id: String,
    pub state: ManifestState,
    pub baseline: ManifestArtifact,
    pub segments: Vec<ManifestArtifact>,
    pub recoverable_first_sequence: u64,
    pub recoverable_last_sequence: u64,
    pub stored_bytes: u64,
    pub expanded_bytes: u64,
    pub manifest_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalVerifyReport {
    pub version: u16,
    pub chain_id: String,
    pub artifact_count: u64,
    pub segment_count: u64,
    pub verified_first_sequence: u64,
    pub verified_last_sequence: u64,
    pub stored_bytes: u64,
    pub expanded_bytes: u64,
    pub orphan_files: Vec<String>,
    pub manifest_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalRestoreReport {
    pub version: u16,
    pub chain_id: String,
    pub restored_sequence: u64,
    pub schema_revision: u64,
    pub row_count: u64,
    pub receipt_count: u64,
    pub manifest_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalCheckpointOptions {
    pub compression: Compression,
    pub limits: ArchiveLimits,
}

impl Default for IncrementalCheckpointOptions {
    fn default() -> Self {
        Self {
            compression: Compression::Zstd,
            limits: ArchiveLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalCheckpointReport {
    pub version: u16,
    pub chain_id: String,
    pub previous_first_sequence: u64,
    pub recoverable_first_sequence: u64,
    pub recoverable_last_sequence: u64,
    pub retired_artifacts: u64,
    pub retired_bytes: u64,
    pub manifest_checksum: String,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalPruneReport {
    pub version: u16,
    pub chain_id: String,
    pub before_sequence: u64,
    pub selected_files: Vec<String>,
    pub selected_bytes: u64,
    pub recoverable_first_sequence: u64,
    pub recoverable_last_sequence: u64,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalDisableReport {
    pub version: u16,
    pub chain_id: String,
    pub recoverable_first_sequence: u64,
    pub recoverable_last_sequence: u64,
    pub lost_first_sequence: Option<u64>,
    pub lost_last_sequence: Option<u64>,
    pub applied: bool,
    pub manifest_checksum: String,
}

pub fn init(
    db: impl Into<PathBuf>,
    repo: impl AsRef<Path>,
    options: IncrementalInitOptions,
) -> Result<IncrementalInitReport> {
    validate_init_options(&options)?;
    let repo = prepare_repo(repo.as_ref())?;
    let mut engine = Engine::open_redb(db.into())?;
    engine.check_integrity()?;
    let status = engine.backup_journal_status()?;
    if repo.join(MANIFEST_NAME).exists() {
        let mut manifest = read_manifest(&repo)?;
        verify_manifest_artifacts(&repo, &manifest, &options.limits)?;
        reconcile_identity(&manifest, &status)?;
        if status.state == BackupJournalState::Disabled {
            if status.head_sequence != manifest.baseline.first_sequence {
                return Err(chain_error("database changed after the prepared baseline"));
            }
            engine.enable_backup_journal(journal_config(&manifest, &options))?;
        }
        let resumed = manifest.state == ManifestState::Prepared;
        if resumed {
            let previous = encode_manifest(manifest.clone())?;
            manifest.state = ManifestState::Active;
            manifest.journal_max_commits = None;
            manifest.journal_max_bytes = None;
            publish_manifest(&repo, Some(&previous), manifest.clone())?;
            manifest = read_manifest(&repo)?;
        }
        return Ok(IncrementalInitReport {
            version: INCREMENTAL_BACKUP_REPORT_VERSION,
            chain_id: manifest.chain_id,
            baseline_sequence: manifest.baseline.first_sequence,
            previous_storage_format: status.storage_format,
            current_storage_format: engine.backup_journal_status()?.storage_format,
            manifest_checksum: manifest.checksum,
            resumed,
        });
    }
    if status.state != BackupJournalState::Disabled {
        return Err(chain_error(
            "database has an active chain but the archive has no manifest",
        ));
    }
    let chain_id = random_chain_id()?;
    let source = engine.incremental_baseline_source(&chain_id)?;
    let encoded = encode_archive(
        ArchiveKind::Baseline,
        source.record_codec,
        options.compression,
        &source.header,
        &source.frames,
        &options.limits,
    )?;
    let relative = format!(
        "baselines/b-{}-{}.uib",
        source.header.first_sequence,
        checksum_suffix(&encoded.payload_checksum)
    );
    write_new_archive_entry(&repo, Path::new(&relative), &encoded.bytes)?;
    let artifact = ManifestArtifact {
        record_codec: (source.record_codec == 2).then_some(source.record_codec),
        path: relative,
        first_sequence: source.header.first_sequence,
        last_sequence: source.header.last_sequence,
        parent_checksum: None,
        payload_checksum: encoded.payload_checksum,
        stored_checksum: encoded.stored_checksum,
        compression: options.compression,
        stored_bytes: encoded.bytes.len() as u64,
        expanded_bytes: encoded.expanded_bytes,
    };
    let prepared = ArchiveManifest {
        format_version: if source.record_codec == 2 {
            ARTIFACT_MANIFEST_FORMAT_VERSION
        } else {
            MANIFEST_FORMAT_VERSION
        },
        archive_codec: ARCHIVE_CODEC_VERSION,
        record_codec: if source.record_codec == 2 {
            0
        } else {
            RECORD_CODEC_VERSION
        },
        chain_id: chain_id.clone(),
        database_digest: source.header.database_digest,
        state: ManifestState::Prepared,
        baseline: artifact.clone(),
        segments: Vec::new(),
        recoverable_first_sequence: artifact.first_sequence,
        recoverable_last_sequence: artifact.last_sequence,
        stored_bytes: artifact.stored_bytes,
        expanded_bytes: artifact.expanded_bytes,
        journal_max_commits: None,
        journal_max_bytes: None,
        checkpoint_previous_first_sequence: None,
        checkpoint_retired_artifacts: Vec::new(),
        checksum: String::new(),
    };
    let prepared_bytes = publish_manifest(&repo, None, prepared.clone())?;
    engine.enable_backup_journal(journal_config(&prepared, &options))?;
    let mut active = prepared;
    active.state = ManifestState::Active;
    publish_manifest(&repo, Some(&prepared_bytes), active)?;
    let manifest = read_manifest(&repo)?;
    Ok(IncrementalInitReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id,
        baseline_sequence: artifact.first_sequence,
        previous_storage_format: source.previous_storage_format,
        current_storage_format: engine.backup_journal_status()?.storage_format,
        manifest_checksum: manifest.checksum,
        resumed: false,
    })
}

pub fn export(
    db: impl Into<PathBuf>,
    repo: impl AsRef<Path>,
    options: IncrementalExportOptions,
) -> Result<IncrementalExportReport> {
    validate_export_options(&options)?;
    let repo = existing_repo(repo.as_ref())?;
    let mut engine = Engine::open_redb(db.into())?;
    engine.check_integrity()?;
    let mut manifest = read_manifest(&repo)?;
    if manifest.state == ManifestState::Prepared {
        let status = engine.backup_journal_status()?;
        reconcile_identity(&manifest, &status)?;
        let previous = encode_manifest(manifest.clone())?;
        manifest.state = ManifestState::Active;
        manifest.journal_max_commits = None;
        manifest.journal_max_bytes = None;
        publish_manifest(&repo, Some(&previous), manifest.clone())?;
        manifest = read_manifest(&repo)?;
    }
    if manifest.state != ManifestState::Active {
        return Err(chain_error("incremental export requires an active archive"));
    }
    let status = engine.backup_journal_status()?;
    reconcile_exported_prefix(&mut engine, &repo, &manifest, &status, &options.limits)?;
    let status = engine.backup_journal_status()?;
    verify_export_head(&repo, &manifest, &status, &options.limits)?;
    reconcile_identity(&manifest, &status)?;
    let available_last = status.last_retained_sequence.unwrap_or(
        status
            .exported_sequence
            .unwrap_or(manifest.recoverable_last_sequence),
    );
    let target = options.through_sequence.unwrap_or(available_last);
    if target > status.head_sequence || target > available_last {
        return Err(chain_error(
            "--through-sequence is beyond the committed journal head",
        ));
    }
    if target <= manifest.recoverable_last_sequence {
        return Ok(export_report(&manifest, None, 0, 0, true));
    }
    let first_exported = manifest.recoverable_last_sequence.checked_add(1);
    let mut commits = 0u64;
    let mut segments = 0u64;
    while manifest.recoverable_last_sequence < target {
        let first = manifest.recoverable_last_sequence + 1;
        let by_count = first.saturating_add(options.max_commits_per_segment - 1);
        let mut through = target.min(by_count);
        let (source, encoded) = loop {
            let mut source = engine
                .incremental_journal_source(Some(through))?
                .ok_or_else(|| chain_error("journal ended before the requested export sequence"))?;
            source
                .header
                .database_digest
                .clone_from(&manifest.database_digest);
            let encoded = encode_archive(
                ArchiveKind::Segment,
                source.record_codec,
                options.compression,
                &source.header,
                &source.frames,
                &options.limits,
            )?;
            if encoded.expanded_bytes <= options.max_expanded_bytes_per_segment
                || source.commit_count == 1
            {
                break (source, encoded);
            }
            through = first + (through - first) / 2;
        };
        if source.header.first_sequence != first {
            return Err(chain_error("journal export does not continue the manifest"));
        }
        let relative = format!(
            "segments/s-{first}-{through}-{}.uis",
            checksum_suffix(&encoded.payload_checksum)
        );
        write_new_archive_entry(&repo, Path::new(&relative), &encoded.bytes)?;
        let previous_manifest = encode_manifest(manifest.clone())?;
        if source.record_codec == 2 {
            promote_record_codecs(&mut manifest);
        }
        let parent = manifest
            .segments
            .last()
            .map_or(&manifest.baseline.payload_checksum, |item| {
                &item.payload_checksum
            })
            .clone();
        manifest.segments.push(ManifestArtifact {
            record_codec: (manifest.format_version == ARTIFACT_MANIFEST_FORMAT_VERSION)
                .then_some(source.record_codec),
            path: relative,
            first_sequence: first,
            last_sequence: through,
            parent_checksum: Some(parent),
            payload_checksum: encoded.payload_checksum,
            stored_checksum: encoded.stored_checksum,
            compression: options.compression,
            stored_bytes: encoded.bytes.len() as u64,
            expanded_bytes: encoded.expanded_bytes,
        });
        manifest.recoverable_last_sequence = through;
        manifest.stored_bytes = manifest
            .stored_bytes
            .saturating_add(encoded.bytes.len() as u64);
        manifest.expanded_bytes = manifest
            .expanded_bytes
            .saturating_add(encoded.expanded_bytes);
        publish_manifest(&repo, Some(&previous_manifest), manifest.clone())?;
        engine.prune_exported_journal(through, &source.last_commit_checksum)?;
        manifest = read_manifest(&repo)?;
        commits = commits.saturating_add(source.commit_count);
        segments = segments.saturating_add(1);
    }
    Ok(export_report(
        &manifest,
        first_exported,
        commits,
        segments,
        false,
    ))
}

pub fn list(repo: impl AsRef<Path>) -> Result<IncrementalListReport> {
    let repo = existing_repo(repo.as_ref())?;
    let manifest = read_manifest(&repo)?;
    Ok(list_report(manifest))
}

pub fn verify(repo: impl AsRef<Path>, limits: ArchiveLimits) -> Result<IncrementalVerifyReport> {
    let repo = existing_repo(repo.as_ref())?;
    let manifest = read_manifest(&repo)?;
    verify_manifest_artifacts(&repo, &manifest, &limits)?;
    let orphan_files = find_orphans(&repo, &manifest);
    Ok(IncrementalVerifyReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: manifest.chain_id,
        artifact_count: manifest.segments.len() as u64 + 1,
        segment_count: manifest.segments.len() as u64,
        verified_first_sequence: manifest.recoverable_first_sequence,
        verified_last_sequence: manifest.recoverable_last_sequence,
        stored_bytes: manifest.stored_bytes,
        expanded_bytes: manifest.expanded_bytes,
        orphan_files,
        manifest_checksum: manifest.checksum,
    })
}

/// Authenticate the complete exported chain before its storage header changes.
pub(crate) fn verify_header_upgrade(engine: &Engine, repo: &Path) -> Result<()> {
    let status = engine.backup_journal_status()?;
    if status.state != BackupJournalState::Active
        || status.exported_sequence != Some(status.head_sequence)
        || status.commit_count != 0
    {
        return Err(chain_error(
            "header upgrade requires an active archive exported through the current database head",
        ));
    }
    let repo = existing_repo(repo)?;
    let manifest = read_manifest(&repo)?;
    if manifest.state != ManifestState::Active
        || manifest.recoverable_last_sequence != status.head_sequence
    {
        return Err(chain_error(
            "header upgrade requires an active archive manifest at the current database head",
        ));
    }
    reconcile_identity(&manifest, &status)?;
    let limits = ArchiveLimits::default();
    verify_manifest_artifacts(&repo, &manifest, &limits)?;
    verify_export_head(&repo, &manifest, &status, &limits)?;
    let tail = manifest.segments.last().unwrap_or(&manifest.baseline);
    let decoded = read_artifact(&repo, tail, manifest.record_codec_for(tail)?, &limits)?;
    if decoded.header.storage_header != engine.stored_header()? {
        return Err(chain_error(
            "archive head storage header differs from the database",
        ));
    }
    Ok(())
}

/// Restore one declared archive sequence to a new durable database.
///
/// The database is first materialized at a sibling temporary path, checked in
/// full, and only then renamed into the requested previously nonexistent path.
pub fn restore(
    repo: impl AsRef<Path>,
    db: impl Into<PathBuf>,
    at_sequence: u64,
    limits: ArchiveLimits,
) -> Result<IncrementalRestoreReport> {
    let repo = existing_repo(repo.as_ref())?;
    let destination = db.into();
    reject_existing_restore_target(&destination)?;
    let manifest = read_manifest(&repo)?;
    if at_sequence < manifest.recoverable_first_sequence {
        return Err(Error::new(
            "E_BACKUP_BEFORE_BASELINE",
            "requested sequence is before the incremental baseline",
        ));
    }
    if at_sequence > manifest.recoverable_last_sequence {
        return Err(Error::new(
            "E_BACKUP_AFTER_HEAD",
            "requested sequence is after the sealed incremental head",
        ));
    }
    let replay = replay_to_sequence(&repo, &manifest, at_sequence, &limits)?;
    let row_count = u64::try_from(replay.rows.len())
        .map_err(|_| Error::new("E_LIMIT", "restored row count exceeds u64"))?;
    let storage_header = replay
        .storage_layout
        .map(crate::redb_storage::StorageHeader::encode_transport)
        .transpose()?;
    let (database, receipts) = replay_database(replay, &manifest)?;
    let temporary = restore_temp_path(&destination)?;
    let result = (|| {
        let mut engine = Engine::restore_redb_with_header(
            temporary.clone(),
            database,
            receipts.clone(),
            storage_header,
        )?;
        engine.check_integrity()?;
        drop(engine);
        // A rename can replace a destination created after the initial check.
        // The temporary database is a single sibling file, so hard-linking it
        // atomically reserves only a nonexistent destination before removing
        // the temporary name.
        fs::hard_link(&temporary, &destination)
            .map_err(|error| archive_error(format!("publish restored database: {error}")))?;
        let _ = fs::remove_file(&temporary);
        sync_parent_directory(&destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(IncrementalRestoreReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: manifest.chain_id,
        restored_sequence: at_sequence,
        schema_revision: Engine::open_redb(destination)?.schema_info().revision,
        row_count,
        receipt_count: u64::try_from(receipts.len()).unwrap_or(u64::MAX),
        manifest_checksum: manifest.checksum,
    })
}

pub fn checkpoint(
    db: impl Into<PathBuf>,
    repo: impl AsRef<Path>,
    options: IncrementalCheckpointOptions,
) -> Result<IncrementalCheckpointReport> {
    let db = db.into();
    let repo = existing_repo(repo.as_ref())?;
    let manifest = read_manifest(&repo)?;
    if manifest.state == ManifestState::Prepared && manifest.journal_max_commits.is_some() {
        return resume_checkpoint(&db, &repo, manifest, &options.limits);
    }
    export(&db, &repo, IncrementalExportOptions::default())?;
    let mut engine = Engine::open_redb(db.clone())?;
    engine.check_integrity()?;
    let status = engine.backup_journal_status()?;
    let mut old = read_manifest(&repo)?;
    let previous_bytes = encode_manifest(old.clone())?;
    reconcile_identity(&old, &status)?;
    if status.commit_count != 0 || status.exported_sequence != Some(status.head_sequence) {
        return Err(chain_error(
            "checkpoint requires export through the database head",
        ));
    }
    let source = engine.incremental_baseline_source(&old.chain_id)?;
    let encoded = encode_archive(
        ArchiveKind::Baseline,
        source.record_codec,
        options.compression,
        &source.header,
        &source.frames,
        &options.limits,
    )?;
    let relative = format!(
        "baselines/b-{}-{}.uib",
        source.header.first_sequence,
        checksum_suffix(&encoded.payload_checksum)
    );
    write_or_reuse_archive(&repo, Path::new(&relative), &encoded.bytes)?;
    if source.record_codec == 2 {
        promote_record_codecs(&mut old);
    }
    let baseline = ManifestArtifact {
        record_codec: (old.format_version == ARTIFACT_MANIFEST_FORMAT_VERSION)
            .then_some(source.record_codec),
        path: relative,
        first_sequence: source.header.first_sequence,
        last_sequence: source.header.last_sequence,
        parent_checksum: None,
        payload_checksum: encoded.payload_checksum,
        stored_checksum: encoded.stored_checksum,
        compression: options.compression,
        stored_bytes: encoded.bytes.len() as u64,
        expanded_bytes: encoded.expanded_bytes,
    };
    let mut prepared = old.clone();
    prepared.state = ManifestState::Prepared;
    prepared.baseline = baseline;
    prepared.segments.clear();
    prepared.recoverable_first_sequence = source.header.first_sequence;
    prepared.recoverable_last_sequence = source.header.last_sequence;
    prepared.stored_bytes = prepared.baseline.stored_bytes;
    prepared.expanded_bytes = prepared.baseline.expanded_bytes;
    prepared.journal_max_commits = Some(status.max_commits);
    prepared.journal_max_bytes = Some(status.max_bytes);
    prepared.checkpoint_previous_first_sequence = Some(old.recoverable_first_sequence);
    prepared.checkpoint_retired_artifacts = std::iter::once(old.baseline.clone())
        .chain(old.segments.iter().cloned())
        .filter(|artifact| artifact.path != prepared.baseline.path)
        .collect();
    publish_manifest(&repo, Some(&previous_bytes), prepared.clone())?;
    finish_checkpoint(&mut engine, &repo, prepared, &options.limits)
}

pub fn prune(
    repo: impl AsRef<Path>,
    before_sequence: u64,
    confirm: bool,
    limits: ArchiveLimits,
) -> Result<IncrementalPruneReport> {
    let repo = existing_repo(repo.as_ref())?;
    let manifest = read_manifest(&repo)?;
    if before_sequence > manifest.recoverable_first_sequence {
        return Err(chain_error(
            "retention cannot advance beyond the current checkpoint; create a checkpoint first",
        ));
    }
    let mut selected_files = Vec::new();
    let mut selected_bytes = 0u64;
    for relative in find_orphans(&repo, &manifest) {
        let path = Path::new(&relative);
        validate_archive_entry(&repo, path)?;
        let artifact_path = repo.join(path);
        let metadata = fs::metadata(&artifact_path)
            .map_err(|error| archive_error(format!("inspect retired archive artifact: {error}")))?;
        if metadata.len() > limits.max_stored_bytes {
            return Err(Error::new(
                "E_LIMIT",
                "retired archive artifact exceeds its stored-byte limit",
            ));
        }
        let bytes = fs::read(artifact_path)
            .map_err(|error| archive_error(format!("read retired archive artifact: {error}")))?;
        if bytes.len() < 8 || !matches!(u16::from_be_bytes([bytes[6], bytes[7]]), 1 | 2) {
            return Err(chain_error("unsupported retired artifact record codec"));
        }
        let decoded = decode_archive(&bytes, &limits)?;
        if decoded.header.chain_id == manifest.chain_id
            && decoded.header.database_digest == manifest.database_digest
            && decoded.header.last_sequence <= before_sequence
        {
            selected_bytes = selected_bytes.saturating_add(bytes.len() as u64);
            selected_files.push(relative);
        }
    }
    selected_files.sort();
    if confirm {
        for relative in &selected_files {
            fs::remove_file(repo.join(relative)).map_err(|error| {
                archive_error(format!("remove retired archive artifact: {error}"))
            })?;
        }
        sync_directory(&repo.join("baselines"))?;
        sync_directory(&repo.join("segments"))?;
    }
    Ok(IncrementalPruneReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: manifest.chain_id,
        before_sequence,
        selected_files,
        selected_bytes,
        recoverable_first_sequence: manifest.recoverable_first_sequence,
        recoverable_last_sequence: manifest.recoverable_last_sequence,
        applied: confirm,
    })
}

pub fn disable(
    db: impl Into<PathBuf>,
    repo: impl AsRef<Path>,
    discard_unexported: bool,
    confirm: bool,
) -> Result<IncrementalDisableReport> {
    let repo = existing_repo(repo.as_ref())?;
    let mut manifest = read_manifest(&repo)?;
    verify_manifest_artifacts(&repo, &manifest, &ArchiveLimits::default())?;
    let previous = encode_manifest(manifest.clone())?;
    let mut engine = Engine::open_redb(db.into())?;
    let status = engine.backup_journal_status()?;
    if manifest.state == ManifestState::Sealed && status.state == BackupJournalState::Disabled {
        return disable_report(manifest, None, None, false);
    }
    if status.state == BackupJournalState::Active {
        reconcile_identity(&manifest, &status)?;
        let lost_first = status.first_retained_sequence;
        let lost_last = status.last_retained_sequence;
        if status.commit_count != 0 && !discard_unexported {
            return Err(chain_error(
                "disable requires exporting the retained journal or explicitly discarding it",
            ));
        }
        if status.commit_count != 0 && !confirm {
            return disable_report(manifest, lost_first, lost_last, false);
        }
        engine.disable_backup_journal(discard_unexported)?;
        manifest.state = ManifestState::Sealed;
        publish_manifest(&repo, Some(&previous), manifest.clone())?;
        return disable_report(read_manifest(&repo)?, lost_first, lost_last, true);
    }
    if manifest.state != ManifestState::Active {
        return Err(chain_error(
            "database and archive disable state do not match",
        ));
    }
    manifest.state = ManifestState::Sealed;
    publish_manifest(&repo, Some(&previous), manifest.clone())?;
    disable_report(read_manifest(&repo)?, None, None, true)
}

fn resume_checkpoint(
    db: &Path,
    repo: &Path,
    prepared: ArchiveManifest,
    limits: &ArchiveLimits,
) -> Result<IncrementalCheckpointReport> {
    verify_manifest_artifacts(repo, &prepared, limits)?;
    let mut engine = Engine::open_redb(db.to_path_buf())?;
    finish_checkpoint(&mut engine, repo, prepared, limits).map(|mut report| {
        report.resumed = true;
        report
    })
}

fn finish_checkpoint(
    engine: &mut Engine,
    repo: &Path,
    mut prepared: ArchiveManifest,
    limits: &ArchiveLimits,
) -> Result<IncrementalCheckpointReport> {
    verify_manifest_artifacts(repo, &prepared, limits)?;
    let status = engine.backup_journal_status()?;
    if status.state == BackupJournalState::Active
        && status.baseline_sequence != Some(prepared.baseline.first_sequence)
    {
        if status.chain_id.as_deref() != Some(prepared.chain_id.as_str())
            || status.commit_count != 0
            || status.exported_sequence != Some(prepared.baseline.first_sequence)
        {
            return Err(chain_error(
                "checkpoint does not match the active exported journal",
            ));
        }
        engine.disable_backup_journal(false)?;
    }
    if engine.backup_journal_status()?.state == BackupJournalState::Disabled {
        engine.enable_backup_journal(checkpoint_journal_config(&prepared)?)?;
    }
    reconcile_identity(&prepared, &engine.backup_journal_status()?)?;
    let previous_first_sequence = prepared
        .checkpoint_previous_first_sequence
        .unwrap_or(prepared.recoverable_first_sequence);
    let retired_artifacts = prepared.checkpoint_retired_artifacts.len() as u64;
    let retired_bytes = prepared
        .checkpoint_retired_artifacts
        .iter()
        .fold(0u64, |total, artifact| {
            total.saturating_add(artifact.stored_bytes)
        });
    let previous = encode_manifest(prepared.clone())?;
    prepared.state = ManifestState::Active;
    prepared.journal_max_commits = None;
    prepared.journal_max_bytes = None;
    prepared.checkpoint_previous_first_sequence = None;
    prepared.checkpoint_retired_artifacts.clear();
    publish_manifest(repo, Some(&previous), prepared.clone())?;
    Ok(IncrementalCheckpointReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: prepared.chain_id,
        previous_first_sequence,
        recoverable_first_sequence: prepared.recoverable_first_sequence,
        recoverable_last_sequence: prepared.recoverable_last_sequence,
        retired_artifacts,
        retired_bytes,
        manifest_checksum: read_manifest(repo)?.checksum,
        resumed: false,
    })
}

fn checkpoint_journal_config(manifest: &ArchiveManifest) -> Result<BackupJournalConfig> {
    let mut config = BackupJournalConfig::new(
        manifest.chain_id.clone(),
        manifest.baseline.first_sequence,
        manifest.baseline.payload_checksum.clone(),
    );
    config.max_commits = manifest
        .journal_max_commits
        .ok_or_else(|| chain_error("prepared checkpoint is missing its journal commit limit"))?;
    config.max_bytes = manifest
        .journal_max_bytes
        .ok_or_else(|| chain_error("prepared checkpoint is missing its journal byte limit"))?;
    Ok(config)
}

fn write_or_reuse_archive(repo: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let path = repo.join(relative);
    match fs::read(&path) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => Err(chain_error(
            "checkpoint artifact path already contains different bytes",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_new_archive_entry(repo, relative, bytes)
        }
        Err(error) => Err(archive_error(format!("read checkpoint artifact: {error}"))),
    }
}

fn disable_report(
    manifest: ArchiveManifest,
    lost_first_sequence: Option<u64>,
    lost_last_sequence: Option<u64>,
    applied: bool,
) -> Result<IncrementalDisableReport> {
    Ok(IncrementalDisableReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: manifest.chain_id,
        recoverable_first_sequence: manifest.recoverable_first_sequence,
        recoverable_last_sequence: manifest.recoverable_last_sequence,
        lost_first_sequence,
        lost_last_sequence,
        applied,
        manifest_checksum: manifest.checksum,
    })
}

fn initial_storage_layout(
    repo: &Path,
    manifest: &ArchiveManifest,
    baseline: &super::ArchiveHeader,
    limits: &ArchiveLimits,
) -> Result<Option<crate::redb_storage::StorageLayout>> {
    if let Some(encoded) = &baseline.storage_header {
        return crate::redb_storage::StorageHeader::decode_transport(encoded).map(Some);
    }
    // A mixed chain authenticates its legacy starting layout through the first
    // explicit before/after transition; ordinary legacy chains retain their contract.
    for artifact in &manifest.segments {
        if manifest.record_codec_for(artifact)? != 2 {
            continue;
        }
        let decoded = read_artifact(repo, artifact, 2, limits)?;
        validate_decoded(
            &decoded,
            ArchiveKind::Segment,
            manifest,
            artifact,
            Some(baseline),
        )?;
        let frame = decoded
            .frames
            .iter()
            .find(|frame| frame.kind == 26)
            .ok_or_else(|| {
                chain_error("mixed chain is missing its initial storage header transition")
            })?;
        let (before, _) = crate::redb_storage::decode_header_transition(&frame.payload)?;
        let initial = legacy_archive_layout(baseline)?;
        if before.has_header()
            || !catalog_codec_continues(u32::from(initial.catalog), u32::from(before.catalog))
            || u32::from(before.value) != baseline.value_codec
            || u32::from(before.receipt) != baseline.receipt_codec
        {
            return Err(chain_error(
                "legacy baseline contradicts the initial storage header transition",
            ));
        }
        return Ok(Some(initial));
    }
    Ok(None)
}

fn legacy_archive_layout(
    header: &super::ArchiveHeader,
) -> Result<crate::redb_storage::StorageLayout> {
    let format = match (
        header.catalog_codec,
        header.value_codec,
        header.receipt_codec,
    ) {
        (6, 3, 3) => 11,
        (7, 3, 3) => 13,
        _ => {
            return Err(chain_error(
                "legacy archive has no supported native-upgrade profile",
            ));
        }
    };
    crate::redb_storage::StorageLayout::for_format(format)
        .ok_or_else(|| chain_error("legacy archive storage profile is unavailable"))
}

fn advance_legacy_catalog_layout(
    current: &mut Option<crate::redb_storage::StorageLayout>,
    value: &[u8],
) -> Result<()> {
    let Some(layout) = *current else {
        return Ok(());
    };
    if layout.has_header() {
        return Ok(());
    }
    if value.len() < 6 || &value[..4] != b"UIDC" {
        return Err(chain_error("legacy catalog record is invalid"));
    }
    let catalog = u16::from_be_bytes(value[4..6].try_into().expect("fixed slice"));
    if catalog == layout.catalog {
        return Ok(());
    }
    if layout.format == 11 && catalog == 7 {
        *current = crate::redb_storage::StorageLayout::for_format(13);
        return Ok(());
    }
    Err(chain_error(
        "legacy catalog codec does not continue its storage profile",
    ))
}

fn replay_to_sequence(
    repo: &Path,
    manifest: &ArchiveManifest,
    at_sequence: u64,
    limits: &ArchiveLimits,
) -> Result<ReplayState> {
    let baseline = read_artifact(
        repo,
        &manifest.baseline,
        manifest.record_codec_for(&manifest.baseline)?,
        limits,
    )?;
    validate_decoded(
        &baseline,
        ArchiveKind::Baseline,
        manifest,
        &manifest.baseline,
        None,
    )?;
    let meta: BaselineMeta = serde_json::from_slice(&baseline.frames[0].payload)
        .map_err(|error| archive_error(format!("decode baseline meta: {error}")))?;
    if meta.sequence != manifest.baseline.first_sequence {
        return Err(chain_error("baseline sequence does not match the manifest"));
    }
    let mut state = ReplayState {
        storage_layout: initial_storage_layout(repo, manifest, &baseline.header, limits)?,
        sequence: meta.sequence,
        schema_revision: meta.schema_revision,
        next_catalog_id: meta.next_catalog_id,
        schema_hash: meta.schema_hash,
        catalog: BTreeMap::new(),
        rows: BTreeMap::new(),
        migrations: BTreeMap::new(),
        receipts: BTreeMap::new(),
    };
    for frame in baseline.frames.iter().skip(1) {
        apply_baseline_frame(&mut state, frame, limits)?;
    }
    if at_sequence == state.sequence {
        return Ok(state);
    }
    let mut expected_parent = manifest.baseline.payload_checksum.clone();
    for artifact in &manifest.segments {
        if artifact.first_sequence > at_sequence {
            break;
        }
        let segment = read_artifact(repo, artifact, manifest.record_codec_for(artifact)?, limits)?;
        validate_decoded(
            &segment,
            ArchiveKind::Segment,
            manifest,
            artifact,
            Some(&baseline.header),
        )?;
        expected_parent = replay_segment(
            &mut state,
            &segment.frames,
            at_sequence,
            &expected_parent,
            limits,
        )?;
        if state.sequence == at_sequence {
            return Ok(state);
        }
    }
    let _ = expected_parent;
    Err(Error::new(
        "E_BACKUP_CHAIN",
        "requested sequence is absent from the verified archive chain",
    ))
}

fn apply_baseline_frame(
    state: &mut ReplayState,
    frame: &super::ArchiveFrame,
    limits: &ArchiveLimits,
) -> Result<()> {
    let (key, value) = split_write_frame(&frame.payload, limits)?;
    let destination = match frame.kind {
        2 => &mut state.catalog,
        3 => &mut state.rows,
        4 => &mut state.migrations,
        5 => &mut state.receipts,
        _ => return Err(chain_error("baseline contains an invalid data frame")),
    };
    if destination.insert(key.to_vec(), value.to_vec()).is_some() {
        return Err(chain_error("baseline contains duplicate durable keys"));
    }
    Ok(())
}

fn replay_segment(
    state: &mut ReplayState,
    frames: &[super::ArchiveFrame],
    target: u64,
    initial_parent: &str,
    limits: &ArchiveLimits,
) -> Result<String> {
    let mut parent = initial_parent.to_owned();
    let mut active: Option<CommitMeta> = None;
    let mut digest: Option<Sha256> = None;
    let mut ordinal = 0_u64;
    for frame in frames {
        if frame.kind == 16 {
            let meta = decode_commit_meta(&frame.payload)?;
            validate_commit_layout(state.storage_layout, meta.storage_layout)?;
            if active.is_some() || meta.sequence != state.sequence.saturating_add(1) {
                return Err(chain_error("segment commit sequence is not contiguous"));
            }
            let mut hash = Sha256::new();
            hash.update(COMMIT_DOMAIN);
            hash.update(parent.as_bytes());
            hash.update(journal_key(meta.sequence, 0));
            hash.update(journal_value(frame.kind, &frame.payload));
            digest = Some(hash);
            active = Some(meta);
            ordinal = 1;
            continue;
        }
        if frame.kind == 25 {
            let meta = active
                .take()
                .ok_or_else(|| chain_error("segment end is misplaced"))?;
            let actual = format!(
                "sha256:{:x}",
                digest
                    .take()
                    .ok_or_else(|| chain_error("segment checksum is missing"))?
                    .finalize()
            );
            let stored = std::str::from_utf8(&frame.payload)
                .map_err(|_| chain_error("segment checksum is not UTF-8"))?;
            if stored != actual {
                return Err(chain_error("segment commit checksum mismatch"));
            }
            parent = actual;
            if meta.sequence <= target {
                state.sequence = meta.sequence;
                state.schema_revision = meta.schema_revision;
                state.next_catalog_id = meta.next_catalog_id;
                state.schema_hash = meta.schema_hash;
            }
            if state.sequence == target {
                return Ok(parent);
            }
            ordinal = 0;
            continue;
        }
        let meta = active
            .as_ref()
            .ok_or_else(|| chain_error("segment change is outside a commit"))?;
        let hash = digest
            .as_mut()
            .ok_or_else(|| chain_error("segment checksum is missing"))?;
        hash.update(journal_key(meta.sequence, ordinal));
        hash.update(journal_value(frame.kind, &frame.payload));
        ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| Error::new("E_LIMIT", "archive ordinal overflow"))?;
        if meta.sequence <= target {
            apply_delta_frame(state, frame, limits)?;
        }
    }
    if active.is_some() {
        return Err(chain_error("segment ends inside a commit"));
    }
    Ok(parent)
}

fn apply_delta_frame(
    state: &mut ReplayState,
    frame: &super::ArchiveFrame,
    limits: &ArchiveLimits,
) -> Result<()> {
    if frame.kind == 26 {
        let (before, after) = crate::redb_storage::decode_header_transition(&frame.payload)?;
        if state.storage_layout != Some(before) {
            return Err(chain_error(
                "storage header transition does not match replay state",
            ));
        }
        state.storage_layout = Some(after);
        return Ok(());
    }
    if frame.kind == 18 {
        let (_, value) = split_write_frame(&frame.payload, limits)?;
        advance_legacy_catalog_layout(&mut state.storage_layout, value)?;
    }
    let (destination, delete) = match frame.kind {
        17 => (&mut state.catalog, true),
        18 => (&mut state.catalog, false),
        19 => (&mut state.rows, true),
        20 => (&mut state.rows, false),
        21 => (&mut state.migrations, true),
        22 => (&mut state.migrations, false),
        23 => (&mut state.receipts, true),
        24 => (&mut state.receipts, false),
        _ => return Err(chain_error("segment contains an invalid change frame")),
    };
    if delete {
        let key = split_delete_frame(&frame.payload, limits)?;
        if destination.remove(key).is_none() {
            return Err(chain_error("segment deletes a missing durable key"));
        }
    } else {
        let (key, value) = split_write_frame(&frame.payload, limits)?;
        destination.insert(key.to_vec(), value.to_vec());
    }
    Ok(())
}

fn replay_database(
    state: ReplayState,
    _manifest: &ArchiveManifest,
) -> Result<(Database, ReceiptMap)> {
    let catalog_value = state
        .catalog
        .values()
        .next()
        .ok_or_else(|| chain_error("baseline catalog is empty"))?;
    if catalog_value.len() < 6 {
        return Err(chain_error("stored catalog entry is truncated"));
    }
    let catalog_codec = u16::from_be_bytes(catalog_value[4..6].try_into().expect("fixed slice"));
    if state
        .storage_layout
        .is_some_and(|layout| layout.catalog != catalog_codec)
    {
        return Err(chain_error(
            "catalog codec contradicts restored storage header",
        ));
    }
    let receipt_codec = state
        .receipts
        .values()
        .next()
        .map(|value| {
            if value.len() < 6 {
                Err(chain_error("stored receipt is truncated"))
            } else {
                Ok(u16::from_be_bytes(
                    value[4..6].try_into().expect("fixed slice"),
                ))
            }
        })
        .transpose()?
        .unwrap_or(state.storage_layout.map_or(2, |layout| layout.receipt));
    if state
        .storage_layout
        .is_some_and(|layout| layout.receipt != receipt_codec)
    {
        return Err(chain_error(
            "receipt codec contradicts restored storage header",
        ));
    }
    let entries = state
        .catalog
        .iter()
        .map(|(key, value)| decode_catalog_entry(key, value, catalog_codec))
        .collect::<Result<Vec<_>>>()?;
    let rows = state
        .rows
        .into_iter()
        .map(|(key, value)| {
            let (table_id, row_id) = decode_row_key(&key)?;
            Ok((table_id, row_id, value))
        })
        .collect::<Result<Vec<_>>>()?;
    let migrations = state
        .migrations
        .iter()
        .enumerate()
        .map(|(expected, (key, value))| {
            if key.as_slice() != (expected as u64).to_be_bytes() {
                return Err(chain_error("migration ledger sequence is not contiguous"));
            }
            decode_migration_entry(value)
        })
        .collect::<Result<Vec<MigrationEntry>>>()?;
    let mut receipts = ReceiptMap::new();
    for (key, value) in state.receipts {
        let key = String::from_utf8(key)
            .map_err(|_| chain_error("idempotency receipt key is not UTF-8"))?;
        if receipts
            .insert(key, decode_receipt(&value, receipt_codec)?)
            .is_some()
        {
            return Err(chain_error("duplicate idempotency receipt key"));
        }
    }
    let identity = crate::pagination::CursorIdentity::generate()?;
    let meta = DurableMeta {
        sequence: state.sequence,
        schema_revision: state.schema_revision,
        next_catalog_id: state.next_catalog_id,
        schema_hash: state.schema_hash,
        cursor_instance_id: *identity.instance_id(),
        cursor_secret: *identity.secret(),
    };
    Ok((
        Database::from_durable(meta, entries, rows, migrations)?,
        receipts,
    ))
}

fn decode_commit_meta(payload: &[u8]) -> Result<CommitMeta> {
    if payload.len() < 8 {
        return Err(chain_error("segment commit begin is truncated"));
    }
    let sequence = u64::from_be_bytes(payload[..8].try_into().expect("fixed slice"));
    let (_, remaining) = take_prefixed(&payload[8..], usize::MAX)?;
    if remaining.len() < 16 {
        return Err(chain_error("segment commit metadata is truncated"));
    }
    let schema_revision = u64::from_be_bytes(remaining[..8].try_into().expect("fixed slice"));
    let next_catalog_id = u64::from_be_bytes(remaining[8..16].try_into().expect("fixed slice"));
    let (schema_hash, tail) = take_prefixed(&remaining[16..], 1024 * 1024)?;
    if tail.len() < 32 || next_catalog_id == 0 {
        return Err(chain_error("segment commit metadata is invalid"));
    }
    let schema_hash = std::str::from_utf8(schema_hash)
        .map_err(|_| chain_error("segment schema hash is not UTF-8"))?
        .to_owned();
    if !schema_hash.starts_with("sha256:") {
        return Err(chain_error("segment schema hash is invalid"));
    }
    Ok(CommitMeta {
        storage_layout: crate::redb_storage::journal_begin_storage_layout(payload)?,
        sequence,
        schema_revision,
        next_catalog_id,
        schema_hash,
    })
}

fn split_delete_frame<'a>(payload: &'a [u8], limits: &ArchiveLimits) -> Result<&'a [u8]> {
    let (key, trailing) = take_prefixed(payload, limits.max_key_bytes)?;
    if !trailing.is_empty() {
        return Err(chain_error("delete frame has trailing bytes"));
    }
    Ok(key)
}

fn split_write_frame<'a>(
    payload: &'a [u8],
    limits: &ArchiveLimits,
) -> Result<(&'a [u8], &'a [u8])> {
    let (key, remaining) = take_prefixed(payload, limits.max_key_bytes)?;
    let (value, trailing) = take_prefixed(remaining, limits.max_value_bytes)?;
    if !trailing.is_empty() {
        return Err(chain_error("write frame has trailing bytes"));
    }
    Ok((key, value))
}

fn take_prefixed(bytes: &[u8], max: usize) -> Result<(&[u8], &[u8])> {
    if bytes.len() < 4 {
        return Err(chain_error("length-delimited frame is truncated"));
    }
    let length = u32::from_be_bytes(bytes[..4].try_into().expect("fixed slice")) as usize;
    if length > max {
        return Err(Error::new(
            "E_LIMIT",
            "archive frame value exceeds its limit",
        ));
    }
    let end = 4usize
        .checked_add(length)
        .ok_or_else(|| Error::new("E_LIMIT", "archive frame length overflows"))?;
    let value = bytes
        .get(4..end)
        .ok_or_else(|| chain_error("length-delimited frame is truncated"))?;
    Ok((value, &bytes[end..]))
}

fn reject_existing_restore_target(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(Error::new(
            "E_BACKUP",
            "incremental restore target already exists",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(archive_error(format!("inspect restore target: {error}"))),
    }
}

fn restore_parent_directory(destination: &Path) -> Result<&Path> {
    destination
        .parent()
        // Path::parent returns an empty path for a filename in the cwd. Both
        // temporary publication and directory sync must resolve it alike.
        .map(|parent| {
            if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            }
        })
        .ok_or_else(|| {
            Error::new(
                "E_BACKUP",
                "incremental restore target must have a parent directory",
            )
        })
}

fn restore_temp_path(destination: &Path) -> Result<PathBuf> {
    let parent = restore_parent_directory(destination)?;
    let metadata = fs::symlink_metadata(parent)
        .map_err(|error| archive_error(format!("inspect restore parent: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            "E_BACKUP",
            "incremental restore parent must be a real directory",
        ));
    }
    let name = destination.file_name().ok_or_else(|| {
        Error::new(
            "E_BACKUP",
            "incremental restore target must name a database file",
        )
    })?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| archive_error(format!("generate restore temporary name: {error}")))?;
    Ok(parent.join(format!(
        ".{}-{:x}.tmp",
        name.to_string_lossy(),
        u128::from_be_bytes(nonce)
    )))
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = restore_parent_directory(path)?;
    let directory = fs::File::open(parent)
        .map_err(|error| archive_error(format!("open restore parent for sync: {error}")))?;
    directory
        .sync_all()
        .map_err(|error| archive_error(format!("sync restore parent: {error}")))
}

fn verify_manifest_artifacts(
    repo: &Path,
    manifest: &ArchiveManifest,
    limits: &ArchiveLimits,
) -> Result<()> {
    let baseline = read_artifact(
        repo,
        &manifest.baseline,
        manifest.record_codec_for(&manifest.baseline)?,
        limits,
    )?;
    validate_decoded(
        &baseline,
        ArchiveKind::Baseline,
        manifest,
        &manifest.baseline,
        None,
    )?;
    let meta: BaselineMeta = serde_json::from_slice(&baseline.frames[0].payload)
        .map_err(|error| archive_error(format!("decode baseline meta: {error}")))?;
    if meta.sequence != manifest.baseline.first_sequence
        || meta.next_catalog_id == 0
        || !meta.schema_hash.starts_with("sha256:")
    {
        return Err(chain_error("baseline meta does not match the manifest"));
    }
    let mut schema = SchemaTransition {
        storage_layout: initial_storage_layout(repo, manifest, &baseline.header, limits)?,
        revision: meta.schema_revision,
        hash: meta.schema_hash,
    };
    let mut commit_parent = manifest.baseline.payload_checksum.clone();
    for artifact in &manifest.segments {
        let decoded = read_artifact(repo, artifact, manifest.record_codec_for(artifact)?, limits)?;
        validate_decoded(
            &decoded,
            ArchiveKind::Segment,
            manifest,
            artifact,
            Some(&baseline.header),
        )?;
        commit_parent = verify_segment_commits(
            &decoded.frames,
            decoded.header.first_sequence,
            &commit_parent,
            Some(&mut schema),
        )?;
        if decoded.record_codec == 2
            && schema
                .storage_layout
                .map(crate::redb_storage::StorageHeader::encode_transport)
                .transpose()?
                != decoded.header.storage_header
        {
            return Err(chain_error(
                "segment header differs from its committed storage transition",
            ));
        }
    }
    let _ = commit_parent;
    Ok(())
}

/// Export only needs the verified baseline and current archive head. Full-chain
/// validation remains the explicit responsibility of `incremental verify`.
fn verify_export_head(
    repo: &Path,
    manifest: &ArchiveManifest,
    status: &super::BackupJournalStatus,
    limits: &ArchiveLimits,
) -> Result<()> {
    let baseline = read_artifact(
        repo,
        &manifest.baseline,
        manifest.record_codec_for(&manifest.baseline)?,
        limits,
    )?;
    validate_decoded(
        &baseline,
        ArchiveKind::Baseline,
        manifest,
        &manifest.baseline,
        None,
    )?;
    let meta: BaselineMeta = serde_json::from_slice(&baseline.frames[0].payload)
        .map_err(|error| archive_error(format!("decode baseline meta: {error}")))?;
    if meta.sequence != manifest.baseline.first_sequence
        || meta.next_catalog_id == 0
        || !meta.schema_hash.starts_with("sha256:")
    {
        return Err(chain_error("baseline meta does not match the manifest"));
    }
    let exported = status
        .exported_sequence
        .ok_or_else(|| chain_error("database has no active export head"))?;
    let exported_checksum = status
        .exported_checksum
        .as_deref()
        .ok_or_else(|| chain_error("database has no active export checksum"))?;
    let Some(tail) = manifest.segments.last() else {
        if exported != manifest.baseline.last_sequence
            || exported_checksum != manifest.baseline.payload_checksum
        {
            return Err(chain_error(
                "baseline does not match the database export head",
            ));
        }
        return Ok(());
    };
    if exported != tail.last_sequence {
        return Err(chain_error("archive and database export heads differ"));
    }
    let decoded = read_artifact(repo, tail, manifest.record_codec_for(tail)?, limits)?;
    validate_decoded(
        &decoded,
        ArchiveKind::Segment,
        manifest,
        tail,
        Some(&baseline.header),
    )?;
    let initial_parent = segment_initial_parent(&decoded.frames)?;
    let actual =
        verify_segment_commits(&decoded.frames, tail.first_sequence, &initial_parent, None)?;
    if actual != exported_checksum {
        return Err(chain_error(
            "archive tail does not match the database export checksum",
        ));
    }
    Ok(())
}

fn segment_initial_parent(frames: &[super::ArchiveFrame]) -> Result<String> {
    let first = frames
        .first()
        .filter(|frame| frame.kind == 16)
        .ok_or_else(|| chain_error("segment is missing its first commit"))?;
    let (_, parent, _, _) = begin_identity(&first.payload)?;
    Ok(parent)
}

fn read_artifact(
    repo: &Path,
    artifact: &ManifestArtifact,
    record_codec: u16,
    limits: &ArchiveLimits,
) -> Result<super::DecodedArchive> {
    validate_archive_entry(repo, Path::new(&artifact.path))?;
    let path = repo.join(&artifact.path);
    let metadata = fs::metadata(&path)
        .map_err(|error| archive_error(format!("inspect archive artifact: {error}")))?;
    if metadata.len() > limits.max_stored_bytes {
        return Err(Error::new(
            "E_LIMIT",
            "archive artifact exceeds its stored-byte limit",
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| archive_error(format!("read archive artifact: {error}")))?;
    if bytes.len() >= 8 && bytes[6..8] != record_codec.to_be_bytes() {
        return Err(chain_error(
            "archive record codec does not match its artifact declaration",
        ));
    }
    let decoded = decode_archive(&bytes, limits)?;
    if decoded.payload_checksum != artifact.payload_checksum
        || decoded.stored_checksum != artifact.stored_checksum
        || decoded.expanded_bytes != artifact.expanded_bytes
        || bytes.len() as u64 != artifact.stored_bytes
        || decoded.compression != artifact.compression
    {
        return Err(chain_error(
            "archive artifact does not match its manifest entry",
        ));
    }
    Ok(decoded)
}

fn validate_decoded(
    decoded: &super::DecodedArchive,
    kind: ArchiveKind,
    manifest: &ArchiveManifest,
    artifact: &ManifestArtifact,
    content_codecs: Option<&super::ArchiveHeader>,
) -> Result<()> {
    if decoded.record_codec == 1 {
        for frame in decoded.frames.iter().filter(|frame| frame.kind == 16) {
            if crate::redb_storage::journal_begin_storage_layout(&frame.payload)?.is_some() {
                return Err(chain_error(
                    "record codec 1 cannot carry commit storage headers",
                ));
            }
        }
    }
    if decoded.kind != kind
        || decoded.record_codec != manifest.record_codec_for(artifact)?
        || decoded.header.chain_id != manifest.chain_id
        || decoded.header.database_digest != manifest.database_digest
        || decoded.header.first_sequence != artifact.first_sequence
        || decoded.header.last_sequence != artifact.last_sequence
        || content_codecs
            .filter(|_| decoded.record_codec != 2)
            .is_some_and(|expected| {
                !catalog_codec_continues(expected.catalog_codec, decoded.header.catalog_codec)
                    || decoded.header.value_codec != expected.value_codec
                    || decoded.header.receipt_codec != expected.receipt_codec
                    || (decoded.header.catalog_codec != expected.catalog_codec
                        && (expected.value_codec != 3 || expected.receipt_codec != 3))
            })
    {
        return Err(chain_error("archive header does not match the manifest"));
    }
    if decoded.record_codec == 2
        && let Some(transition) = decoded.frames.iter().rev().find(|frame| frame.kind == 26)
    {
        let (_, after) = crate::redb_storage::decode_header_transition(&transition.payload)?;
        if decoded.header.storage_header.as_deref()
            != Some(crate::redb_storage::StorageHeader::encode_transport(after)?.as_str())
        {
            return Err(chain_error(
                "archive header differs from its final storage transition",
            ));
        }
    }
    Ok(())
}

fn catalog_codec_continues(baseline: u32, segment: u32) -> bool {
    // A format-11 -> format-13 upgrade is a journaled full catalog rewrite.
    // Later segments advertise the new catalog reader even when their writes
    // only touch rows. Preserve the immutable baseline header; authenticated
    // commit frames and replay_database validate the actual complete catalog.
    baseline == segment || (baseline == 6 && segment == 7)
}

fn verify_segment_commits(
    frames: &[super::ArchiveFrame],
    first: u64,
    initial_parent: &str,
    mut schema: Option<&mut SchemaTransition>,
) -> Result<String> {
    let mut expected_sequence = first;
    let mut expected_parent = initial_parent.to_owned();
    let mut ordinal = 0u64;
    let mut digest: Option<Sha256> = None;
    for frame in frames {
        let key = journal_key(expected_sequence, ordinal);
        let value = journal_value(frame.kind, &frame.payload);
        if frame.kind == 16 {
            let (sequence, parent, revision, schema_hash) = begin_identity(&frame.payload)?;
            if sequence != expected_sequence || parent != expected_parent || ordinal != 0 {
                return Err(chain_error(
                    "segment commit sequence or parent checksum is inconsistent",
                ));
            }
            if let Some(current) = schema.as_deref_mut() {
                let commit_layout =
                    crate::redb_storage::journal_begin_storage_layout(&frame.payload)?;
                validate_commit_layout(current.storage_layout, commit_layout)?;
                if revision < current.revision
                    || (revision == current.revision && schema_hash != current.hash)
                {
                    return Err(chain_error("segment schema transition is inconsistent"));
                }
                current.revision = revision;
                current.hash = schema_hash;
            }
            let mut hash = Sha256::new();
            hash.update(COMMIT_DOMAIN);
            hash.update(parent.as_bytes());
            hash.update(key);
            hash.update(value);
            digest = Some(hash);
            ordinal = 1;
        } else if frame.kind == 25 {
            let stored = std::str::from_utf8(&frame.payload)
                .map_err(|_| chain_error("segment commit checksum is not UTF-8"))?;
            let actual = format!(
                "sha256:{:x}",
                digest
                    .take()
                    .ok_or_else(|| chain_error("segment commit end is misplaced"))?
                    .finalize()
            );
            if stored != actual {
                return Err(chain_error("segment commit checksum mismatch"));
            }
            expected_parent = actual;
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "archive sequence overflow"))?;
            ordinal = 0;
        } else {
            if frame.kind == 18
                && let Some(current) = schema.as_deref_mut()
            {
                let (_, tail) = take_prefixed(&frame.payload, usize::MAX)?;
                let (value, _) = take_prefixed(tail, usize::MAX)?;
                advance_legacy_catalog_layout(&mut current.storage_layout, value)?;
            }
            if frame.kind == 26 {
                let (before, after) =
                    crate::redb_storage::decode_header_transition(&frame.payload)?;
                if let Some(current) = schema.as_deref_mut() {
                    if current.storage_layout != Some(before) {
                        return Err(chain_error(
                            "storage header transition does not match verified chain",
                        ));
                    }
                    current.storage_layout = Some(after);
                }
            }
            let hash = digest
                .as_mut()
                .ok_or_else(|| chain_error("segment change is outside a commit"))?;
            hash.update(key);
            hash.update(value);
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "journal ordinal overflow"))?;
        }
    }
    Ok(expected_parent)
}

fn validate_commit_layout(
    current: Option<crate::redb_storage::StorageLayout>,
    declared: Option<crate::redb_storage::StorageLayout>,
) -> Result<()> {
    if declared.is_some() && declared != current
        || declared.is_none() && current.is_some_and(|layout| layout.has_header())
    {
        return Err(chain_error(
            "commit storage header differs from its replay state",
        ));
    }
    Ok(())
}

fn reconcile_exported_prefix(
    engine: &mut Engine,
    repo: &Path,
    manifest: &ArchiveManifest,
    status: &super::BackupJournalStatus,
    limits: &ArchiveLimits,
) -> Result<()> {
    let exported = status
        .exported_sequence
        .ok_or_else(|| chain_error("database has no active export head"))?;
    if exported == manifest.recoverable_last_sequence {
        return Ok(());
    }
    if exported > manifest.recoverable_last_sequence {
        return Err(chain_error(
            "database export head is ahead of the archive manifest",
        ));
    }
    let segment = manifest
        .segments
        .last()
        .ok_or_else(|| chain_error("archive is ahead without a segment"))?;
    if exported.saturating_add(1) != segment.first_sequence {
        return Err(chain_error(
            "archive/database export reconciliation is not contiguous",
        ));
    }
    let parent = status
        .exported_checksum
        .as_deref()
        .ok_or_else(|| chain_error("database has no active export checksum"))?;
    let baseline = read_artifact(
        repo,
        &manifest.baseline,
        manifest.record_codec_for(&manifest.baseline)?,
        limits,
    )?;
    validate_decoded(
        &baseline,
        ArchiveKind::Baseline,
        manifest,
        &manifest.baseline,
        None,
    )?;
    let decoded = read_artifact(repo, segment, manifest.record_codec_for(segment)?, limits)?;
    validate_decoded(
        &decoded,
        ArchiveKind::Segment,
        manifest,
        segment,
        Some(&baseline.header),
    )?;
    let commit_checksum =
        verify_segment_commits(&decoded.frames, segment.first_sequence, parent, None)?;
    engine.prune_exported_journal(segment.last_sequence, &commit_checksum)?;
    Ok(())
}

fn reconcile_identity(
    manifest: &ArchiveManifest,
    status: &super::BackupJournalStatus,
) -> Result<()> {
    match status.state {
        BackupJournalState::Disabled if manifest.state == ManifestState::Prepared => Ok(()),
        BackupJournalState::Active
            if status.chain_id.as_deref() == Some(manifest.chain_id.as_str())
                && status.baseline_sequence == Some(manifest.baseline.first_sequence) =>
        {
            Ok(())
        }
        _ => Err(chain_error(
            "archive manifest and database backup chain do not match",
        )),
    }
}

fn journal_config(
    manifest: &ArchiveManifest,
    options: &IncrementalInitOptions,
) -> BackupJournalConfig {
    let mut config = BackupJournalConfig::new(
        manifest.chain_id.clone(),
        manifest.baseline.first_sequence,
        manifest.baseline.payload_checksum.clone(),
    );
    config.max_commits = options.journal_max_commits;
    config.max_bytes = options.journal_max_bytes;
    config
}

fn publish_manifest(
    repo: &Path,
    expected: Option<&[u8]>,
    manifest: ArchiveManifest,
) -> Result<Vec<u8>> {
    let bytes = encode_manifest(manifest)?;
    let path = repo.join(MANIFEST_NAME);
    match (expected, fs::read(&path)) {
        (None, Ok(_)) => return Err(chain_error("archive manifest already exists")),
        (Some(expected), Ok(current)) if current != expected => {
            return Err(chain_error("archive manifest changed concurrently"));
        }
        (Some(_), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(chain_error("archive manifest disappeared"));
        }
        (_, Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(archive_error(format!("read archive manifest: {error}")));
        }
        _ => {}
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| archive_error(format!("generate manifest nonce: {error}")))?;
    let temp = repo.join(format!(".manifest-{:x}.tmp", u128::from_be_bytes(nonce)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| archive_error(format!("create temporary manifest: {error}")))?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(archive_error(format!("write temporary manifest: {error}")));
    }
    drop(file);
    decode_manifest(
        &fs::read(&temp)
            .map_err(|error| archive_error(format!("verify temporary manifest: {error}")))?,
    )?;
    fs::rename(&temp, &path)
        .map_err(|error| archive_error(format!("publish archive manifest: {error}")))?;
    sync_directory(repo)?;
    Ok(bytes)
}

fn prepare_repo(path: &Path) -> Result<PathBuf> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(archive_error("archive repository must be a real directory"));
        }
    } else {
        fs::create_dir(path)
            .map_err(|error| archive_error(format!("create archive repository: {error}")))?;
    }
    fs::create_dir_all(path.join("baselines"))
        .map_err(|error| archive_error(format!("create baseline directory: {error}")))?;
    fs::create_dir_all(path.join("segments"))
        .map_err(|error| archive_error(format!("create segment directory: {error}")))?;
    existing_repo(path)
}

fn existing_repo(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| archive_error(format!("inspect archive repository: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(archive_error("archive repository must be a real directory"));
    }
    path.canonicalize()
        .map_err(|error| archive_error(format!("resolve archive repository: {error}")))
}

fn promote_record_codecs(manifest: &mut ArchiveManifest) {
    if manifest.format_version == ARTIFACT_MANIFEST_FORMAT_VERSION {
        return;
    }
    for artifact in std::iter::once(&mut manifest.baseline)
        .chain(&mut manifest.segments)
        .chain(&mut manifest.checkpoint_retired_artifacts)
    {
        artifact.record_codec = Some(manifest.record_codec);
    }
    manifest.format_version = ARTIFACT_MANIFEST_FORMAT_VERSION;
    manifest.record_codec = 0;
}

fn read_manifest(repo: &Path) -> Result<ArchiveManifest> {
    let path = repo.join(MANIFEST_NAME);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| archive_error(format!("inspect archive manifest: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(archive_error(
            "archive manifest must be a bounded regular file",
        ));
    }
    let manifest = decode_manifest(
        &fs::read(path)
            .map_err(|error| archive_error(format!("read archive manifest: {error}")))?,
    )?;
    // Container primitives can describe opaque nonzero record codecs. Business
    // workflows must not interpret an unknown codec as today's frame semantics.
    for artifact in std::iter::once(&manifest.baseline)
        .chain(&manifest.segments)
        .chain(&manifest.checkpoint_retired_artifacts)
    {
        let codec = manifest.record_codec_for(artifact)?;
        if !matches!(codec, 1 | 2)
            || (codec == 2 && manifest.format_version != ARTIFACT_MANIFEST_FORMAT_VERSION)
        {
            return Err(chain_error("unsupported artifact record codec"));
        }
    }
    Ok(manifest)
}

fn list_report(manifest: ArchiveManifest) -> IncrementalListReport {
    IncrementalListReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: manifest.chain_id,
        state: manifest.state,
        baseline: manifest.baseline,
        segments: manifest.segments,
        recoverable_first_sequence: manifest.recoverable_first_sequence,
        recoverable_last_sequence: manifest.recoverable_last_sequence,
        stored_bytes: manifest.stored_bytes,
        expanded_bytes: manifest.expanded_bytes,
        manifest_checksum: manifest.checksum,
    }
}

fn find_orphans(repo: &Path, manifest: &ArchiveManifest) -> Vec<String> {
    let referenced = std::iter::once(manifest.baseline.path.as_str())
        .chain(manifest.segments.iter().map(|item| item.path.as_str()))
        .collect::<BTreeSet<_>>();
    let mut orphans = Vec::new();
    for directory in ["baselines", "segments"] {
        let Ok(entries) = fs::read_dir(repo.join(directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let relative = format!("{directory}/{}", entry.file_name().to_string_lossy());
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && !referenced.contains(relative.as_str())
            {
                orphans.push(relative);
            }
        }
    }
    orphans.sort();
    orphans
}

fn export_report(
    manifest: &ArchiveManifest,
    first: Option<u64>,
    commits: u64,
    segments: u64,
    no_op: bool,
) -> IncrementalExportReport {
    IncrementalExportReport {
        version: INCREMENTAL_BACKUP_REPORT_VERSION,
        chain_id: manifest.chain_id.clone(),
        first_sequence: first,
        last_sequence: manifest.recoverable_last_sequence,
        exported_commits: commits,
        created_segments: segments,
        stored_bytes: manifest.stored_bytes,
        expanded_bytes: manifest.expanded_bytes,
        manifest_checksum: manifest.checksum.clone(),
        no_op,
    }
}

fn validate_init_options(options: &IncrementalInitOptions) -> Result<()> {
    if options.journal_max_commits == 0 || options.journal_max_bytes == 0 {
        return Err(Error::new("E_LIMIT", "journal limits must be positive"));
    }
    Ok(())
}

fn validate_export_options(options: &IncrementalExportOptions) -> Result<()> {
    if options.max_commits_per_segment == 0 || options.max_expanded_bytes_per_segment == 0 {
        return Err(Error::new("E_LIMIT", "segment limits must be positive"));
    }
    Ok(())
}

fn random_chain_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| archive_error(format!("generate chain ID: {error}")))?;
    Ok(format!("{:x}", u128::from_be_bytes(bytes)))
}

fn checksum_suffix(checksum: &str) -> &str {
    checksum.strip_prefix("sha256:").unwrap_or(checksum)
}

fn journal_key(sequence: u64, ordinal: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&sequence.to_be_bytes());
    key[8..].copy_from_slice(&ordinal.to_be_bytes());
    key
}

fn journal_value(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(7 + payload.len());
    value.extend_from_slice(b"UIDJ");
    value.extend_from_slice(&1u16.to_be_bytes());
    value.push(kind);
    value.extend_from_slice(payload);
    value
}

fn begin_identity(payload: &[u8]) -> Result<(u64, String, u64, String)> {
    if payload.len() < 12 {
        return Err(chain_error("segment commit begin is truncated"));
    }
    let sequence = u64::from_be_bytes(payload[..8].try_into().expect("fixed slice"));
    let len = u32::from_be_bytes(payload[8..12].try_into().expect("fixed slice")) as usize;
    let end = 12usize
        .checked_add(len)
        .ok_or_else(|| Error::new("E_LIMIT", "commit parent length overflow"))?;
    let parent = payload
        .get(12..end)
        .ok_or_else(|| chain_error("segment commit parent is truncated"))?;
    let tail = payload
        .get(end..)
        .ok_or_else(|| chain_error("segment commit metadata is truncated"))?;
    if tail.len() < 20 {
        return Err(chain_error("segment commit schema metadata is truncated"));
    }
    let revision = u64::from_be_bytes(tail[..8].try_into().expect("fixed slice"));
    let hash_len = u32::from_be_bytes(tail[16..20].try_into().expect("fixed slice")) as usize;
    let hash_end = 20usize
        .checked_add(hash_len)
        .ok_or_else(|| Error::new("E_LIMIT", "schema hash length overflow"))?;
    let schema_hash = std::str::from_utf8(
        tail.get(20..hash_end)
            .ok_or_else(|| chain_error("segment commit schema hash is truncated"))?,
    )
    .map_err(|_| chain_error("segment commit schema hash is not UTF-8"))?
    .to_owned();
    Ok((
        sequence,
        std::str::from_utf8(parent)
            .map_err(|_| chain_error("segment commit parent is not UTF-8"))?
            .to_owned(),
        revision,
        schema_hash,
    ))
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| archive_error(format!("sync archive directory: {error}")))?;
    Ok(())
}

fn archive_error(message: impl Into<String>) -> Error {
    Error::new("E_BACKUP_ARCHIVE", message)
}

fn chain_error(message: impl Into<String>) -> Error {
    Error::new("E_BACKUP_CHAIN", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let mut nonce = [0u8; 8];
            getrandom::fill(&mut nonce).unwrap();
            let path = std::env::temp_dir().join(format!(
                "unionid-incremental-reconcile-{}-{:x}",
                std::process::id(),
                u64::from_be_bytes(nonce)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn database(path: &Path) -> Engine {
        let mut engine = Engine::open_redb(path.to_path_buf()).unwrap();
        assert!(
            engine
                .execute(
                    "type Item = { id int, label text }\n\
                     table items Item\n  key id\n\
                     insert items {id = 1, label = \"one\"}"
                )
                .ok
        );
        engine
    }

    #[test]
    fn legacy_reference_codec_transition_continues_into_native_header_recovery() {
        let temp = Temp::new();
        let path = temp.0.join("source.redb");
        let repo = temp.0.join("archive");
        drop(database(&path));
        let baseline = init(&path, &repo, Default::default())
            .unwrap()
            .baseline_sequence;
        let mut engine = Engine::open_redb(&path).unwrap();
        let schema = engine.schema_info();
        engine.upgrade_storage(13).unwrap();
        drop(engine);
        export(&path, &repo, Default::default()).unwrap();
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage_with_archive(14, &repo).unwrap();
        drop(engine);
        export(&path, &repo, Default::default()).unwrap();
        verify(&repo, Default::default()).unwrap();
        for (offset, format) in [(0, 11), (1, 13), (2, 14)] {
            let restored = temp.0.join(format!("restored-{offset}.redb"));
            restore(&repo, &restored, baseline + offset, Default::default()).unwrap();
            let mut engine = Engine::open_redb(&restored).unwrap();
            engine.check_integrity().unwrap();
            assert_eq!(
                engine.backup_journal_status().unwrap().storage_format,
                format
            );
            assert_eq!(engine.schema_info(), schema);
            assert_eq!(engine.execute("from items").rows.len(), 1);
        }
    }

    #[test]
    fn native_capability_installation_rewrites_codecs_atomically_and_restores_each_boundary() {
        let temp = Temp::new();
        let legacy = temp.0.join("legacy.redb");
        let path = temp.0.join("native.redb");
        let repo = temp.0.join("archive");
        let source = database(&legacy);
        let database = source.database_snapshot().unwrap();
        let schema = source.schema_info();
        drop(source);
        let header = crate::redb_storage::StorageHeader::encode_transport(
            crate::redb_storage::StorageLayout::from_header_profile(false, false, false, false),
        )
        .unwrap();
        drop(
            Engine::restore_redb_with_header(
                path.clone(),
                database,
                ReceiptMap::new(),
                Some(header),
            )
            .unwrap(),
        );
        init(&path, &repo, Default::default()).unwrap();
        for (capability, expected_catalog) in [
            ("typed_map", 5),
            ("partial_unique_index", 6),
            ("typed_references", 7),
        ] {
            let mut engine = Engine::open_redb(&path).unwrap();
            let before_header = engine.stored_header().unwrap();
            let before = engine.backup_journal_status().unwrap();
            let requested = [capability.to_owned()];
            assert_eq!(
                engine
                    .install_storage_capabilities(&requested, None)
                    .unwrap_err()
                    .code,
                "E_BACKUP_CHAIN_ACTIVE"
            );
            assert_eq!(engine.backup_journal_status().unwrap(), before);
            let result = engine
                .install_storage_capabilities(&requested, Some(&repo))
                .unwrap();
            assert!(result.changed);
            assert_eq!(result.format, 14);
            assert_eq!(engine.schema_info(), schema);
            assert_eq!(
                engine.backup_journal_status().unwrap().head_sequence,
                before.head_sequence + 1
            );
            let after_header = engine.stored_header().unwrap();
            assert_eq!(
                engine
                    .read_snapshot()
                    .introspection()
                    .required_storage_capabilities,
                engine.introspection().required_storage_capabilities
            );
            assert_eq!(
                crate::redb_storage::StorageHeader::decode_transport(
                    after_header.as_deref().unwrap()
                )
                .unwrap()
                .catalog,
                expected_catalog
            );
            assert!(
                !engine
                    .install_storage_capabilities(&requested, None)
                    .unwrap()
                    .changed
            );
            assert_eq!(
                engine.backup_journal_status().unwrap().head_sequence,
                before.head_sequence + 1
            );
            drop(engine);
            export(&path, &repo, Default::default()).unwrap();
            verify(&repo, Default::default()).unwrap();
            for (sequence, header) in [
                (before.head_sequence, before_header),
                (before.head_sequence + 1, after_header),
            ] {
                let restored = temp
                    .0
                    .join(format!("restored-{capability}-{sequence}.redb"));
                restore(&repo, &restored, sequence, Default::default()).unwrap();
                let mut restored = Engine::open_redb(&restored).unwrap();
                restored.check_integrity().unwrap();
                assert_eq!(restored.stored_header().unwrap(), header);
                assert_eq!(restored.schema_info(), schema);
                assert_eq!(restored.execute("from items").rows.len(), 1);
            }
        }
    }

    #[test]
    fn capability_upgrade_rejects_unexported_or_corrupt_archives_without_changing_database() {
        let temp = Temp::new();
        let path = temp.0.join("source.redb");
        let repo = temp.0.join("archive");
        drop(database(&path));
        init(&path, &repo, Default::default()).unwrap();
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute("insert items {id = 2, label = \"two\"}").ok);
        let before = engine.backup_journal_status().unwrap();
        assert_eq!(
            engine
                .upgrade_storage_with_archive(14, &repo)
                .unwrap_err()
                .code,
            "E_BACKUP_CHAIN"
        );
        assert_eq!(engine.backup_journal_status().unwrap(), before);
        drop(engine);
        export(&path, &repo, Default::default()).unwrap();
        let manifest = read_manifest(&repo).unwrap();
        let artifact = repo.join(&manifest.baseline.path);
        let mut bytes = fs::read(&artifact).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&artifact, bytes).unwrap();
        let mut engine = Engine::open_redb(&path).unwrap();
        let before = engine.backup_journal_status().unwrap();
        assert!(engine.upgrade_storage_with_archive(14, &repo).is_err());
        assert_eq!(engine.backup_journal_status().unwrap(), before);
        engine.check_integrity().unwrap();
        assert_eq!(engine.execute("from items").rows.len(), 2);
        drop(engine);
        let mut read_only = Engine::open_redb_read_only(&path).unwrap();
        assert_eq!(
            read_only
                .upgrade_storage_with_archive(14, temp.0.join("missing"))
                .unwrap_err()
                .code,
            "E_READ_ONLY"
        );
    }

    #[test]
    fn capability_header_keeps_physical_format_across_init_checkpoint_and_disable() {
        for references in [false, true] {
            let temp = Temp::new();
            let path = temp.0.join("source.redb");
            let repo = temp.0.join("archive");
            let mut engine = database(&path);
            if references {
                engine.upgrade_storage(12).unwrap();
            }
            let schema = engine.schema_info();
            engine.upgrade_storage(14).unwrap();
            drop(engine);
            let report = init(&path, &repo, Default::default()).unwrap();
            assert_eq!(report.previous_storage_format, 14);
            assert_eq!(report.current_storage_format, 14);
            assert_eq!(read_manifest(&repo).unwrap().baseline.record_codec, Some(2));
            let mut engine = Engine::open_redb(&path).unwrap();
            assert!(engine.execute("insert items {id = 2, label = \"two\"}").ok);
            let sequence = engine.backup_journal_status().unwrap().head_sequence;
            drop(engine);
            export(&path, &repo, Default::default()).unwrap();
            checkpoint(&path, &repo, Default::default()).unwrap();
            verify(&repo, Default::default()).unwrap();
            let restored = temp.0.join("restored.redb");
            restore(&repo, &restored, sequence, Default::default()).unwrap();
            let mut restored = Engine::open_redb(&restored).unwrap();
            restored.check_integrity().unwrap();
            assert_eq!(restored.schema_info(), schema);
            assert_eq!(restored.execute("from items").rows.len(), 2);
            assert_eq!(restored.backup_journal_status().unwrap().storage_format, 14);
            drop(restored);
            disable(&path, &repo, false, true).unwrap();
            let mut engine = Engine::open_redb(&path).unwrap();
            engine.check_integrity().unwrap();
            let status = engine.backup_journal_status().unwrap();
            assert_eq!(status.storage_format, 14);
            assert_eq!(status.state, BackupJournalState::Disabled);
            let encoded = engine.stored_header().unwrap().unwrap();
            assert_eq!(
                crate::redb_storage::StorageHeader::decode_transport(&encoded)
                    .unwrap()
                    .journal,
                1
            );
        }
    }

    #[test]
    fn capability_header_upgrade_preserves_each_side_of_the_archive_transition() {
        for references in [false, true] {
            let temp = Temp::new();
            let path = temp.0.join("source.redb");
            let repo = temp.0.join("archive");
            let mut engine = database(&path);
            if references {
                engine.upgrade_storage(12).unwrap();
            }
            let schema = engine.schema_info();
            drop(engine);
            init(&path, &repo, Default::default()).unwrap();
            let before = Engine::open_redb(&path)
                .unwrap()
                .backup_journal_status()
                .unwrap();
            let old_manifest = read_manifest(&repo).unwrap();
            let old_baseline = fs::read(repo.join(&old_manifest.baseline.path)).unwrap();
            let mut engine = Engine::open_redb(&path).unwrap();
            assert_eq!(
                engine.upgrade_storage(14).unwrap_err().code,
                "E_BACKUP_CHAIN_ACTIVE"
            );
            engine.upgrade_storage_with_archive(14, &repo).unwrap();
            assert_eq!(engine.schema_info(), schema);
            assert!(engine.stored_header().unwrap().is_some());
            assert_eq!(
                engine.backup_journal_status().unwrap().head_sequence,
                before.head_sequence + 1
            );
            assert!(!engine.upgrade_storage(14).unwrap().changed);
            drop(engine);
            // Reopen after the DB commit but before manifest publication.
            let mut engine = Engine::open_redb(&path).unwrap();
            engine.check_integrity().unwrap();
            assert!(engine.execute("insert items {id = 2, label = \"two\"}").ok);
            drop(engine);
            export(&path, &repo, Default::default()).unwrap();
            verify(&repo, Default::default()).unwrap();
            let manifest = read_manifest(&repo).unwrap();
            assert_eq!(manifest.format_version, ARTIFACT_MANIFEST_FORMAT_VERSION);
            assert_eq!(manifest.baseline.record_codec, Some(1));
            assert_eq!(manifest.segments[0].record_codec, Some(2));
            assert_eq!(
                fs::read(repo.join(&manifest.baseline.path)).unwrap(),
                old_baseline
            );
            for (offset, expected_format, rows) in
                [(0, before.storage_format, 1), (1, 14, 1), (2, 14, 2)]
            {
                let restored = temp.0.join(format!("restored-{offset}.redb"));
                restore(
                    &repo,
                    &restored,
                    before.head_sequence + offset,
                    Default::default(),
                )
                .unwrap();
                let mut restored = Engine::open_redb(&restored).unwrap();
                restored.check_integrity().unwrap();
                assert_eq!(
                    restored.backup_journal_status().unwrap().storage_format,
                    expected_format
                );
                assert_eq!(restored.schema_info(), schema);
                let response = restored.execute("from items");
                assert!(response.ok, "{:?}", response.error);
                assert_eq!(response.rows.len(), rows);
            }
        }
    }

    fn prepare_only(engine: &Engine, repo: &Path, chain_id: &str) -> ArchiveManifest {
        let repo = prepare_repo(repo).unwrap();
        let source = engine.incremental_baseline_source(chain_id).unwrap();
        let encoded = encode_archive(
            ArchiveKind::Baseline,
            RECORD_CODEC_VERSION,
            Compression::None,
            &source.header,
            &source.frames,
            &ArchiveLimits::default(),
        )
        .unwrap();
        let relative = format!(
            "baselines/b-{}-{}.uib",
            source.header.first_sequence,
            checksum_suffix(&encoded.payload_checksum)
        );
        write_new_archive_entry(&repo, Path::new(&relative), &encoded.bytes).unwrap();
        let artifact = ManifestArtifact {
            record_codec: None,
            path: relative,
            first_sequence: source.header.first_sequence,
            last_sequence: source.header.last_sequence,
            parent_checksum: None,
            payload_checksum: encoded.payload_checksum,
            stored_checksum: encoded.stored_checksum,
            compression: Compression::None,
            stored_bytes: encoded.bytes.len() as u64,
            expanded_bytes: encoded.expanded_bytes,
        };
        let manifest = ArchiveManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            archive_codec: ARCHIVE_CODEC_VERSION,
            record_codec: RECORD_CODEC_VERSION,
            chain_id: chain_id.to_owned(),
            database_digest: source.header.database_digest,
            state: ManifestState::Prepared,
            baseline: artifact.clone(),
            segments: Vec::new(),
            recoverable_first_sequence: artifact.first_sequence,
            recoverable_last_sequence: artifact.last_sequence,
            stored_bytes: artifact.stored_bytes,
            expanded_bytes: artifact.expanded_bytes,
            journal_max_commits: None,
            journal_max_bytes: None,
            checkpoint_previous_first_sequence: None,
            checkpoint_retired_artifacts: Vec::new(),
            checksum: String::new(),
        };
        publish_manifest(&repo, None, manifest.clone()).unwrap();
        manifest
    }

    #[test]
    fn init_recovers_before_and_after_database_enable() {
        for enable_before_retry in [false, true] {
            let temp = Temp::new();
            let db = temp.0.join("app.redb");
            let repo = temp.0.join("archive");
            let mut engine = database(&db);
            engine.check_integrity().unwrap();
            let prepared = prepare_only(&engine, &repo, "reconcile-chain");
            if enable_before_retry {
                engine
                    .enable_backup_journal(journal_config(
                        &prepared,
                        &IncrementalInitOptions::default(),
                    ))
                    .unwrap();
            }
            drop(engine);
            let report = init(&db, &repo, IncrementalInitOptions::default()).unwrap();
            assert!(report.resumed);
            assert_eq!(read_manifest(&repo).unwrap().state, ManifestState::Active);
            assert_eq!(
                Engine::open_redb(db.clone())
                    .unwrap()
                    .backup_journal_status()
                    .unwrap()
                    .state,
                BackupJournalState::Active
            );
        }
    }

    #[test]
    fn export_recovers_manifest_publication_before_journal_prune() {
        let temp = Temp::new();
        let db = temp.0.join("app.redb");
        let repo = temp.0.join("archive");
        drop(database(&db));
        init(&db, &repo, IncrementalInitOptions::default()).unwrap();
        let mut engine = Engine::open_redb(db.clone()).unwrap();
        assert!(
            engine
                .execute("update items | filter id == 1 | set label = \"two\"")
                .ok
        );
        let mut manifest = read_manifest(&repo).unwrap();
        let mut source = engine.incremental_journal_source(None).unwrap().unwrap();
        source
            .header
            .database_digest
            .clone_from(&manifest.database_digest);
        let encoded = encode_archive(
            ArchiveKind::Segment,
            manifest.record_codec,
            Compression::Zstd,
            &source.header,
            &source.frames,
            &ArchiveLimits::default(),
        )
        .unwrap();
        let relative = format!(
            "segments/s-{}-{}-{}.uis",
            source.header.first_sequence,
            source.header.last_sequence,
            checksum_suffix(&encoded.payload_checksum)
        );
        write_new_archive_entry(&repo, Path::new(&relative), &encoded.bytes).unwrap();
        let previous = encode_manifest(manifest.clone()).unwrap();
        manifest.segments.push(ManifestArtifact {
            record_codec: (manifest.format_version == ARTIFACT_MANIFEST_FORMAT_VERSION)
                .then_some(RECORD_CODEC_VERSION),
            path: relative,
            first_sequence: source.header.first_sequence,
            last_sequence: source.header.last_sequence,
            parent_checksum: Some(manifest.baseline.payload_checksum.clone()),
            payload_checksum: encoded.payload_checksum,
            stored_checksum: encoded.stored_checksum,
            compression: Compression::Zstd,
            stored_bytes: encoded.bytes.len() as u64,
            expanded_bytes: encoded.expanded_bytes,
        });
        manifest.recoverable_last_sequence = source.header.last_sequence;
        manifest.stored_bytes += encoded.bytes.len() as u64;
        manifest.expanded_bytes += encoded.expanded_bytes;
        publish_manifest(&repo, Some(&previous), manifest).unwrap();
        drop(engine);

        let retry = export(&db, &repo, IncrementalExportOptions::default()).unwrap();
        assert!(retry.no_op);
        let status = Engine::open_redb(db)
            .unwrap()
            .backup_journal_status()
            .unwrap();
        assert_eq!(status.commit_count, 0);
        assert_eq!(status.exported_sequence, Some(source.header.last_sequence));
    }

    #[test]
    fn checkpoint_resumes_after_prepared_manifest_publication() {
        checkpoint_resume(false, false);
    }

    #[test]
    fn artifact_codec_checkpoint_resumes_after_prepared_manifest_publication() {
        checkpoint_resume(true, false);
    }

    #[test]
    fn mixed_native_checkpoint_resumes_after_prepared_manifest_publication() {
        checkpoint_resume(true, true);
    }

    fn checkpoint_resume(explicit_codecs: bool, native_header: bool) {
        let temp = Temp::new();
        let db = temp.0.join("checkpoint.redb");
        let repo = temp.0.join("archive");
        drop(database(&db));
        init(&db, &repo, IncrementalInitOptions::default()).unwrap();
        let mut engine = Engine::open_redb(db.clone()).unwrap();
        assert!(engine.execute("update items | set label = \"two\"").ok);
        drop(engine);
        export(&db, &repo, Default::default()).unwrap();

        if native_header {
            let mut engine = Engine::open_redb(&db).unwrap();
            engine.upgrade_storage_with_archive(14, &repo).unwrap();
            drop(engine);
            export(&db, &repo, Default::default()).unwrap();
            let manifest = read_manifest(&repo).unwrap();
            assert_eq!(manifest.baseline.record_codec, Some(1));
            assert_eq!(manifest.segments.last().unwrap().record_codec, Some(2));
        }

        if explicit_codecs && !native_header {
            let old = read_manifest(&repo).unwrap();
            let mut explicit = old.clone();
            explicit.format_version = ARTIFACT_MANIFEST_FORMAT_VERSION;
            explicit.record_codec = 0;
            for artifact in std::iter::once(&mut explicit.baseline).chain(&mut explicit.segments) {
                artifact.record_codec = Some(RECORD_CODEC_VERSION);
            }
            publish_manifest(&repo, Some(&encode_manifest(old).unwrap()), explicit).unwrap();
        }

        let engine = Engine::open_redb(db.clone()).unwrap();
        let status = engine.backup_journal_status().unwrap();
        let old = read_manifest(&repo).unwrap();
        let source = engine.incremental_baseline_source(&old.chain_id).unwrap();
        let encoded = encode_archive(
            ArchiveKind::Baseline,
            source.record_codec,
            Compression::Zstd,
            &source.header,
            &source.frames,
            &ArchiveLimits::default(),
        )
        .unwrap();
        let relative = format!(
            "baselines/b-{}-{}.uib",
            source.header.first_sequence,
            checksum_suffix(&encoded.payload_checksum)
        );
        write_new_archive_entry(&repo, Path::new(&relative), &encoded.bytes).unwrap();
        let mut prepared = old.clone();
        prepared.state = ManifestState::Prepared;
        prepared.baseline = ManifestArtifact {
            record_codec: explicit_codecs.then_some(source.record_codec),
            path: relative,
            first_sequence: source.header.first_sequence,
            last_sequence: source.header.last_sequence,
            parent_checksum: None,
            payload_checksum: encoded.payload_checksum,
            stored_checksum: encoded.stored_checksum,
            compression: Compression::Zstd,
            stored_bytes: encoded.bytes.len() as u64,
            expanded_bytes: encoded.expanded_bytes,
        };
        prepared.segments.clear();
        prepared.recoverable_first_sequence = source.header.first_sequence;
        prepared.recoverable_last_sequence = source.header.last_sequence;
        prepared.stored_bytes = prepared.baseline.stored_bytes;
        prepared.expanded_bytes = prepared.baseline.expanded_bytes;
        prepared.journal_max_commits = Some(status.max_commits);
        prepared.journal_max_bytes = Some(status.max_bytes);
        let retired = std::iter::once(old.baseline.clone())
            .chain(old.segments.iter().cloned())
            .filter(|artifact| artifact.path != prepared.baseline.path)
            .collect::<Vec<_>>();
        prepared.checkpoint_previous_first_sequence = Some(old.recoverable_first_sequence);
        prepared.checkpoint_retired_artifacts = retired.clone();
        publish_manifest(&repo, Some(&encode_manifest(old).unwrap()), prepared).unwrap();
        drop(engine);

        let report = checkpoint(&db, &repo, Default::default()).unwrap();
        assert!(report.resumed);
        assert_eq!(report.retired_artifacts, retired.len() as u64);
        assert_eq!(
            report.retired_bytes,
            retired
                .iter()
                .map(|artifact| artifact.stored_bytes)
                .sum::<u64>()
        );
        assert_eq!(read_manifest(&repo).unwrap().state, ManifestState::Active);
        let status = Engine::open_redb(db)
            .unwrap()
            .backup_journal_status()
            .unwrap();
        assert_eq!(
            status.baseline_sequence,
            Some(report.recoverable_first_sequence)
        );
        assert_eq!(status.commit_count, 0);
        if native_header {
            let manifest = read_manifest(&repo).unwrap();
            assert_eq!(manifest.baseline.record_codec, Some(2));
            verify(&repo, Default::default()).unwrap();
            // An unknown orphan must abort confirmed pruning before removing
            // any supported retired artifact from the mixed codec chain.
            let unknown_path = repo.join("segments/unknown-codec.uij");
            let mut unknown = encoded.bytes.clone();
            unknown[6..8].copy_from_slice(&99u16.to_be_bytes());
            fs::write(&unknown_path, &unknown).unwrap();
            let retained_bytes = retired
                .iter()
                .map(|artifact| {
                    (
                        artifact.path.clone(),
                        fs::read(repo.join(&artifact.path)).unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            let manifest_bytes = fs::read(repo.join(MANIFEST_NAME)).unwrap();
            let error = prune(
                &repo,
                report.recoverable_first_sequence,
                true,
                Default::default(),
            )
            .unwrap_err();
            assert!(
                error
                    .message
                    .contains("unsupported retired artifact record codec")
            );
            for (relative, bytes) in &retained_bytes {
                assert_eq!(fs::read(repo.join(relative)).unwrap(), *bytes);
            }
            assert_eq!(fs::read(&unknown_path).unwrap(), unknown);
            assert_eq!(fs::read(repo.join(MANIFEST_NAME)).unwrap(), manifest_bytes);
            fs::remove_file(unknown_path).unwrap();
            let preview = prune(
                &repo,
                report.recoverable_first_sequence,
                false,
                Default::default(),
            )
            .unwrap();
            let mut expected_files = retired
                .iter()
                .map(|artifact| artifact.path.clone())
                .collect::<Vec<_>>();
            expected_files.sort();
            assert_eq!(preview.selected_files, expected_files);
            let applied = prune(
                &repo,
                report.recoverable_first_sequence,
                true,
                Default::default(),
            )
            .unwrap();
            assert_eq!(applied.selected_files, preview.selected_files);
            assert!(applied.applied);
            assert_eq!(fs::read(repo.join(MANIFEST_NAME)).unwrap(), manifest_bytes);
            verify(&repo, Default::default()).unwrap();
            let restored = temp.0.join("native-checkpoint-restore.redb");
            restore(
                &repo,
                &restored,
                report.recoverable_first_sequence,
                Default::default(),
            )
            .unwrap();
            let mut engine = Engine::open_redb(restored).unwrap();
            engine.check_integrity().unwrap();
            assert_eq!(engine.introspection().storage_versions.unwrap().format, 14);
            assert_eq!(engine.execute("from items").rows.len(), 1);
        }
    }
}
