use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::db::Database;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct SnapshotStore {
    path: PathBuf,
}

impl SnapshotStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| format!("create snapshot directory: {e}"))?;
        }
        Ok(Self { path })
    }

    pub fn load(&self) -> Result<Option<Database>, String> {
        if !self.path.exists() {
            return Ok(None);
        }
        let file = File::open(&self.path).map_err(|e| format!("open snapshot: {e}"))?;
        let mut db: Database = serde_json::from_reader(BufReader::new(file))
            .map_err(|e| format!("parse snapshot: {e}"))?;
        db.rebuild_indexes().map_err(|e| e.to_string())?;
        Ok(Some(db))
    }

    pub fn save(&self, db: &Database) -> Result<(), String> {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = PathBuf::from(name);
        let result = (|| {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, db)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            // serde_json's reader has a nesting limit that its writer does not.
            // Never replace the recovery snapshot or discard the WAL with a
            // snapshot that this same implementation cannot reopen.
            let _: Database = serde_json::from_reader(BufReader::new(File::open(&temp)?))?;
            fs::rename(&temp, &self.path)?;
            let parent = self
                .path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            File::open(parent)?.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(|e| format!("publish snapshot '{}': {e}", self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
