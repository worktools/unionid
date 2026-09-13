use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ARCHIVE_CODEC_VERSION, ArchiveKind, ArchiveLimits, ArchiveManifest, BackupJournalConfig,
    BackupJournalState, Compression, MANIFEST_FORMAT_VERSION, ManifestArtifact, ManifestState,
    decode_archive, decode_manifest, encode_archive, encode_manifest, validate_archive_entry,
    write_new_archive_entry,
};
use crate::Engine;
use crate::error::{Error, Result};

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
    revision: u64,
    hash: String,
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
    pub gaps: Vec<String>,
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
        RECORD_CODEC_VERSION,
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
        format_version: MANIFEST_FORMAT_VERSION,
        archive_codec: ARCHIVE_CODEC_VERSION,
        record_codec: RECORD_CODEC_VERSION,
        chain_id: chain_id.clone(),
        database_digest: source.header.database_digest,
        state: ManifestState::Prepared,
        baseline: artifact.clone(),
        segments: Vec::new(),
        recoverable_first_sequence: artifact.first_sequence,
        recoverable_last_sequence: artifact.last_sequence,
        stored_bytes: artifact.stored_bytes,
        expanded_bytes: artifact.expanded_bytes,
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
        publish_manifest(&repo, Some(&previous), manifest.clone())?;
        manifest = read_manifest(&repo)?;
    }
    if manifest.state != ManifestState::Active {
        return Err(chain_error("incremental export requires an active archive"));
    }
    verify_manifest_artifacts(&repo, &manifest, &options.limits)?;
    reconcile_exported_prefix(&mut engine, &repo, &manifest, &options.limits)?;
    let status = engine.backup_journal_status()?;
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
                manifest.record_codec,
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
        let parent = manifest
            .segments
            .last()
            .map_or(&manifest.baseline.payload_checksum, |item| {
                &item.payload_checksum
            })
            .clone();
        manifest.segments.push(ManifestArtifact {
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

fn verify_manifest_artifacts(
    repo: &Path,
    manifest: &ArchiveManifest,
    limits: &ArchiveLimits,
) -> Result<()> {
    let baseline = read_artifact(repo, &manifest.baseline, limits)?;
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
        revision: meta.schema_revision,
        hash: meta.schema_hash,
    };
    let mut commit_parent = manifest.baseline.payload_checksum.clone();
    for artifact in &manifest.segments {
        let decoded = read_artifact(repo, artifact, limits)?;
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
    }
    let _ = commit_parent;
    Ok(())
}

fn read_artifact(
    repo: &Path,
    artifact: &ManifestArtifact,
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
    if decoded.kind != kind
        || decoded.record_codec != manifest.record_codec
        || decoded.header.chain_id != manifest.chain_id
        || decoded.header.database_digest != manifest.database_digest
        || decoded.header.first_sequence != artifact.first_sequence
        || decoded.header.last_sequence != artifact.last_sequence
        || content_codecs.is_some_and(|expected| {
            decoded.header.catalog_codec != expected.catalog_codec
                || decoded.header.value_codec != expected.value_codec
                || decoded.header.receipt_codec != expected.receipt_codec
        })
    {
        return Err(chain_error("archive header does not match the manifest"));
    }
    Ok(())
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

fn reconcile_exported_prefix(
    engine: &mut Engine,
    repo: &Path,
    manifest: &ArchiveManifest,
    limits: &ArchiveLimits,
) -> Result<()> {
    let status = engine.backup_journal_status()?;
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
    let mut commit_checksum = manifest.baseline.payload_checksum.clone();
    for archived in &manifest.segments {
        let decoded = read_artifact(repo, archived, limits)?;
        commit_checksum = verify_segment_commits(
            &decoded.frames,
            archived.first_sequence,
            &commit_checksum,
            None,
        )?;
    }
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

fn read_manifest(repo: &Path) -> Result<ArchiveManifest> {
    let path = repo.join(MANIFEST_NAME);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| archive_error(format!("inspect archive manifest: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(archive_error(
            "archive manifest must be a bounded regular file",
        ));
    }
    decode_manifest(
        &fs::read(path)
            .map_err(|error| archive_error(format!("read archive manifest: {error}")))?,
    )
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
        gaps: Vec::new(),
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
}
