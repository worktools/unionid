use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::syntax;

// A source byte needs at most six JSON bytes (e.g. a control character's
// Unicode escape), plus the record envelope, sequence and newline.
pub const MAX_RECORD_BYTES: usize = syntax::MAX_SOURCE_BYTES * 6 + 256;

#[derive(Debug)]
pub struct Wal {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Record {
    format_version: u32,
    sequence: u64,
    source: String,
}

impl Wal {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| format!("create WAL directory: {e}"))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open WAL '{}': {e}", path.display()))?;
        file.sync_all().map_err(|e| format!("sync WAL file: {e}"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("sync WAL directory: {e}"))?;
        Ok(Self { path })
    }

    pub fn append(&self, sequence: u64, source: &str) -> Result<(), String> {
        if source.len() > syntax::MAX_SOURCE_BYTES {
            return Err(format!(
                "WAL source exceeds {} byte limit",
                syntax::MAX_SOURCE_BYTES
            ));
        }
        let mut encoded = serde_json::to_vec(&Record {
            format_version: 1,
            sequence,
            source: source.into(),
        })
        .map_err(|e| e.to_string())?;
        encoded.push(b'\n');
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open WAL: {e}"))?;
        file.write_all(&encoded)
            .map_err(|e| format!("write WAL: {e}"))?;
        file.sync_all().map_err(|e| format!("sync WAL: {e}"))
    }

    pub fn replay_into(&self, db: &mut Database) -> Result<usize, String> {
        replay_from_path(&self.path, db)
    }
    pub fn truncate(&self) -> Result<(), String> {
        File::create(&self.path)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("truncate WAL: {e}"))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn replay_from_path(path: &Path, db: &mut Database) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let file = File::open(path).map_err(|e| format!("open WAL: {e}"))?;
    let mut reader = BufReader::new(file);
    let mut applied = 0;
    let snapshot_sequence = db.sequence;
    let mut encoded = Vec::new();
    let mut line_number = 0;
    loop {
        encoded.clear();
        let n = (&mut reader)
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_until(b'\n', &mut encoded)
            .map_err(|e| format!("read WAL: {e}"))?;
        if n == 0 {
            break;
        }
        line_number += 1;
        if n > MAX_RECORD_BYTES {
            return Err(format!(
                "WAL line {line_number}: record exceeds {MAX_RECORD_BYTES} byte limit; preserve this file and restore/repair explicitly"
            ));
        }
        let line = std::str::from_utf8(&encoded)
            .map_err(|e| format!("WAL line {line_number}: invalid UTF-8: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        if !line.ends_with('\n') {
            return Err(format!(
                "WAL line {line_number}: incomplete final record; preserve this file and restore/repair explicitly"
            ));
        }
        let (sequence, source) = if line.trim_start().starts_with('{') {
            let record: Record =
                serde_json::from_str(line).map_err(|e| format!("WAL line {line_number}: {e}"))?;
            if record.format_version != 1 {
                return Err(format!(
                    "WAL line {line_number}: unsupported format version {}",
                    record.format_version
                ));
            }
            if record.sequence == 0 {
                return Err(format!("WAL line {line_number}: invalid commit sequence 0"));
            }
            if record.sequence <= snapshot_sequence {
                continue;
            }
            if Some(record.sequence) != db.sequence.checked_add(1) {
                return Err(format!(
                    "WAL line {line_number}: noncontiguous commit sequence"
                ));
            }
            (record.sequence, record.source)
        } else {
            // Read original one-line prototype logs for supported legacy syntax.
            (
                db.sequence
                    .checked_add(1)
                    .ok_or_else(|| format!("WAL line {line_number}: commit sequence exhausted"))?,
                line.trim().to_string(),
            )
        };
        let statements =
            syntax::parse(&source).map_err(|e| format!("WAL line {line_number}: {e}"))?;
        if !statements.iter().any(|s| s.statement.is_mutating()) {
            return Err(format!("WAL line {line_number}: no mutation"));
        }
        let mut candidate = db.clone();
        for statement in statements {
            candidate
                .execute(statement.statement)
                .map_err(|e| format!("WAL line {line_number}: {e}"))?;
        }
        candidate.sequence = sequence;
        *db = candidate;
        applied += 1;
    }
    Ok(applied)
}
