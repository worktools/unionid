use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use serde::Serialize;

use crate::db::{Database, QueryResponse};
use crate::error::{Error, Result};
use crate::redb_storage::{CommitFailure, RedbStore};
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
    durable: Option<Box<dyn DurableBackend>>,
    _locks: Vec<File>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageIntegrity {
    pub backend: &'static str,
    pub backend_clean: bool,
    pub schema: crate::db::SchemaInfo,
}

trait DurableBackend: Send {
    fn commit(&mut self, database: &Database) -> std::result::Result<(), CommitFailure>;
    fn check_integrity(&mut self) -> Result<(bool, Database)>;
}

impl DurableBackend for RedbStore {
    fn commit(&mut self, database: &Database) -> std::result::Result<(), CommitFailure> {
        RedbStore::commit(self, database)
    }

    fn check_integrity(&mut self) -> Result<(bool, Database)> {
        RedbStore::check_integrity(self)
    }
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
            durable: None,
            _locks: locks,
        })
    }

    /// Open the durable redb backend. Every mutating source request is
    /// committed as one synchronous, two-phase redb transaction.
    pub fn open_redb(path: impl Into<PathBuf>) -> Result<Self> {
        let (redb, db) = RedbStore::open(path)?;
        Ok(Self {
            db,
            durable: Some(Box::new(redb)),
            ..Self::default()
        })
    }

    pub fn execute(&mut self, source: &str) -> QueryResponse {
        match self.try_execute(source) {
            Ok(response) => response,
            Err(error) => self.with_schema(QueryResponse::failure(error)),
        }
    }

    fn try_execute(&mut self, source: &str) -> Result<QueryResponse> {
        let statements = syntax::parse(source)?;
        let mutating = statements.iter().any(|s| s.statement.is_mutating());
        let schema_changing = statements.iter().any(|s| s.statement.changes_schema());
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
            if schema_changing {
                candidate.advance_schema_revision()?;
            }
            candidate.sequence = self
                .db
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
            if let Some(durable) = &mut self.durable {
                match durable.commit(&candidate) {
                    Ok(()) => {}
                    Err(CommitFailure::Definite(error)) => {
                        return Err(Error::new(
                            "E_STORAGE",
                            format!(
                                "durable transaction aborted before commit; state was not changed: {}",
                                error.message
                            ),
                        ));
                    }
                    Err(CommitFailure::Uncertain(error)) => {
                        self.durable = None;
                        self.write_failed = true;
                        return Err(Error::new(
                            "E_STORAGE",
                            format!(
                                "redb commit result is uncertain; state was not published in this process: {}; reopen the database before retrying",
                                error.message
                            ),
                        ));
                    }
                }
            } else if let Some(wal) = &self.wal
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
        Ok(self.with_schema(response))
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

    pub fn check_integrity(&mut self) -> Result<StorageIntegrity> {
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "integrity check requires reopening after an uncertain commit",
            ));
        }
        let durable = self.durable.as_mut().ok_or_else(|| {
            Error::new(
                "E_CONFIG",
                "integrity check requires a database opened with Engine::open_redb",
            )
        })?;
        let (backend_clean, database) = durable.check_integrity()?;
        self.db = database;
        Ok(StorageIntegrity {
            backend: "redb",
            backend_clean,
            schema: self.db.schema_info(),
        })
    }

    pub fn schema(&self) -> String {
        self.db.schema_text()
    }
    pub fn tables(&self) -> Vec<String> {
        self.db.table_names()
    }

    pub fn schema_info(&self) -> crate::db::SchemaInfo {
        self.db.schema_info()
    }

    fn with_schema(&self, mut response: QueryResponse) -> QueryResponse {
        response.schema = Some(self.db.schema_info());
        response
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

#[cfg(test)]
mod tests {
    use super::*;

    struct FailOnce {
        uncertain: Option<bool>,
    }

    impl DurableBackend for FailOnce {
        fn commit(&mut self, _: &Database) -> std::result::Result<(), CommitFailure> {
            match self.uncertain.take() {
                Some(false) => Err(CommitFailure::Definite(Error::new(
                    "E_STORAGE",
                    "injected pre-commit failure",
                ))),
                Some(true) => Err(CommitFailure::Uncertain(Error::new(
                    "E_STORAGE",
                    "injected commit failure",
                ))),
                None => Ok(()),
            }
        }

        fn check_integrity(&mut self) -> Result<(bool, Database)> {
            unreachable!()
        }
    }

    fn engine_with_failure(uncertain: bool) -> Engine {
        Engine {
            durable: Some(Box::new(FailOnce {
                uncertain: Some(uncertain),
            })),
            ..Engine::default()
        }
    }

    #[test]
    fn definite_pre_commit_failure_keeps_old_state_and_allows_retry() {
        let mut engine = engine_with_failure(false);
        let failed = engine.execute("create table entries (id int)");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_STORAGE");
        assert!(failed.message.contains("aborted before commit"));
        assert_eq!(
            engine.execute("from entries").error.unwrap().code,
            "E_TABLE"
        );
        assert!(engine.execute("create table entries (id int)").ok);
        assert!(engine.execute("from entries").ok);
    }

    #[test]
    fn uncertain_commit_failure_requires_reopen_before_another_write() {
        let mut engine = engine_with_failure(true);
        let failed = engine.execute("create table entries (id int)");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_STORAGE");
        assert!(failed.message.contains("result is uncertain"));
        assert_eq!(
            engine.execute("from entries").error.unwrap().code,
            "E_TABLE"
        );
        let blocked = engine.execute("create table later (id int)");
        assert!(!blocked.ok);
        assert!(blocked.message.contains("writes are disabled"));
    }
}
