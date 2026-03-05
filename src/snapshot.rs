use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use crate::db::Database;

#[derive(Debug)]
pub struct SnapshotStore {
    path: PathBuf,
}

impl SnapshotStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("create snapshot dir '{}' failed: {e}", parent.display())
                })?;
            }
        }
        Ok(Self { path })
    }

    pub fn load(&self) -> Result<Option<Database>, String> {
        if !self.path.exists() {
            return Ok(None);
        }

        let file = File::open(&self.path)
            .map_err(|e| format!("open snapshot '{}' failed: {e}", self.path.display()))?;
        let reader = BufReader::new(file);
        let db = serde_json::from_reader(reader)
            .map_err(|e| format!("parse snapshot '{}' failed: {e}", self.path.display()))?;
        Ok(Some(db))
    }

    pub fn save(&self, db: &Database) -> Result<(), String> {
        let file = File::create(&self.path)
            .map_err(|e| format!("create snapshot '{}' failed: {e}", self.path.display()))?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, db)
            .map_err(|e| format!("write snapshot '{}' failed: {e}", self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
