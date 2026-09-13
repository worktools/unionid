//! Portable archive primitives for incremental backup chains.
//!
//! This module defines the archive codec independently from redb and the
//! database journal. Higher-level init/export/restore operations build on it.

mod journal;

pub use journal::{
    BACKUP_JOURNAL_STATUS_VERSION, BackupJournalConfig, BackupJournalState, BackupJournalStatus,
    DEFAULT_JOURNAL_MAX_BYTES, DEFAULT_JOURNAL_MAX_COMMITS, HARD_JOURNAL_MAX_BYTES,
    HARD_JOURNAL_MAX_COMMITS,
};

use std::fs::OpenOptions;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const ARCHIVE_CODEC_VERSION: u16 = 1;
pub const MANIFEST_FORMAT_VERSION: u16 = 1;
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

const BASELINE_MAGIC: [u8; 4] = *b"UIB1";
const SEGMENT_MAGIC: [u8; 4] = *b"UIS1";
const PREFIX_BYTES: usize = 16;
const END_KIND: u8 = 255;
const END_PAYLOAD_BYTES: usize = 48;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Baseline,
    Segment,
}

impl ArchiveKind {
    fn magic(self) -> [u8; 4] {
        match self {
            Self::Baseline => BASELINE_MAGIC,
            Self::Segment => SEGMENT_MAGIC,
        }
    }

    fn from_magic(magic: [u8; 4]) -> Result<Self> {
        match magic {
            BASELINE_MAGIC => Ok(Self::Baseline),
            SEGMENT_MAGIC => Ok(Self::Segment),
            _ => Err(archive_error("unknown archive magic")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    None,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestState {
    Prepared,
    Active,
    Sealed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestArtifact {
    pub path: String,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub parent_checksum: Option<String>,
    pub payload_checksum: String,
    pub stored_checksum: String,
    pub compression: Compression,
    pub stored_bytes: u64,
    pub expanded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    pub format_version: u16,
    pub archive_codec: u16,
    pub record_codec: u16,
    pub chain_id: String,
    pub database_digest: String,
    pub state: ManifestState,
    pub baseline: ManifestArtifact,
    pub segments: Vec<ManifestArtifact>,
    pub recoverable_first_sequence: u64,
    pub recoverable_last_sequence: u64,
    pub stored_bytes: u64,
    pub expanded_bytes: u64,
    pub checksum: String,
}

#[derive(Serialize)]
struct ManifestPayload<'a> {
    format_version: u16,
    archive_codec: u16,
    record_codec: u16,
    chain_id: &'a str,
    database_digest: &'a str,
    state: ManifestState,
    baseline: &'a ManifestArtifact,
    segments: &'a [ManifestArtifact],
    recoverable_first_sequence: u64,
    recoverable_last_sequence: u64,
    stored_bytes: u64,
    expanded_bytes: u64,
}

impl Compression {
    fn byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Zstd => 1,
        }
    }

    fn from_byte(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Zstd),
            _ => Err(archive_error("unknown archive compression")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveHeader {
    pub chain_id: String,
    pub database_digest: String,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub catalog_codec: u32,
    pub value_codec: u32,
    pub receipt_codec: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveFrame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl ArchiveFrame {
    pub fn new(kind: u8, payload: Vec<u8>) -> Self {
        Self { kind, payload }
    }

    pub fn delete(kind: u8, key: &[u8]) -> Result<Self> {
        let mut payload = Vec::new();
        push_len(&mut payload, key.len())?;
        payload.extend_from_slice(key);
        Ok(Self { kind, payload })
    }

    pub fn write(kind: u8, key: &[u8], value: &[u8]) -> Result<Self> {
        let mut payload = Vec::new();
        push_len(&mut payload, key.len())?;
        payload.extend_from_slice(key);
        push_len(&mut payload, value.len())?;
        payload.extend_from_slice(value);
        Ok(Self { kind, payload })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveLimits {
    pub max_stored_bytes: u64,
    pub max_expanded_bytes: u64,
    pub max_header_bytes: usize,
    pub max_frames: u64,
    pub max_record_bytes: usize,
    pub max_key_bytes: usize,
    pub max_value_bytes: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_stored_bytes: 1024 * 1024 * 1024,
            max_expanded_bytes: 1024 * 1024 * 1024,
            max_header_bytes: 1024 * 1024,
            max_frames: 10_000_000,
            max_record_bytes: 16 * 1024 * 1024,
            max_key_bytes: 1024 * 1024,
            max_value_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedArchive {
    pub bytes: Vec<u8>,
    pub payload_checksum: String,
    pub stored_checksum: String,
    pub expanded_bytes: u64,
    pub record_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedArchive {
    pub kind: ArchiveKind,
    pub record_codec: u16,
    pub compression: Compression,
    pub header: ArchiveHeader,
    pub frames: Vec<ArchiveFrame>,
    pub payload_checksum: String,
    pub stored_checksum: String,
    pub expanded_bytes: u64,
}

pub fn encode_archive(
    kind: ArchiveKind,
    record_codec: u16,
    compression: Compression,
    header: &ArchiveHeader,
    frames: &[ArchiveFrame],
    limits: &ArchiveLimits,
) -> Result<EncodedArchive> {
    if record_codec == 0 {
        return Err(archive_error("record codec must be nonzero"));
    }
    validate_header(kind, header)?;
    validate_frames(kind, frames, limits)?;
    let header_bytes = canonical_header(header)?;
    if header_bytes.len() > limits.max_header_bytes {
        return Err(limit_error("archive header exceeds its byte limit"));
    }

    let mut stream = Vec::new();
    for frame in frames {
        push_frame(&mut stream, frame.kind, &frame.payload)?;
        check_expanded(stream.len(), limits)?;
    }
    let expanded_bytes = u64::try_from(stream.len())
        .map_err(|_| limit_error("expanded archive length does not fit u64"))?;
    let mut payload_hash = Sha256::new();
    payload_hash.update(&header_bytes);
    payload_hash.update(&stream);
    let payload_digest: [u8; 32] = payload_hash.finalize().into();

    let mut end = Vec::with_capacity(END_PAYLOAD_BYTES);
    end.extend_from_slice(
        &u64::try_from(frames.len())
            .map_err(|_| limit_error("archive frame count does not fit u64"))?
            .to_be_bytes(),
    );
    end.extend_from_slice(&expanded_bytes.to_be_bytes());
    end.extend_from_slice(&payload_digest);
    push_frame(&mut stream, END_KIND, &end)?;
    check_expanded(stream.len(), limits)?;

    let stored_stream = match compression {
        Compression::None => stream,
        Compression::Zstd => zstd::stream::encode_all(Cursor::new(stream), DEFAULT_ZSTD_LEVEL)
            .map_err(|error| archive_error(format!("compress archive: {error}")))?,
    };
    let header_len = u32::try_from(header_bytes.len())
        .map_err(|_| limit_error("archive header length does not fit u32"))?;
    let mut bytes = Vec::with_capacity(PREFIX_BYTES + header_bytes.len() + stored_stream.len());
    bytes.extend_from_slice(&kind.magic());
    bytes.extend_from_slice(&ARCHIVE_CODEC_VERSION.to_be_bytes());
    bytes.extend_from_slice(&record_codec.to_be_bytes());
    bytes.push(compression.byte());
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes.extend_from_slice(&header_len.to_be_bytes());
    bytes.extend_from_slice(&header_bytes);
    bytes.extend_from_slice(&stored_stream);
    check_stored(bytes.len(), limits)?;
    let stored_checksum = checksum(&bytes);
    Ok(EncodedArchive {
        bytes,
        payload_checksum: checksum_digest(payload_digest),
        stored_checksum,
        expanded_bytes,
        record_count: frames.len() as u64,
    })
}

pub fn decode_archive(bytes: &[u8], limits: &ArchiveLimits) -> Result<DecodedArchive> {
    check_stored(bytes.len(), limits)?;
    if bytes.len() < PREFIX_BYTES {
        return Err(archive_error("truncated archive prefix"));
    }
    let kind = ArchiveKind::from_magic(bytes[0..4].try_into().expect("fixed slice"))?;
    let version = u16::from_be_bytes(bytes[4..6].try_into().expect("fixed slice"));
    if version != ARCHIVE_CODEC_VERSION {
        return Err(archive_error(format!(
            "unsupported archive codec version {version}"
        )));
    }
    let record_codec = u16::from_be_bytes(bytes[6..8].try_into().expect("fixed slice"));
    if record_codec == 0 {
        return Err(archive_error("record codec must be nonzero"));
    }
    let compression = Compression::from_byte(bytes[8])?;
    if bytes[9..12] != [0, 0, 0] {
        return Err(archive_error("archive reserved bytes must be zero"));
    }
    let header_len = u32::from_be_bytes(bytes[12..16].try_into().expect("fixed slice")) as usize;
    if header_len > limits.max_header_bytes {
        return Err(limit_error("archive header exceeds its byte limit"));
    }
    let header_end = PREFIX_BYTES
        .checked_add(header_len)
        .ok_or_else(|| limit_error("archive header length overflow"))?;
    if header_end > bytes.len() {
        return Err(archive_error("truncated archive header"));
    }
    let header_bytes = &bytes[PREFIX_BYTES..header_end];
    let header: ArchiveHeader = serde_json::from_slice(header_bytes)
        .map_err(|error| archive_error(format!("decode archive header: {error}")))?;
    if canonical_header(&header)? != header_bytes {
        return Err(archive_error("archive header is not canonical"));
    }
    validate_header(kind, &header)?;

    let compressed = &bytes[header_end..];
    let stream = match compression {
        Compression::None => {
            check_expanded(compressed.len(), limits)?;
            compressed.to_vec()
        }
        Compression::Zstd => {
            let decoder = zstd::stream::read::Decoder::new(Cursor::new(compressed))
                .map_err(|error| archive_error(format!("open zstd archive: {error}")))?;
            let mut stream = Vec::new();
            decoder
                .take(limits.max_expanded_bytes.saturating_add(1))
                .read_to_end(&mut stream)
                .map_err(|error| archive_error(format!("decompress archive: {error}")))?;
            check_expanded(stream.len(), limits)?;
            stream
        }
    };

    let (frames, expected_count, expected_expanded, expected_digest) =
        parse_frames(&stream, limits)?;
    validate_frames(kind, &frames, limits)?;
    if expected_count != frames.len() as u64 {
        return Err(archive_error(
            "archive end frame has the wrong record count",
        ));
    }
    let data_len = stream
        .len()
        .checked_sub(5 + END_PAYLOAD_BYTES)
        .ok_or_else(|| archive_error("truncated archive end frame"))?;
    if expected_expanded != data_len as u64 {
        return Err(archive_error(
            "archive end frame has the wrong expanded length",
        ));
    }
    let mut payload_hash = Sha256::new();
    payload_hash.update(header_bytes);
    payload_hash.update(&stream[..data_len]);
    let actual_digest: [u8; 32] = payload_hash.finalize().into();
    if actual_digest != expected_digest {
        return Err(archive_error("archive payload checksum mismatch"));
    }
    Ok(DecodedArchive {
        kind,
        record_codec,
        compression,
        header,
        frames,
        payload_checksum: checksum_digest(actual_digest),
        stored_checksum: checksum(bytes),
        expanded_bytes: expected_expanded,
    })
}

pub fn encode_manifest(mut manifest: ArchiveManifest) -> Result<Vec<u8>> {
    manifest.checksum.clear();
    validate_manifest(&manifest, false)?;
    manifest.checksum = checksum(&manifest_payload_bytes(&manifest)?);
    let encoded = serde_json::to_vec(&manifest)
        .map_err(|error| archive_error(format!("encode archive manifest: {error}")))?;
    if encoded.len() > MAX_MANIFEST_BYTES {
        return Err(limit_error("archive manifest exceeds its byte limit"));
    }
    Ok(encoded)
}

pub fn decode_manifest(bytes: &[u8]) -> Result<ArchiveManifest> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(limit_error("archive manifest exceeds its byte limit"));
    }
    let manifest: ArchiveManifest = serde_json::from_slice(bytes)
        .map_err(|error| archive_error(format!("decode archive manifest: {error}")))?;
    let canonical = serde_json::to_vec(&manifest)
        .map_err(|error| archive_error(format!("encode archive manifest: {error}")))?;
    if canonical != bytes {
        return Err(archive_error("archive manifest is not canonical"));
    }
    validate_manifest(&manifest, true)?;
    let actual = checksum(&manifest_payload_bytes(&manifest)?);
    if actual != manifest.checksum {
        return Err(archive_error("archive manifest checksum mismatch"));
    }
    Ok(manifest)
}

pub fn validate_relative_archive_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.to_string_lossy().contains(['\\', '\0'])
    {
        return Err(archive_error(
            "archive path must be a nonempty relative path",
        ));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(archive_error("archive path contains an unsafe component")),
        }
    }
    Ok(())
}

pub fn write_new_archive_entry(root: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    validate_relative_archive_path(relative)?;
    let root = root
        .canonicalize()
        .map_err(|error| archive_error(format!("resolve archive root: {error}")))?;
    let parent = relative
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| archive_error("archive entry must have a parent directory"))?;
    reject_symlink_components(&root, parent)?;
    let parent = root
        .join(parent)
        .canonicalize()
        .map_err(|error| archive_error(format!("resolve archive entry parent: {error}")))?;
    if !parent.starts_with(&root) {
        return Err(archive_error("archive entry escapes its root"));
    }
    let output = parent.join(
        relative
            .file_name()
            .ok_or_else(|| archive_error("archive entry has no file name"))?,
    );
    if let Ok(metadata) = std::fs::symlink_metadata(&output) {
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != bytes.len() as u64
            || hash_file(&output)? != checksum(bytes)
        {
            return Err(archive_error(
                "archive entry already exists with other content",
            ));
        }
        return Ok(());
    }
    let (temporary, mut file) = create_temporary_entry(&parent)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(archive_error(format!("write archive entry: {error}")));
    }
    drop(file);
    let actual = hash_file(&temporary)?;
    if actual != checksum(bytes) {
        let _ = std::fs::remove_file(&temporary);
        return Err(archive_error("temporary archive entry failed verification"));
    }
    if let Err(error) = std::fs::hard_link(&temporary, &output) {
        let _ = std::fs::remove_file(&temporary);
        return Err(archive_error(format!("publish archive entry: {error}")));
    }
    sync_directory(&parent)?;
    let _ = std::fs::remove_file(&temporary);
    sync_directory(&parent)?;
    Ok(())
}

pub fn validate_archive_entry(root: &Path, relative: &Path) -> Result<()> {
    validate_relative_archive_path(relative)?;
    let root = root
        .canonicalize()
        .map_err(|error| archive_error(format!("resolve archive root: {error}")))?;
    let candidate = root.join(relative);
    let metadata = std::fs::symlink_metadata(&candidate)
        .map_err(|error| archive_error(format!("inspect archive entry: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(archive_error("archive entry must be a regular file"));
    }
    let resolved = candidate
        .canonicalize()
        .map_err(|error| archive_error(format!("resolve archive entry: {error}")))?;
    if !resolved.starts_with(&root) {
        return Err(archive_error("archive entry escapes its root"));
    }
    Ok(())
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(archive_error("archive path contains an unsafe component"));
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| archive_error(format!("inspect archive directory: {error}")))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(archive_error(
                "archive path parent must be a real directory",
            ));
        }
    }
    Ok(())
}

fn create_temporary_entry(parent: &Path) -> Result<(std::path::PathBuf, std::fs::File)> {
    for _ in 0..8 {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| archive_error(format!("generate archive nonce: {error}")))?;
        let name = nonce
            .iter()
            .fold(String::from(".unionid-archive-"), |mut name, byte| {
                use std::fmt::Write as _;
                write!(name, "{byte:02x}").expect("writing to String cannot fail");
                name
            });
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(archive_error(format!(
                    "create temporary archive entry: {error}"
                )));
            }
        }
    }
    Err(archive_error(
        "could not allocate a unique temporary archive entry",
    ))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| archive_error(format!("reopen archive entry: {error}")))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| archive_error(format!("verify archive entry: {error}")))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(checksum_digest(hash.finalize().into()))
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| archive_error(format!("sync archive directory: {error}")))?;
    }
    Ok(())
}

fn canonical_header(header: &ArchiveHeader) -> Result<Vec<u8>> {
    serde_json::to_vec(header)
        .map_err(|error| archive_error(format!("encode archive header: {error}")))
}

fn manifest_payload_bytes(manifest: &ArchiveManifest) -> Result<Vec<u8>> {
    serde_json::to_vec(&ManifestPayload {
        format_version: manifest.format_version,
        archive_codec: manifest.archive_codec,
        record_codec: manifest.record_codec,
        chain_id: &manifest.chain_id,
        database_digest: &manifest.database_digest,
        state: manifest.state,
        baseline: &manifest.baseline,
        segments: &manifest.segments,
        recoverable_first_sequence: manifest.recoverable_first_sequence,
        recoverable_last_sequence: manifest.recoverable_last_sequence,
        stored_bytes: manifest.stored_bytes,
        expanded_bytes: manifest.expanded_bytes,
    })
    .map_err(|error| archive_error(format!("encode archive manifest payload: {error}")))
}

fn validate_manifest(manifest: &ArchiveManifest, require_checksum: bool) -> Result<()> {
    if manifest.format_version != MANIFEST_FORMAT_VERSION
        || manifest.archive_codec != ARCHIVE_CODEC_VERSION
        || manifest.record_codec == 0
    {
        return Err(archive_error(
            "unsupported archive manifest version or codec",
        ));
    }
    if manifest.chain_id.is_empty() || manifest.database_digest.is_empty() {
        return Err(archive_error(
            "archive manifest identity fields must not be empty",
        ));
    }
    if require_checksum && !valid_checksum(&manifest.checksum) {
        return Err(archive_error("archive manifest checksum is malformed"));
    }
    validate_artifact(&manifest.baseline, true)?;
    if manifest.recoverable_first_sequence != manifest.baseline.first_sequence
        || manifest.baseline.first_sequence != manifest.baseline.last_sequence
        || manifest.recoverable_last_sequence < manifest.recoverable_first_sequence
    {
        return Err(archive_error(
            "archive manifest has an invalid recoverable range",
        ));
    }
    let mut expected_first = manifest.baseline.last_sequence.saturating_add(1);
    let mut expected_parent = manifest.baseline.payload_checksum.as_str();
    let mut stored = manifest.baseline.stored_bytes;
    let mut expanded = manifest.baseline.expanded_bytes;
    for segment in &manifest.segments {
        validate_artifact(segment, false)?;
        if segment.first_sequence != expected_first
            || segment.parent_checksum.as_deref() != Some(expected_parent)
        {
            return Err(archive_error(
                "archive manifest segment chain is not contiguous",
            ));
        }
        expected_first = segment
            .last_sequence
            .checked_add(1)
            .ok_or_else(|| limit_error("archive sequence overflow"))?;
        expected_parent = &segment.payload_checksum;
        stored = stored
            .checked_add(segment.stored_bytes)
            .ok_or_else(|| limit_error("archive stored-byte total overflow"))?;
        expanded = expanded
            .checked_add(segment.expanded_bytes)
            .ok_or_else(|| limit_error("archive expanded-byte total overflow"))?;
    }
    let actual_last = manifest
        .segments
        .last()
        .map_or(manifest.baseline.last_sequence, |segment| {
            segment.last_sequence
        });
    if manifest.recoverable_last_sequence != actual_last
        || manifest.stored_bytes != stored
        || manifest.expanded_bytes != expanded
    {
        return Err(archive_error("archive manifest totals are inconsistent"));
    }
    Ok(())
}

fn validate_artifact(artifact: &ManifestArtifact, baseline: bool) -> Result<()> {
    validate_relative_archive_path(Path::new(&artifact.path))?;
    let expected_prefix = if baseline { "baselines/" } else { "segments/" };
    if !artifact.path.starts_with(expected_prefix)
        || artifact.first_sequence > artifact.last_sequence
        || !valid_checksum(&artifact.payload_checksum)
        || !valid_checksum(&artifact.stored_checksum)
    {
        return Err(archive_error(
            "archive manifest artifact metadata is invalid",
        ));
    }
    if baseline != artifact.parent_checksum.is_none()
        || artifact
            .parent_checksum
            .as_deref()
            .is_some_and(|value| !valid_checksum(value))
    {
        return Err(archive_error("archive manifest artifact parent is invalid"));
    }
    Ok(())
}

fn valid_checksum(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        && value[7..]
            .bytes()
            .all(|byte| !byte.is_ascii_alphabetic() || byte.is_ascii_lowercase())
}

fn validate_header(kind: ArchiveKind, header: &ArchiveHeader) -> Result<()> {
    if header.chain_id.is_empty() || header.database_digest.is_empty() {
        return Err(archive_error("archive identity fields must not be empty"));
    }
    if header.first_sequence > header.last_sequence {
        return Err(archive_error("archive sequence range is reversed"));
    }
    if kind == ArchiveKind::Baseline && header.first_sequence != header.last_sequence {
        return Err(archive_error("baseline must describe exactly one sequence"));
    }
    if header.catalog_codec == 0 || header.value_codec == 0 || header.receipt_codec == 0 {
        return Err(archive_error("content codec versions must be nonzero"));
    }
    Ok(())
}

fn parse_frames(
    stream: &[u8],
    limits: &ArchiveLimits,
) -> Result<(Vec<ArchiveFrame>, u64, u64, [u8; 32])> {
    let mut offset = 0usize;
    let mut frames = Vec::new();
    loop {
        if offset == stream.len() {
            return Err(archive_error("archive is missing its end frame"));
        }
        if stream.len() - offset < 5 {
            return Err(archive_error("truncated archive frame header"));
        }
        let kind = stream[offset];
        let len = u32::from_be_bytes(
            stream[offset + 1..offset + 5]
                .try_into()
                .expect("fixed slice"),
        ) as usize;
        if len > limits.max_record_bytes && kind != END_KIND {
            return Err(limit_error("archive record exceeds its byte limit"));
        }
        let end = offset
            .checked_add(5)
            .and_then(|value| value.checked_add(len))
            .ok_or_else(|| limit_error("archive frame length overflow"))?;
        if end > stream.len() {
            return Err(archive_error("truncated archive frame payload"));
        }
        if kind == END_KIND {
            if len != END_PAYLOAD_BYTES || end != stream.len() {
                return Err(archive_error("invalid or nonterminal archive end frame"));
            }
            let payload = &stream[offset + 5..end];
            let count = u64::from_be_bytes(payload[0..8].try_into().expect("fixed slice"));
            let expanded = u64::from_be_bytes(payload[8..16].try_into().expect("fixed slice"));
            let digest = payload[16..48].try_into().expect("fixed slice");
            return Ok((frames, count, expanded, digest));
        }
        if frames.len() as u64 >= limits.max_frames {
            return Err(limit_error("archive frame count exceeds its limit"));
        }
        frames.push(ArchiveFrame {
            kind,
            payload: stream[offset + 5..end].to_vec(),
        });
        offset = end;
    }
}

fn validate_frames(
    kind: ArchiveKind,
    frames: &[ArchiveFrame],
    limits: &ArchiveLimits,
) -> Result<()> {
    if frames.len() as u64 > limits.max_frames {
        return Err(limit_error("archive frame count exceeds its limit"));
    }
    match kind {
        ArchiveKind::Baseline => validate_baseline_frames(frames, limits),
        ArchiveKind::Segment => validate_segment_frames(frames, limits),
    }
}

fn validate_baseline_frames(frames: &[ArchiveFrame], limits: &ArchiveLimits) -> Result<()> {
    let mut previous_kind = 0;
    let mut previous_key = Vec::new();
    let mut seen_meta = false;
    for frame in frames {
        check_payload(frame, limits)?;
        if !(1..=5).contains(&frame.kind) || frame.kind < previous_kind {
            return Err(archive_error("baseline frames are out of canonical order"));
        }
        if frame.kind == 1 {
            if seen_meta || previous_kind != 0 {
                return Err(archive_error(
                    "baseline meta frame is duplicated or misplaced",
                ));
            }
            seen_meta = true;
        } else {
            let key = write_key(&frame.payload, limits)?;
            if frame.kind == previous_kind && key <= previous_key.as_slice() {
                return Err(archive_error(
                    "baseline record keys are duplicated or unordered",
                ));
            }
            previous_key.clear();
            previous_key.extend_from_slice(key);
        }
        if frame.kind != previous_kind {
            previous_key.clear();
            if frame.kind != 1 {
                previous_key.extend_from_slice(write_key(&frame.payload, limits)?);
            }
        }
        previous_kind = frame.kind;
    }
    if !seen_meta {
        return Err(archive_error("baseline is missing its meta frame"));
    }
    Ok(())
}

fn validate_segment_frames(frames: &[ArchiveFrame], limits: &ArchiveLimits) -> Result<()> {
    let mut in_commit = false;
    let mut commits = 0u64;
    let mut previous_kind = 0;
    let mut previous_key = Vec::new();
    for frame in frames {
        check_payload(frame, limits)?;
        match frame.kind {
            16 if !in_commit => {
                in_commit = true;
                commits += 1;
                previous_kind = 16;
                previous_key.clear();
            }
            17..=24 if in_commit => {
                if frame.kind < previous_kind {
                    return Err(archive_error("segment frames are out of canonical order"));
                }
                let key = if matches!(frame.kind, 17 | 19 | 21 | 23) {
                    delete_key(&frame.payload, limits)?
                } else {
                    write_key(&frame.payload, limits)?
                };
                if frame.kind == previous_kind && key <= previous_key.as_slice() {
                    return Err(archive_error(
                        "segment record keys are duplicated or unordered",
                    ));
                }
                previous_key.clear();
                previous_key.extend_from_slice(key);
                previous_kind = frame.kind;
            }
            25 if in_commit => {
                in_commit = false;
                previous_kind = 0;
                previous_key.clear();
            }
            _ => return Err(archive_error("segment commit framing is invalid")),
        }
    }
    if in_commit || commits == 0 {
        return Err(archive_error(
            "segment has an incomplete or empty commit sequence",
        ));
    }
    Ok(())
}

fn delete_key<'a>(payload: &'a [u8], limits: &ArchiveLimits) -> Result<&'a [u8]> {
    let (key, remaining) = take_len_prefixed(payload, limits.max_key_bytes)?;
    if !remaining.is_empty() {
        return Err(archive_error("delete record has trailing bytes"));
    }
    Ok(key)
}

fn write_key<'a>(payload: &'a [u8], limits: &ArchiveLimits) -> Result<&'a [u8]> {
    let (key, remaining) = take_len_prefixed(payload, limits.max_key_bytes)?;
    let (_, trailing) = take_len_prefixed(remaining, limits.max_value_bytes)?;
    if !trailing.is_empty() {
        return Err(archive_error("write record has trailing bytes"));
    }
    Ok(key)
}

fn take_len_prefixed(bytes: &[u8], max: usize) -> Result<(&[u8], &[u8])> {
    if bytes.len() < 4 {
        return Err(archive_error("truncated length-delimited record"));
    }
    let len = u32::from_be_bytes(bytes[..4].try_into().expect("fixed slice")) as usize;
    if len > max {
        return Err(limit_error("length-delimited field exceeds its byte limit"));
    }
    let end = 4usize
        .checked_add(len)
        .ok_or_else(|| limit_error("length-delimited field overflow"))?;
    if end > bytes.len() {
        return Err(archive_error("truncated length-delimited field"));
    }
    Ok((&bytes[4..end], &bytes[end..]))
}

fn check_payload(frame: &ArchiveFrame, limits: &ArchiveLimits) -> Result<()> {
    if frame.payload.len() > limits.max_record_bytes {
        return Err(limit_error("archive record exceeds its byte limit"));
    }
    Ok(())
}

fn push_frame(output: &mut Vec<u8>, kind: u8, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| limit_error("archive record length does not fit u32"))?;
    output.push(kind);
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(payload);
    Ok(())
}

fn push_len(output: &mut Vec<u8>, len: usize) -> Result<()> {
    let len =
        u32::try_from(len).map_err(|_| limit_error("length-delimited field does not fit u32"))?;
    output.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn check_stored(len: usize, limits: &ArchiveLimits) -> Result<()> {
    if len as u64 > limits.max_stored_bytes {
        return Err(limit_error("archive exceeds its stored-byte limit"));
    }
    Ok(())
}

fn check_expanded(len: usize, limits: &ArchiveLimits) -> Result<()> {
    if len as u64 > limits.max_expanded_bytes {
        return Err(limit_error("archive exceeds its expanded-byte limit"));
    }
    Ok(())
}

fn checksum(bytes: &[u8]) -> String {
    checksum_digest(Sha256::digest(bytes).into())
}

fn checksum_digest(digest: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn archive_error(message: impl Into<String>) -> Error {
    Error::new("E_BACKUP_ARCHIVE", message)
}

fn limit_error(message: impl Into<String>) -> Error {
    Error::new("E_LIMIT", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn header(first: u64, last: u64) -> ArchiveHeader {
        ArchiveHeader {
            chain_id: "chain-01".into(),
            database_digest: "sha256:database".into(),
            first_sequence: first,
            last_sequence: last,
            catalog_codec: 3,
            value_codec: 3,
            receipt_codec: 2,
        }
    }

    fn baseline_frames() -> Vec<ArchiveFrame> {
        vec![
            ArchiveFrame::new(1, b"meta-v1".to_vec()),
            ArchiveFrame::write(2, b"catalog/1", b"type-v1").unwrap(),
            ArchiveFrame::write(3, b"row/1/1", b"value-v3").unwrap(),
            ArchiveFrame::write(4, b"migration/1", b"ledger-v1").unwrap(),
            ArchiveFrame::write(5, b"receipt/1", b"receipt-v2").unwrap(),
        ]
    }

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn manifest() -> ArchiveManifest {
        ArchiveManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            archive_codec: ARCHIVE_CODEC_VERSION,
            record_codec: 1,
            chain_id: "chain-01".into(),
            database_digest: digest('d'),
            state: ManifestState::Active,
            baseline: ManifestArtifact {
                path: format!("baselines/b-7-{}.uib", digest('a')),
                first_sequence: 7,
                last_sequence: 7,
                parent_checksum: None,
                payload_checksum: digest('a'),
                stored_checksum: digest('b'),
                compression: Compression::Zstd,
                stored_bytes: 100,
                expanded_bytes: 200,
            },
            segments: vec![ManifestArtifact {
                path: format!("segments/s-8-9-{}.uis", digest('c')),
                first_sequence: 8,
                last_sequence: 9,
                parent_checksum: Some(digest('a')),
                payload_checksum: digest('c'),
                stored_checksum: digest('e'),
                compression: Compression::Zstd,
                stored_bytes: 20,
                expanded_bytes: 40,
            }],
            recoverable_first_sequence: 7,
            recoverable_last_sequence: 9,
            stored_bytes: 120,
            expanded_bytes: 240,
            checksum: String::new(),
        }
    }

    #[test]
    fn none_and_zstd_share_payload_identity() {
        let limits = ArchiveLimits::default();
        let none = encode_archive(
            ArchiveKind::Baseline,
            1,
            Compression::None,
            &header(7, 7),
            &baseline_frames(),
            &limits,
        )
        .unwrap();
        let zstd = encode_archive(
            ArchiveKind::Baseline,
            1,
            Compression::Zstd,
            &header(7, 7),
            &baseline_frames(),
            &limits,
        )
        .unwrap();
        assert_eq!(none.payload_checksum, zstd.payload_checksum);
        assert_ne!(none.stored_checksum, zstd.stored_checksum);
        assert_eq!(
            decode_archive(&none.bytes, &limits).unwrap().frames,
            baseline_frames()
        );
        assert_eq!(
            decode_archive(&zstd.bytes, &limits).unwrap().frames,
            baseline_frames()
        );
    }

    #[test]
    fn canonical_baseline_golden_vector_is_stable() {
        let encoded = encode_archive(
            ArchiveKind::Baseline,
            1,
            Compression::None,
            &header(7, 7),
            &baseline_frames(),
            &ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(
            encoded.payload_checksum,
            "sha256:79ac38a55219a89add8447f77c6e9a1b7cc21aef0e90d829d6ff57752c4c051b"
        );
        assert_eq!(
            encoded.stored_checksum,
            "sha256:a7b2cfb800be50828e941fd6bdfc43fe8230f1fd78fca14775ebbf3f35984b84"
        );
        assert_eq!(
            decode_archive(&encoded.bytes, &ArchiveLimits::default())
                .unwrap()
                .frames,
            baseline_frames()
        );
    }

    #[test]
    fn rejects_corruption_truncation_order_and_limits() {
        let limits = ArchiveLimits::default();
        let encoded = encode_archive(
            ArchiveKind::Baseline,
            1,
            Compression::None,
            &header(7, 7),
            &baseline_frames(),
            &limits,
        )
        .unwrap();
        let mut corrupt = encoded.bytes.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode_archive(&corrupt, &limits).is_err());
        assert!(decode_archive(&encoded.bytes[..encoded.bytes.len() - 1], &limits).is_err());

        let unordered = vec![
            ArchiveFrame::new(1, b"meta".to_vec()),
            ArchiveFrame::write(3, b"b", b"v").unwrap(),
            ArchiveFrame::write(3, b"a", b"v").unwrap(),
        ];
        assert!(
            encode_archive(
                ArchiveKind::Baseline,
                1,
                Compression::None,
                &header(1, 1),
                &unordered,
                &limits
            )
            .is_err()
        );

        let small = ArchiveLimits {
            max_expanded_bytes: 16,
            ..limits.clone()
        };
        assert!(
            encode_archive(
                ArchiveKind::Baseline,
                1,
                Compression::Zstd,
                &header(7, 7),
                &baseline_frames(),
                &small
            )
            .is_err()
        );

        let compressed = encode_archive(
            ArchiveKind::Baseline,
            1,
            Compression::Zstd,
            &header(7, 7),
            &baseline_frames(),
            &limits,
        )
        .unwrap();
        let decode_limit = ArchiveLimits {
            max_expanded_bytes: compressed.expanded_bytes - 1,
            ..limits
        };
        assert!(decode_archive(&compressed.bytes, &decode_limit).is_err());
    }

    #[test]
    fn segment_requires_complete_ordered_commits() {
        let frames = vec![
            ArchiveFrame::new(16, b"commit-8".to_vec()),
            ArchiveFrame::delete(17, b"catalog/0").unwrap(),
            ArchiveFrame::write(18, b"catalog/1", b"next").unwrap(),
            ArchiveFrame::delete(19, b"row/1/1").unwrap(),
            ArchiveFrame::write(20, b"row/1/2", b"value-v3").unwrap(),
            ArchiveFrame::delete(21, b"migration/0").unwrap(),
            ArchiveFrame::write(22, b"migration/1", b"ledger-v1").unwrap(),
            ArchiveFrame::delete(23, b"receipt/0").unwrap(),
            ArchiveFrame::write(24, b"receipt/1", b"receipt-v2").unwrap(),
            ArchiveFrame::new(25, b"commit-end".to_vec()),
        ];
        let encoded = encode_archive(
            ArchiveKind::Segment,
            1,
            Compression::Zstd,
            &header(8, 8),
            &frames,
            &ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(
            encoded.payload_checksum,
            "sha256:df71e8b3e4679f854bd95dd27405d7e7de712db7e3ed11a8d47607053272dcd3"
        );
        assert_eq!(
            decode_archive(&encoded.bytes, &ArchiveLimits::default())
                .unwrap()
                .frames,
            frames
        );
    }

    #[test]
    fn manifest_is_canonical_bounded_and_chain_checked() {
        let encoded = encode_manifest(manifest()).unwrap();
        let decoded = decode_manifest(&encoded).unwrap();
        assert_eq!(encode_manifest(decoded.clone()).unwrap(), encoded);
        assert_eq!(
            decoded.checksum,
            "sha256:016596bf091a6de3c439b777ab6755c8cbbf6eceb30c25a06197bd9c4bbc3f03"
        );
        assert_eq!(
            checksum(&encoded),
            "sha256:98e35ed3154a0a07fb6480a8232f72de9bc6b09f3207f60db4ad8c7f1ea4549a"
        );

        let mut fork = decoded;
        fork.segments[0].parent_checksum = Some(digest('f'));
        fork.checksum.clear();
        assert!(encode_manifest(fork).is_err());

        let noncanonical = [b" ".as_slice(), encoded.as_slice()].concat();
        assert!(decode_manifest(&noncanonical).is_err());
        assert!(decode_manifest(&vec![b'x'; MAX_MANIFEST_BYTES + 1]).is_err());
    }

    #[test]
    fn relative_paths_and_symlinks_fail_closed() {
        assert!(validate_relative_archive_path(Path::new("segments/s.uis")).is_ok());
        assert!(validate_relative_archive_path(Path::new("../outside")).is_err());
        assert!(validate_relative_archive_path(Path::new("/absolute")).is_err());

        let root =
            std::env::temp_dir().join(format!("unionid-archive-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("segments")).unwrap();
        fs::write(root.join("segments/ok.uis"), b"ok").unwrap();
        validate_archive_entry(&root, Path::new("segments/ok.uis")).unwrap();
        assert!(
            write_new_archive_entry(&root, Path::new("segments/ok.uis"), b"different").is_err()
        );
        write_new_archive_entry(&root, Path::new("segments/ok.uis"), b"ok").unwrap();
        write_new_archive_entry(&root, Path::new("segments/new.uis"), b"new").unwrap();
        assert_eq!(fs::read(root.join("segments/new.uis")).unwrap(), b"new");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                root.join("segments/ok.uis"),
                root.join("segments/link.uis"),
            )
            .unwrap();
            assert!(validate_archive_entry(&root, Path::new("segments/link.uis")).is_err());
            fs::create_dir(root.join("outside")).unwrap();
            std::os::unix::fs::symlink(root.join("outside"), root.join("linked-dir")).unwrap();
            assert!(
                write_new_archive_entry(&root, Path::new("linked-dir/new.uis"), b"escape").is_err()
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
