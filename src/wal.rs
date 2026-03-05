use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::query::parse_statement;

#[derive(Debug)]
pub struct Wal {
    path: PathBuf,
}

impl Wal {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create wal dir '{}' failed: {e}", parent.display()))?;
            }
        }

        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open wal '{}' failed: {e}", path.display()))?;

        Ok(Self { path })
    }

    pub fn append(&self, request: &str) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open wal '{}' failed: {e}", self.path.display()))?;

        file.write_all(request.as_bytes())
            .map_err(|e| format!("write wal '{}' failed: {e}", self.path.display()))?;
        file.write_all(b"\n")
            .map_err(|e| format!("write wal newline '{}' failed: {e}", self.path.display()))?;
        file.flush()
            .map_err(|e| format!("flush wal '{}' failed: {e}", self.path.display()))
    }

    pub fn replay_into(&self, db: &mut Database) -> Result<usize, String> {
        replay_from_path(&self.path, db)
    }

    pub fn truncate(&self) -> Result<(), String> {
        File::create(&self.path)
            .map(|_| ())
            .map_err(|e| format!("truncate wal '{}' failed: {e}", self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn replay_from_path(path: &Path, db: &mut Database) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }

    let file =
        File::open(path).map_err(|e| format!("open wal '{}' failed: {e}", path.display()))?;
    let reader = BufReader::new(file);

    let mut applied = 0usize;
    for (index, line) in reader.lines().enumerate() {
        let raw = line.map_err(|e| format!("read wal line {} failed: {e}", index + 1))?;
        let stmt_text = raw.trim();
        if stmt_text.is_empty() {
            continue;
        }

        let stmt = parse_statement(stmt_text)
            .map_err(|e| format!("parse wal line {} failed: {}", index + 1, e))?;

        if !stmt.is_mutating() {
            return Err(format!(
                "wal line {} is not mutable statement (only create table/create index/insert allowed)",
                index + 1
            ));
        }

        let resp = db.execute(stmt);
        if !resp.ok {
            return Err(format!(
                "apply wal line {} failed: {}",
                index + 1,
                resp.message
            ));
        }
        applied += 1;
    }

    Ok(applied)
}
