use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::db::{Database, QueryResponse};
use crate::error::{Error, Result};
use crate::introspection::{Introspection, StorageMode};
use crate::migration::{
    MigrationApply, MigrationEntry, MigrationFile, MigrationPlan, MigrationPlanItem,
    MigrationStatus, describe_step, validate_files_against_history,
};
use crate::query::{LocatedStatement, Statement};
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
    storage_mode: StorageMode,
    _locks: Vec<File>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageIntegrity {
    pub backend: &'static str,
    pub backend_clean: bool,
    pub schema: crate::db::SchemaInfo,
}

/// A parsed query or supported parameterized operation bound to one schema
/// identity. Parameter values are supplied for each execution and type checked
/// before rows are scanned or mutations are published.
#[derive(Debug, Clone)]
pub struct PreparedQuery {
    source: String,
    statements: Vec<LocatedStatement>,
    schema: crate::db::SchemaInfo,
    parameters: Vec<String>,
    parameter_types: std::collections::BTreeMap<String, String>,
    mutating: bool,
}

impl PreparedQuery {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn schema(&self) -> &crate::db::SchemaInfo {
        &self.schema
    }

    pub fn parameters(&self) -> &[String] {
        &self.parameters
    }

    pub fn parameter_types(&self) -> &std::collections::BTreeMap<String, String> {
        &self.parameter_types
    }
}

trait DurableBackend: Send {
    fn commit(
        &mut self,
        previous: &Database,
        database: &Database,
    ) -> std::result::Result<(), CommitFailure>;
    fn check_integrity(&mut self) -> Result<(bool, Database)>;
}

impl DurableBackend for RedbStore {
    fn commit(
        &mut self,
        previous: &Database,
        database: &Database,
    ) -> std::result::Result<(), CommitFailure> {
        RedbStore::commit(self, previous, database)
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
        let storage_mode = match (&wal, &snapshot) {
            (Some(_), Some(_)) => StorageMode::LegacyWalSnapshot,
            (Some(_), None) => StorageMode::LegacyWal,
            (None, _) => StorageMode::Memory,
        };
        Ok(Self {
            db,
            wal,
            snapshot,
            snapshot_every,
            writes_since_snapshot: 0,
            write_failed: false,
            durable: None,
            storage_mode,
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
            storage_mode: StorageMode::Redb,
            ..Self::default()
        })
    }

    pub fn execute(&mut self, source: &str) -> QueryResponse {
        self.execute_with_params_at_schema_and_deadline(
            source,
            std::collections::BTreeMap::new(),
            None,
            None,
        )
    }

    pub fn execute_with_params(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
    ) -> QueryResponse {
        self.execute_with_params_at_schema_and_deadline(source, parameters, None, None)
    }

    pub fn execute_with_params_at_schema(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
    ) -> QueryResponse {
        self.execute_with_params_at_schema_and_deadline(source, parameters, expected_schema, None)
    }

    pub fn execute_with_params_until(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: std::time::Instant,
    ) -> QueryResponse {
        self.execute_with_params_at_schema_and_deadline(
            source,
            parameters,
            expected_schema,
            Some(deadline),
        )
    }

    fn execute_with_params_at_schema_and_deadline(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<std::time::Instant>,
    ) -> QueryResponse {
        match self.try_execute_with_params(source, parameters, expected_schema, deadline) {
            Ok(response) => response,
            Err(error) => self.with_schema(QueryResponse::failure(error)),
        }
    }

    pub fn prepare(&self, source: &str) -> Result<PreparedQuery> {
        let mut statements = syntax::parse(source)?;
        let mut mutating = false;
        for located in &mut statements {
            match &mut located.statement {
                Statement::Explain(pipeline) | Statement::Pipeline(pipeline) => self
                    .db
                    .prepare_pipeline(pipeline)
                    .map(|_| ())
                    .map_err(|error| error.at(located.span))?,
                Statement::InsertManyParameter {
                    table,
                    parameter_type,
                    returning,
                    ..
                } => {
                    *parameter_type = Some(
                        self.db
                            .prepare_bulk_insert_parameter(table, returning.as_ref())
                            .map_err(|error| error.at(located.span))?,
                    );
                    mutating = true;
                }
                Statement::InsertParameter {
                    table,
                    parameter_type,
                    returning,
                    ..
                } => {
                    *parameter_type = Some(
                        self.db
                            .prepare_insert_parameter(table, returning.as_ref())
                            .map_err(|error| error.at(located.span))?,
                    );
                    mutating = true;
                }
                Statement::UpsertParameter {
                    table,
                    parameter_type,
                    returning,
                    ..
                } => {
                    *parameter_type = Some(
                        self.db
                            .prepare_upsert_parameter(table, returning.as_ref())
                            .map_err(|error| error.at(located.span))?,
                    );
                    mutating = true;
                }
                Statement::UpsertManyParameter {
                    table,
                    parameter_type,
                    returning,
                    ..
                } => {
                    *parameter_type = Some(
                        self.db
                            .prepare_bulk_upsert_parameter(table, returning.as_ref())
                            .map_err(|error| error.at(located.span))?,
                    );
                    mutating = true;
                }
                Statement::Update {
                    target,
                    assignments,
                    returning,
                } => {
                    self.db
                        .prepare_update(target, assignments, returning.as_ref())
                        .map_err(|error| error.at(located.span))?;
                    mutating = true;
                }
                Statement::Delete { target, returning } => {
                    self.db
                        .prepare_delete(target, returning.as_ref())
                        .map_err(|error| error.at(located.span))?;
                    mutating = true;
                }
                _ => {
                    return Err(Error::new(
                        "E_PREPARE",
                        "prepared operations support read pipelines, explain, parameterized insert/upsert, and update/delete",
                    )
                    .at(located.span));
                }
            }
        }
        let parameter_types = crate::params::types(&statements)?
            .into_iter()
            .map(|(name, ty)| (name, self.db.catalog.describe(&ty)))
            .collect();
        Ok(PreparedQuery {
            source: source.into(),
            parameters: crate::params::names(&statements).into_iter().collect(),
            parameter_types,
            statements,
            schema: self.db.schema_info(),
            mutating,
        })
    }

    pub fn query(
        &mut self,
        prepared: &PreparedQuery,
        parameters: std::collections::BTreeMap<String, crate::Value>,
    ) -> QueryResponse {
        self.execute_prepared_with_deadline(prepared, parameters, None)
    }

    fn execute_prepared_with_deadline(
        &mut self,
        prepared: &PreparedQuery,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        deadline: Option<std::time::Instant>,
    ) -> QueryResponse {
        if prepared.schema != self.db.schema_info() {
            return self.with_schema(QueryResponse::failure(Error::new(
                "E_SCHEMA_CHANGED",
                format!(
                    "prepared query uses schema revision {} ({}) but the database is at revision {} ({})",
                    prepared.schema.revision,
                    prepared.schema.hash,
                    self.db.schema_info().revision,
                    self.db.schema_info().hash
                ),
            )));
        }
        if prepared.mutating && self.wal.is_some() {
            return self.with_schema(QueryResponse::failure(Error::new(
                "E_CONFIG",
                "parameterized writes require redb or memory mode; the transitional WAL stores source text",
            )));
        }
        let mut statements = prepared.statements.clone();
        let result = crate::params::bind(&mut statements, &parameters)
            .and_then(|()| self.try_execute_statements(statements, None, deadline));
        match result {
            Ok(response) => response,
            Err(error) => self.with_schema(QueryResponse::failure(error)),
        }
    }

    pub fn execute_prepared(
        &mut self,
        prepared: &PreparedQuery,
        parameters: std::collections::BTreeMap<String, crate::Value>,
    ) -> QueryResponse {
        self.query(prepared, parameters)
    }

    pub fn execute_prepared_until(
        &mut self,
        prepared: &PreparedQuery,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        deadline: std::time::Instant,
    ) -> QueryResponse {
        self.execute_prepared_with_deadline(prepared, parameters, Some(deadline))
    }

    fn try_execute_with_params(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        if let Some(expected) = expected_schema
            && expected != &self.db.schema_info()
        {
            return Err(Error::new(
                "E_SCHEMA_CHANGED",
                format!(
                    "request expects schema revision {} ({}) but the database is at revision {} ({})",
                    expected.revision,
                    expected.hash,
                    self.db.schema_info().revision,
                    self.db.schema_info().hash
                ),
            ));
        }
        let mut statements = syntax::parse(source)?;
        crate::params::bind(&mut statements, &parameters)?;
        let contains_parameters = !parameters.is_empty();
        let mutating = statements
            .iter()
            .any(|statement| statement.statement.is_mutating());
        if contains_parameters && mutating && self.wal.is_some() {
            return Err(Error::new(
                "E_CONFIG",
                "parameterized writes require redb or memory mode; the transitional WAL stores source text",
            ));
        }
        self.try_execute_statements(statements, Some(source), deadline)
    }

    fn try_execute_statements(
        &mut self,
        statements: Vec<LocatedStatement>,
        wal_source: Option<&str>,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        let mutating = statements.iter().any(|s| s.statement.is_mutating());
        let schema_changing = statements.iter().any(|s| s.statement.changes_schema());
        if schema_changing && !self.db.migration_history().is_empty() {
            return Err(Error::new(
                "E_MIGRATION",
                "schema is managed by the migration ledger; use the versioned migration runner",
            ));
        }
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
            ensure_deadline(deadline)?;
            response = target
                .execute_with_deadline(located.statement, deadline)
                .map_err(|e| e.at(located.span))?;
        }
        ensure_deadline(deadline)?;
        if let Some(mut candidate) = candidate {
            if schema_changing {
                candidate.advance_schema_revision()?;
            }
            candidate.sequence = self
                .db
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
            self.commit_candidate(candidate, wal_source, &mut response)?;
        }
        Ok(self.with_schema(response))
    }

    pub fn migration_status(&self, files: &[MigrationFile]) -> Result<MigrationStatus> {
        let applied_count = validate_files_against_history(files, self.db.migration_history())?;
        Ok(MigrationStatus {
            schema: self.db.schema_info(),
            applied: self.db.migration_history().to_vec(),
            pending: files[applied_count..]
                .iter()
                .map(|file| file.id.clone())
                .collect(),
        })
    }

    pub fn plan_migrations(&self, files: &[MigrationFile]) -> Result<MigrationPlan> {
        let applied_count = validate_files_against_history(files, self.db.migration_history())?;
        let current_schema = self.db.schema_info();
        let mut candidate = self.db.clone();
        let mut pending = Vec::new();
        for file in &files[applied_count..] {
            let before = candidate.schema_info();
            candidate
                .execute(Statement::Migration {
                    name: file.id.clone(),
                    parent: file.parent.clone(),
                    steps: file.steps.clone(),
                })
                .map_err(|error| migration_file_error(file, error))?;
            candidate.advance_schema_revision()?;
            let after = candidate.schema_info();
            let operations = file.steps.iter().map(describe_step).collect::<Vec<_>>();
            pending.push(MigrationPlanItem {
                id: file.id.clone(),
                parent: file.parent.clone(),
                checksum: file.checksum.clone(),
                before,
                after: after.clone(),
                operations: operations
                    .iter()
                    .map(|(description, _)| description.clone())
                    .collect(),
                destructive: operations.iter().any(|(_, destructive)| *destructive),
            });
            candidate.append_migration(MigrationEntry {
                id: file.id.clone(),
                parent: file.parent.clone(),
                checksum: file.checksum.clone(),
                schema_revision: after.revision,
                schema_hash: after.hash,
                applied_at_unix_ms: 0,
            })?;
        }
        Ok(MigrationPlan {
            current_schema,
            target_schema: candidate.schema_info(),
            applied_count,
            pending,
        })
    }

    pub fn apply_migrations(&mut self, files: &[MigrationFile]) -> Result<MigrationApply> {
        if self.wal.is_some() {
            return Err(Error::new(
                "E_CONFIG",
                "versioned migrations require redb or an in-memory Engine",
            ));
        }
        let applied_count = validate_files_against_history(files, self.db.migration_history())?;
        let skipped = files[..applied_count]
            .iter()
            .map(|file| file.id.clone())
            .collect();
        let mut applied = Vec::new();
        for file in &files[applied_count..] {
            if let Err(error) = self.apply_migration_file(file) {
                if applied.is_empty() {
                    return Err(error);
                }
                return Err(Error::new(
                    &error.code,
                    format!(
                        "{}; {} earlier migration(s) were committed: {}",
                        error.message,
                        applied.len(),
                        applied.join(", ")
                    ),
                ));
            }
            applied.push(file.id.clone());
        }
        Ok(MigrationApply {
            applied,
            skipped,
            schema: self.db.schema_info(),
        })
    }

    fn apply_migration_file(&mut self, file: &MigrationFile) -> Result<()> {
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "writes are disabled after a storage failure; reopen the database to resolve the commit state",
            ));
        }
        let mut candidate = self.db.clone();
        candidate
            .execute(Statement::Migration {
                name: file.id.clone(),
                parent: file.parent.clone(),
                steps: file.steps.clone(),
            })
            .map_err(|error| migration_file_error(file, error))?;
        candidate.advance_schema_revision()?;
        candidate.sequence = self
            .db
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
        let schema = candidate.schema_info();
        let applied_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| Error::new("E_TIME", error.to_string()))?
            .as_millis()
            .try_into()
            .map_err(|_| Error::new("E_LIMIT", "migration timestamp exceeds u64"))?;
        candidate.append_migration(MigrationEntry {
            id: file.id.clone(),
            parent: file.parent.clone(),
            checksum: file.checksum.clone(),
            schema_revision: schema.revision,
            schema_hash: schema.hash,
            applied_at_unix_ms,
        })?;
        let mut response = QueryResponse::ok_message(format!("migration '{}' applied", file.id));
        self.commit_candidate(candidate, None, &mut response)
    }

    fn commit_candidate(
        &mut self,
        candidate: Database,
        wal_source: Option<&str>,
        response: &mut QueryResponse,
    ) -> Result<()> {
        if let Some(durable) = &mut self.durable {
            match durable.commit(&self.db, &candidate) {
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
        } else if let Some(wal) = &self.wal {
            let source = wal_source.ok_or_else(|| {
                Error::new(
                    "E_CONFIG",
                    "versioned migration commits cannot be represented by the transitional WAL",
                )
            })?;
            if let Err(error) = wal.append(candidate.sequence, source) {
                self.write_failed = true;
                return Err(Error::new(
                    "E_STORAGE",
                    format!(
                        "WAL commit failed; state was not published, but disk commit may be uncertain: {error}; reopen before retrying"
                    ),
                ));
            }
        }
        self.db = candidate;
        self.writes_since_snapshot += 1;
        if self.snapshot_every > 0
            && self.writes_since_snapshot >= self.snapshot_every
            && let Err(error) = self.checkpoint()
        {
            response.warnings.push(error.to_string());
        }
        Ok(())
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

    pub fn introspection(&self) -> Introspection {
        Introspection {
            schema: self.db.schema_info(),
            schema_source: self.db.schema_text(),
            tables: self.db.table_names(),
            types: self.db.type_names(),
            fields: self.db.field_names(),
            storage: self.storage_mode,
            migration_count: self.db.migration_history().len(),
            migration_head: self
                .db
                .migration_history()
                .last()
                .map(|entry| entry.id.clone()),
        }
    }

    pub fn schema_info(&self) -> crate::db::SchemaInfo {
        self.db.schema_info()
    }

    pub fn migration_history(&self) -> &[MigrationEntry] {
        self.db.migration_history()
    }

    pub(crate) fn database_snapshot(&self) -> Database {
        self.db.clone()
    }

    pub(crate) fn restore_redb(path: PathBuf, database: Database) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::new("E_IO", error.to_string()))?;
        }
        let reservation = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                Error::new(
                    "E_BACKUP",
                    format!("reserve restore target '{}': {error}", path.display()),
                )
            })?;
        if let Err(error) = reservation.sync_all() {
            drop(reservation);
            let _ = std::fs::remove_file(&path);
            return Err(Error::new("E_IO", error.to_string()));
        }
        drop(reservation);
        let result = (|| {
            let mut engine = Self::open_redb(path.clone())?;
            let mut response = QueryResponse::ok_message("backup restored");
            engine.commit_candidate(database, None, &mut response)?;
            Ok(engine)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }

    pub fn check_schema(source: &str) -> Result<crate::schema::SchemaCheck> {
        crate::schema::check(source)
    }

    pub fn diff_schema(
        &self,
        target_source: &str,
        migration_id: &str,
        parent: Option<&str>,
    ) -> Result<crate::schema::SchemaDiff> {
        crate::schema::diff(&self.db, target_source, migration_id, parent)
    }

    fn with_schema(&self, mut response: QueryResponse) -> QueryResponse {
        response.schema = Some(self.db.schema_info());
        response
    }
}

fn ensure_deadline(deadline: Option<std::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(Error::new(
            "E_TIMEOUT",
            "request execution deadline exceeded",
        ))
    } else {
        Ok(())
    }
}

fn migration_file_error(file: &MigrationFile, error: Error) -> Error {
    let location = file
        .path
        .as_ref()
        .map_or_else(|| file.id.clone(), |path| path.display().to_string());
    Error::new(
        &error.code,
        format!(
            "migration '{}', file '{location}': {}",
            file.id, error.message
        ),
    )
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
        fn commit(&mut self, _: &Database, _: &Database) -> std::result::Result<(), CommitFailure> {
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
