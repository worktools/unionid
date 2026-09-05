use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use crate::db::{Database, QueryResponse};
use crate::error::{Error, Result};
use crate::snapshot::SnapshotStore;
use crate::syntax;
use crate::wal::Wal;

/// The shared execution boundary. A source request is one atomic batch.
/// The preview stages writes by cloning its small in-memory database.
#[derive(Default)]
pub struct Engine {
    db: Database,
    wal: Option<Wal>,
    snapshot: Option<SnapshotStore>,
    snapshot_every: usize,
    writes_since_snapshot: usize,
    write_failed: bool,
    _locks: Vec<File>,
}

impl Engine {
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn open(
        wal_path: Option<PathBuf>,
        snapshot_path: Option<PathBuf>,
        snapshot_every: usize,
    ) -> Result<Self> {
        let wal_path = wal_path.map(canonical_existing_path).transpose()?;
        let snapshot_path = snapshot_path.map(canonical_existing_path).transpose()?;
        if snapshot_path.is_some() && wal_path.is_none() {
            return Err(Error::new("E_CONFIG", "snapshot requires a WAL path"));
        }
        if snapshot_every > 0 && snapshot_path.is_none() {
            return Err(Error::new(
                "E_CONFIG",
                "snapshot-every requires a snapshot path",
            ));
        }
        if wal_path.is_some() && wal_path == snapshot_path {
            return Err(Error::new(
                "E_CONFIG",
                "WAL and snapshot must use different files",
            ));
        }
        let mut locks = Vec::new();
        for path in wal_path.iter().chain(snapshot_path.iter()) {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| Error::new("E_IO", e.to_string()))?;
            }
            let mut lock_name = path.as_os_str().to_os_string();
            lock_name.push(".lock");
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(PathBuf::from(lock_name))
                .map_err(|e| Error::new("E_IO", format!("open database lock: {e}")))?;
            lock.try_lock().map_err(|e| {
                Error::new(
                    "E_BUSY",
                    format!(
                        "database file '{}' is already in use or cannot be locked: {e}",
                        path.display()
                    ),
                )
            })?;
            locks.push(lock);
        }
        let snapshot = snapshot_path
            .map(SnapshotStore::new)
            .transpose()
            .map_err(|e| Error::new("E_STORAGE", e))?;
        let mut db = snapshot
            .as_ref()
            .map(SnapshotStore::load)
            .transpose()
            .map_err(|e| Error::new("E_STORAGE", e))?
            .flatten()
            .unwrap_or_default();
        let wal = wal_path
            .map(Wal::new)
            .transpose()
            .map_err(|e| Error::new("E_STORAGE", e))?;
        if let Some(wal) = &wal {
            wal.replay_into(&mut db)
                .map_err(|e| Error::new("E_STORAGE", e))?;
        }
        db.rebuild_indexes()?;
        Ok(Self {
            db,
            wal,
            snapshot,
            snapshot_every,
            writes_since_snapshot: 0,
            write_failed: false,
            _locks: locks,
        })
    }

    pub fn execute(&mut self, source: &str) -> QueryResponse {
        match self.try_execute(source) {
            Ok(response) => response,
            Err(error) => QueryResponse::failure(error),
        }
    }

    fn try_execute(&mut self, source: &str) -> Result<QueryResponse> {
        let statements = syntax::parse(source)?;
        let mutating = statements.iter().any(|s| s.statement.is_mutating());
        if mutating && self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "writes are disabled after a storage failure; reopen the database to resolve the commit state",
            ));
        }
        let mut candidate = if mutating {
            Some(self.db.clone())
        } else {
            None
        };
        let target = candidate.as_mut().unwrap_or(&mut self.db);
        let mut response = QueryResponse::ok_message("ok");
        for located in statements {
            response = target
                .execute(located.statement)
                .map_err(|e| e.at(located.span))?;
        }
        if let Some(mut candidate) = candidate {
            candidate.sequence = self
                .db
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
            if let Some(wal) = &self.wal
                && let Err(error) = wal.append(candidate.sequence, source)
            {
                self.write_failed = true;
                return Err(Error::new(
                    "E_STORAGE",
                    format!(
                        "WAL commit failed; state was not published, but disk commit may be uncertain: {error}; reopen before retrying"
                    ),
                ));
            }
            self.db = candidate;
            self.writes_since_snapshot += 1;
            if self.snapshot_every > 0 && self.writes_since_snapshot >= self.snapshot_every {
                // The WAL commit has already succeeded. Checkpoint failure is a
                // maintenance warning, never a false report that the write failed.
                if let Err(error) = self.checkpoint() {
                    response.warnings.push(error.to_string());
                }
            }
        }
        Ok(response)
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "checkpoint is disabled after a storage failure; reopen the database to resolve the commit state",
            ));
        }
        if let Some(snapshot) = &self.snapshot {
            snapshot
                .save(&self.db)
                .map_err(|e| Error::new("E_CHECKPOINT", e))?;
            if let Some(wal) = &self.wal {
                wal.truncate().map_err(|e| Error::new("E_CHECKPOINT", e))?;
            }
            self.writes_since_snapshot = 0;
        }
        Ok(())
    }

    pub fn schema(&self) -> String {
        self.db.schema_text()
    }
    pub fn tables(&self) -> Vec<String> {
        self.db.table_names()
    }
}

fn canonical_existing_path(path: PathBuf) -> Result<PathBuf> {
    match std::fs::canonicalize(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(&path).is_ok() {
                Err(Error::new(
                    "E_CONFIG",
                    "database paths must not be dangling symbolic links",
                ))
            } else {
                Ok(path)
            }
        }
        Err(error) => Err(Error::new(
            "E_IO",
            format!("resolve database path: {error}"),
        )),
    }
}
