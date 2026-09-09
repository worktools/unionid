use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::control::ExecutionControl;
use crate::db::{Database, DurableCatalogEntry, LogicalWriteSet, QueryResponse, QueryRowSink};
use crate::error::{Error, Result};
use crate::idempotency::{
    IdempotencyDurability, IdempotencyPruneOptions, IdempotencyPruneResult, IdempotencyReceipt,
    IdempotencyStatus, IdempotentExecution, MAX_IDEMPOTENCY_PRUNE_RECEIPTS,
    MAX_IDEMPOTENCY_RECEIPTS, MAX_IDEMPOTENCY_TOTAL_BYTES, ReceiptMap, boundary,
    receipt_encoded_len, validate_digest, validate_key, validate_new_receipt, validate_receipts,
};
use crate::introspection::{Introspection, StorageMode, StorageVersions};
use crate::migration::{
    MigrationAbort, MigrationApply, MigrationEntry, MigrationFile, MigrationMaintenance,
    MigrationMaintenancePhase, MigrationPlan, MigrationPlanItem, MigrationStatus, describe_step,
    validate_files_against_history,
};
use crate::profile::{DurableCommitProfile, StorageCheckProfile, StorageOpenProfile};
use crate::query::{LocatedStatement, PageSpec, Stage, Statement};
use crate::redb_storage::{CommitFailure, MaintenanceInfo, MaintenanceState, RedbStore};
use crate::row_source::TypedRowSource;
use crate::snapshot::SnapshotStore;
use crate::syntax;
use crate::wal::Wal;

/// The shared execution boundary. A source request is one atomic batch.
/// Writes stage a private candidate while read snapshots share the last
/// complete committed state through immutable `Arc` references.
#[derive(Default)]
pub struct Engine {
    committed: Arc<CommittedView>,
    wal: Option<Wal>,
    snapshot: Option<SnapshotStore>,
    snapshot_every: usize,
    writes_since_snapshot: usize,
    write_failed: bool,
    read_reopen_required: bool,
    read_only: bool,
    snapshot_execution: bool,
    durable: Option<Box<dyn DurableBackend>>,
    storage_mode: StorageMode,
    snapshot_storage_versions: Option<StorageVersions>,
    last_mutation_profile: Option<MutationProfile>,
    open_profile: Option<StorageOpenProfile>,
    _locks: Vec<DatabaseLock>,
}

#[derive(Clone)]
struct CommittedView {
    db: Arc<Database>,
    source: Arc<dyn TypedRowSource>,
    receipts: Arc<ReceiptMap>,
}

impl Default for CommittedView {
    fn default() -> Self {
        Self::memory(Database::default(), ReceiptMap::new())
    }
}

impl CommittedView {
    fn memory(db: Database, receipts: ReceiptMap) -> Self {
        let db = Arc::new(db);
        let source: Arc<dyn TypedRowSource> = db.clone();
        Self {
            db,
            source,
            receipts: Arc::new(receipts),
        }
    }

    fn new(
        db: Arc<Database>,
        source: Arc<dyn TypedRowSource>,
        receipts: Arc<ReceiptMap>,
    ) -> Result<Self> {
        let identity = source.snapshot_identity();
        let schema = db.schema_info();
        if identity.database_instance != db.durable_meta().cursor_instance_id
            || identity.sequence != db.sequence
            || identity.schema_hash != schema.hash
        {
            return Err(Error::new(
                "E_STORAGE",
                "committed row source identity does not match its catalog snapshot",
            ));
        }
        Ok(Self {
            db,
            source,
            receipts,
        })
    }
}

struct DatabaseLock(File);

impl Drop for DatabaseLock {
    fn drop(&mut self) {
        // Closing a file releases its advisory lock, but explicitly unlocking
        // avoids a transient reacquisition failure observed on macOS when a
        // database is reopened immediately after dropping its Engine.
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageIntegrity {
    pub backend: &'static str,
    pub backend_clean: bool,
    pub schema: crate::db::SchemaInfo,
    pub versions: StorageVersions,
    pub profile: StorageCheckProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StorageUpgrade {
    pub previous_format: u32,
    pub format: u32,
    pub changed: bool,
}

/// Phase timings for the last successful mutating request.
///
/// This observation is intended for diagnostics and workload evaluation. It
/// contains no source text, parameter values, row values, or idempotency keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MutationProfile {
    pub candidate_micros: u64,
    pub durable_commit_micros: u64,
    pub full_rebuild: bool,
    pub touched_tables: usize,
    pub row_inserts: usize,
    pub row_updates: usize,
    pub row_deletes: usize,
    pub index_inserts: usize,
    pub index_deletes: usize,
    pub receipt_changes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable: Option<DurableCommitProfile>,
}

impl MutationProfile {
    fn from_write_set(started: std::time::Instant, write_set: Option<&LogicalWriteSet>) -> Self {
        let summary = write_set.map(LogicalWriteSet::summary).unwrap_or_default();
        Self {
            candidate_micros: elapsed_micros(started),
            durable_commit_micros: 0,
            full_rebuild: write_set.is_none(),
            touched_tables: summary.touched_tables,
            row_inserts: summary.row_inserts,
            row_updates: summary.row_updates,
            row_deletes: summary.row_deletes,
            index_inserts: summary.index_inserts,
            index_deletes: summary.index_deletes,
            receipt_changes: summary.receipt_changes,
            durable: None,
        }
    }
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
        previous_receipts: &ReceiptMap,
        database: &Database,
        receipts: &ReceiptMap,
        write_set: Option<&LogicalWriteSet>,
    ) -> std::result::Result<DurableCommitProfile, CommitFailure>;
    fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap, StorageCheckProfile)>;
    fn supports_production_scalars(&self) -> bool;
    fn supports_bounded_row_mutation(&self) -> bool {
        false
    }
    fn versions(&self) -> StorageVersions;
    fn committed_view(
        &self,
        database: &Database,
    ) -> Result<(Arc<Database>, Arc<dyn TypedRowSource>)> {
        let database = Arc::new(database.clone());
        let source: Arc<dyn TypedRowSource> = database.clone();
        Ok((database, source))
    }
    fn upgrade(
        &mut self,
        database: &Database,
        receipts: &ReceiptMap,
        target: u32,
    ) -> std::result::Result<StorageUpgrade, CommitFailure>;
    fn maintenance_info(&self) -> Result<Option<MaintenanceInfo>> {
        Ok(None)
    }
    fn start_maintenance(
        &mut self,
        _source: &Database,
        _target: &Database,
        _file: &MigrationFile,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        Err(CommitFailure::Definite(Error::new(
            "E_CONFIG",
            "durable backend does not support recoverable migrations",
        )))
    }
    fn append_maintenance_batch(
        &mut self,
        _file: &MigrationFile,
        _target_batch: &Database,
        _checkpoint: (u64, crate::RowId),
        _source_rows: usize,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        Err(CommitFailure::Definite(Error::new(
            "E_CONFIG",
            "durable backend does not support recoverable migrations",
        )))
    }
    fn mark_maintenance_ready(
        &mut self,
        _file: &MigrationFile,
        _target: &Database,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        Err(CommitFailure::Definite(Error::new(
            "E_CONFIG",
            "durable backend does not support recoverable migrations",
        )))
    }
    fn cutover_maintenance(
        &mut self,
        _file: &MigrationFile,
        _target: &Database,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        Err(CommitFailure::Definite(Error::new(
            "E_CONFIG",
            "durable backend does not support recoverable migrations",
        )))
    }
    fn begin_maintenance_abort(
        &mut self,
    ) -> std::result::Result<Option<MaintenanceInfo>, CommitFailure> {
        Ok(None)
    }
    fn reclaim_maintenance_step(&mut self) -> std::result::Result<bool, CommitFailure> {
        Ok(true)
    }
}

impl DurableBackend for RedbStore {
    fn commit(
        &mut self,
        previous: &Database,
        previous_receipts: &ReceiptMap,
        database: &Database,
        receipts: &ReceiptMap,
        write_set: Option<&LogicalWriteSet>,
    ) -> std::result::Result<DurableCommitProfile, CommitFailure> {
        match write_set {
            Some(write_set) => RedbStore::commit_incremental(
                self,
                previous,
                previous_receipts,
                database,
                receipts,
                write_set,
            ),
            None => RedbStore::commit(self, previous, database, receipts),
        }
    }

    fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap, StorageCheckProfile)> {
        RedbStore::check_integrity(self)
    }

    fn supports_production_scalars(&self) -> bool {
        RedbStore::supports_production_scalars(self)
    }

    fn supports_bounded_row_mutation(&self) -> bool {
        RedbStore::supports_bounded_row_mutation(self)
    }

    fn versions(&self) -> StorageVersions {
        RedbStore::versions(self)
    }

    fn committed_view(
        &self,
        database: &Database,
    ) -> Result<(Arc<Database>, Arc<dyn TypedRowSource>)> {
        RedbStore::committed_view(self, database)
    }

    fn upgrade(
        &mut self,
        database: &Database,
        receipts: &ReceiptMap,
        target: u32,
    ) -> std::result::Result<StorageUpgrade, CommitFailure> {
        let result = RedbStore::upgrade(self, database, receipts, target)?;
        Ok(StorageUpgrade {
            previous_format: result.previous_format,
            format: result.format,
            changed: result.changed,
        })
    }

    fn maintenance_info(&self) -> Result<Option<MaintenanceInfo>> {
        RedbStore::maintenance_info(self)
    }

    fn start_maintenance(
        &mut self,
        source: &Database,
        target: &Database,
        file: &MigrationFile,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        RedbStore::start_maintenance(self, source, target, file)
    }

    fn append_maintenance_batch(
        &mut self,
        file: &MigrationFile,
        target_batch: &Database,
        checkpoint: (u64, crate::RowId),
        source_rows: usize,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        RedbStore::append_maintenance_batch(self, file, target_batch, checkpoint, source_rows)
    }

    fn mark_maintenance_ready(
        &mut self,
        file: &MigrationFile,
        target: &Database,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        RedbStore::mark_maintenance_ready(self, file, target)
    }

    fn cutover_maintenance(
        &mut self,
        file: &MigrationFile,
        target: &Database,
    ) -> std::result::Result<MaintenanceInfo, CommitFailure> {
        RedbStore::cutover_maintenance(self, file, target)
    }

    fn begin_maintenance_abort(
        &mut self,
    ) -> std::result::Result<Option<MaintenanceInfo>, CommitFailure> {
        RedbStore::begin_maintenance_abort(self)
    }

    fn reclaim_maintenance_step(&mut self) -> std::result::Result<bool, CommitFailure> {
        RedbStore::reclaim_maintenance_step(self)
    }
}

#[derive(Clone, Copy)]
struct PendingIdempotency<'a> {
    key: &'a str,
    digest: &'a str,
}

impl Engine {
    pub fn current_storage_versions() -> StorageVersions {
        crate::redb_storage::production_versions()
    }

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
            locks.push(DatabaseLock(lock));
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
            committed: Arc::new(CommittedView::memory(db, ReceiptMap::new())),
            wal,
            snapshot,
            snapshot_every,
            writes_since_snapshot: 0,
            write_failed: false,
            read_reopen_required: false,
            read_only: false,
            snapshot_execution: false,
            durable: None,
            storage_mode,
            snapshot_storage_versions: None,
            last_mutation_profile: None,
            open_profile: None,
            _locks: locks,
        })
    }

    /// Open the durable redb backend. Every mutating source request is
    /// committed as one synchronous, two-phase redb transaction.
    pub fn open_redb(path: impl Into<PathBuf>) -> Result<Self> {
        let (redb, db, receipts, source, open_profile) = RedbStore::open(path)?;
        Ok(Self {
            committed: Arc::new(CommittedView::new(db, source, Arc::new(receipts))?),
            durable: Some(Box::new(redb)),
            storage_mode: StorageMode::Redb,
            open_profile: Some(open_profile),
            ..Self::default()
        })
    }

    /// Open an existing durable redb database behind a read-only execution
    /// boundary. A missing path is rejected instead of creating an empty
    /// database that could hide a deployment mistake.
    pub fn open_redb_read_only(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.is_file() {
            return Err(Error::new(
                "E_CONFIG",
                format!(
                    "read-only database '{}' does not exist or is not a file",
                    path.display()
                ),
            ));
        }
        Self::open_redb(path).map(|engine| engine.with_read_only(true))
    }

    /// Configure this Engine as a read-only execution boundary.
    ///
    /// Every mutating script or pending migration is rejected after validation
    /// and before candidate state or a durable transaction is created.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        if let Some(profile) = &mut self.open_profile {
            profile.read_only = read_only;
        }
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn last_mutation_profile(&self) -> Option<MutationProfile> {
        self.last_mutation_profile
    }

    /// Return the value-free phase profile captured by the successful durable
    /// open that created this Engine. Memory and legacy WAL engines return
    /// `None`.
    pub fn open_profile(&self) -> Option<StorageOpenProfile> {
        self.open_profile
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

    pub fn execute_page(&mut self, source: &str, page: PageSpec) -> QueryResponse {
        self.execute_with_params_page(source, std::collections::BTreeMap::new(), None, page)
    }

    pub fn execute_with_params_page(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        page: PageSpec,
    ) -> QueryResponse {
        match self.try_execute_with_params_and_idempotency(
            source,
            parameters,
            expected_schema,
            None,
            None,
            Some(page),
        ) {
            Ok(response) => response,
            Err(error) => self.with_schema(QueryResponse::failure(error)),
        }
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
        let control = ExecutionControl::deadline(deadline);
        self.execute_with_params_at_schema_and_deadline(
            source,
            parameters,
            expected_schema,
            Some(&control),
        )
    }

    pub(crate) fn execute_read_statements_controlled(
        &mut self,
        mut statements: Vec<LocatedStatement>,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        control: &ExecutionControl,
    ) -> QueryResponse {
        self.last_mutation_profile = None;
        let result = (|| {
            control.checkpoint()?;
            if let Some(expected) = expected_schema
                && expected != &self.committed.db.schema_info()
            {
                return Err(Error::new(
                    "E_SCHEMA_CHANGED",
                    format!(
                        "request expects schema revision {} ({}) but the database is at revision {} ({})",
                        expected.revision,
                        expected.hash,
                        self.committed.db.schema_info().revision,
                        self.committed.db.schema_info().hash
                    ),
                ));
            }
            crate::params::bind(&mut statements, &parameters)?;
            control.checkpoint()?;
            if statements
                .iter()
                .any(|statement| statement.statement.is_mutating())
            {
                return Err(Error::new(
                    "E_STREAM_SHAPE",
                    "registered read contains a mutating statement",
                ));
            }
            self.try_execute_statements(statements, None, Some(control), None)
        })();
        match result {
            Ok(response) => response,
            Err(error) => self.with_schema(QueryResponse::failure(error)),
        }
    }

    pub(crate) fn execute_read_stream_controlled(
        &mut self,
        mut statements: Vec<LocatedStatement>,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        control: &ExecutionControl,
        sink: &mut dyn QueryRowSink,
    ) -> Result<QueryResponse> {
        self.last_mutation_profile = None;
        control.checkpoint()?;
        if self.read_reopen_required {
            return Err(Error::new(
                "E_STORAGE_REOPEN_REQUIRED",
                "the durable effect was committed but no matching read view is available; reopen the database before reading",
            ));
        }
        if let Some(expected) = expected_schema
            && expected != &self.committed.db.schema_info()
        {
            return Err(Error::new(
                "E_SCHEMA_CHANGED",
                format!(
                    "request expects schema revision {} ({}) but the database is at revision {} ({})",
                    expected.revision,
                    expected.hash,
                    self.committed.db.schema_info().revision,
                    self.committed.db.schema_info().hash
                ),
            ));
        }
        crate::params::bind(&mut statements, &parameters)?;
        control.checkpoint()?;
        if statements.len() != 1 || statements[0].statement.is_mutating() {
            return Err(Error::new(
                "E_STREAM_SHAPE",
                "streaming requires exactly one read query",
            ));
        }
        let located = statements.remove(0);
        self.committed
            .db
            .execute_read_stream_from(
                self.committed.source.as_ref(),
                located.statement,
                Some(control),
                sink,
            )
            .map_err(|error| error.at(located.span))
    }

    /// Execute a structured bounded-page request through the same pipeline
    /// stage used by the source language.
    pub fn execute_with_params_page_until(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        page: PageSpec,
        deadline: std::time::Instant,
    ) -> QueryResponse {
        let control = ExecutionControl::deadline(deadline);
        match self.try_execute_with_params_and_idempotency(
            source,
            parameters,
            expected_schema,
            Some(&control),
            None,
            Some(page),
        ) {
            Ok(response) => response,
            Err(error) => self.with_schema(QueryResponse::failure(error)),
        }
    }

    /// Execute one mutation with an already-computed canonical request digest.
    /// Protocol adapters are responsible for constructing that digest from the
    /// exact source, wire parameters, and schema precondition.
    pub fn execute_idempotent_with_params(
        &mut self,
        key: &str,
        digest: &str,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
    ) -> Result<IdempotentExecution> {
        self.execute_idempotent_with_deadline(
            key,
            digest,
            source,
            parameters,
            expected_schema,
            None,
        )
    }

    pub fn execute_idempotent_with_params_until(
        &mut self,
        key: &str,
        digest: &str,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: std::time::Instant,
    ) -> Result<IdempotentExecution> {
        let control = ExecutionControl::deadline(deadline);
        self.execute_idempotent_with_deadline(
            key,
            digest,
            source,
            parameters,
            expected_schema,
            Some(&control),
        )
    }

    fn execute_idempotent_with_deadline(
        &mut self,
        key: &str,
        digest: &str,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<&ExecutionControl>,
    ) -> Result<IdempotentExecution> {
        self.last_mutation_profile = None;
        validate_key(key)?;
        validate_digest(digest)?;
        let durability = if self.durable.is_some() {
            IdempotencyDurability::Durable
        } else {
            IdempotencyDurability::ProcessLocal
        };
        if let Some(receipt) = self.committed.receipts.get(key) {
            if receipt.digest != digest {
                return Err(Error::new(
                    "E_IDEMPOTENCY_CONFLICT",
                    format!(
                        "idempotency key has digest {}; new request has digest {digest}",
                        receipt.digest
                    ),
                ));
            }
            return Ok(IdempotentExecution {
                response: receipt.response.clone(),
                replayed: true,
                digest: receipt.digest.clone(),
                committed_sequence: receipt.committed_sequence,
                durability,
            });
        }
        if self.wal.is_some() {
            return Err(Error::new(
                "E_CONFIG",
                "idempotent writes require redb or memory mode; the transitional WAL cannot store receipts",
            ));
        }
        let response = self.try_execute_with_params_and_idempotency(
            source,
            parameters,
            expected_schema,
            deadline,
            Some(PendingIdempotency { key, digest }),
            None,
        )?;
        let receipt = self
            .committed
            .receipts
            .get(key)
            .expect("successful idempotent execution publishes its receipt");
        Ok(IdempotentExecution {
            response,
            replayed: false,
            digest: receipt.digest.clone(),
            committed_sequence: receipt.committed_sequence,
            durability,
        })
    }

    pub fn idempotency_status(&self) -> Result<IdempotencyStatus> {
        let encoded_bytes =
            validate_receipts(&self.committed.receipts, self.committed.db.sequence)?;
        let mut ordered = self.committed.receipts.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(key, receipt)| {
            (
                receipt.committed_sequence,
                receipt.completed_at_unix_ms,
                key.as_str(),
            )
        });
        Ok(IdempotencyStatus {
            count: self.committed.receipts.len(),
            encoded_bytes,
            max_count: MAX_IDEMPOTENCY_RECEIPTS,
            max_encoded_bytes: MAX_IDEMPOTENCY_TOTAL_BYTES,
            oldest: ordered.first().map(|(key, receipt)| boundary(key, receipt)),
            newest: ordered.last().map(|(key, receipt)| boundary(key, receipt)),
            durability: self.idempotency_durability(),
        })
    }

    pub fn plan_idempotency_prune(
        &self,
        options: IdempotencyPruneOptions,
    ) -> Result<IdempotencyPruneResult> {
        let selected = self.idempotency_prune_selection(&options)?;
        let selected_encoded_bytes = selected.iter().try_fold(0usize, |total, key| {
            total
                .checked_add(receipt_encoded_len(&self.committed.receipts[*key])?)
                .ok_or_else(|| Error::new("E_IDEMPOTENCY_CAPACITY", "receipt size overflow"))
        })?;
        Ok(IdempotencyPruneResult {
            options,
            selected_count: selected.len(),
            selected_encoded_bytes,
            remaining_count: self.committed.receipts.len().saturating_sub(selected.len()),
            applied: false,
            first_selected: selected
                .first()
                .map(|key| boundary(key, &self.committed.receipts[*key])),
            last_selected: selected
                .last()
                .map(|key| boundary(key, &self.committed.receipts[*key])),
        })
    }

    pub fn prune_idempotency_receipts(
        &mut self,
        options: IdempotencyPruneOptions,
    ) -> Result<IdempotencyPruneResult> {
        if self.read_only {
            return Err(Error::new(
                "E_READ_ONLY",
                "idempotency receipts cannot be pruned through a read-only Engine",
            ));
        }
        if self.wal.is_some() {
            return Err(Error::new(
                "E_CONFIG",
                "idempotency receipt pruning requires redb or memory mode",
            ));
        }
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "receipt pruning is disabled after a storage failure; reopen the database",
            ));
        }
        if self.unfinished_maintenance()? {
            return Err(Error::new(
                "E_MAINTENANCE_REQUIRED",
                "receipt pruning is blocked while a migration generation is unfinished",
            ));
        }
        let keys = self
            .idempotency_prune_selection(&options)?
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut result = self.plan_idempotency_prune(options)?;
        if keys.is_empty() {
            result.applied = true;
            return Ok(result);
        }
        let mut receipts = self.committed.receipts.as_ref().clone();
        for key in keys {
            receipts.remove(&key);
        }
        let mut candidate = self.mutable_candidate(None)?;
        candidate.sequence = self
            .committed
            .db
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
        let mut response = QueryResponse::ok_message("idempotency receipts pruned");
        self.commit_candidate(candidate, None, &mut response, Some(receipts), None)?;
        result.applied = true;
        Ok(result)
    }

    fn idempotency_prune_selection(
        &self,
        options: &IdempotencyPruneOptions,
    ) -> Result<Vec<&String>> {
        if options.completed_before_unix_ms.is_none()
            && options.committed_through_sequence.is_none()
        {
            return Err(Error::new(
                "E_IDEMPOTENCY_PRUNE",
                "receipt pruning requires a time or commit-sequence cutoff",
            ));
        }
        if options.max_receipts == 0 || options.max_receipts > MAX_IDEMPOTENCY_PRUNE_RECEIPTS {
            return Err(Error::new(
                "E_IDEMPOTENCY_PRUNE",
                format!("max_receipts must be between 1 and {MAX_IDEMPOTENCY_PRUNE_RECEIPTS}"),
            ));
        }
        let mut selected = self
            .committed
            .receipts
            .iter()
            .filter(|(_, receipt)| {
                options
                    .completed_before_unix_ms
                    .is_none_or(|cutoff| receipt.completed_at_unix_ms < cutoff)
                    && options
                        .committed_through_sequence
                        .is_none_or(|cutoff| receipt.committed_sequence <= cutoff)
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|(key, receipt)| {
            (
                receipt.committed_sequence,
                receipt.completed_at_unix_ms,
                key.as_str(),
            )
        });
        selected.truncate(options.max_receipts);
        Ok(selected.into_iter().map(|(key, _)| key).collect())
    }

    fn idempotency_durability(&self) -> IdempotencyDurability {
        if self.durable.is_some() {
            IdempotencyDurability::Durable
        } else {
            IdempotencyDurability::ProcessLocal
        }
    }

    fn execute_with_params_at_schema_and_deadline(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<&ExecutionControl>,
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
                    .committed
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
                        self.committed
                            .db
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
                        self.committed
                            .db
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
                        self.committed
                            .db
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
                        self.committed
                            .db
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
                    self.committed
                        .db
                        .prepare_update(target, assignments, returning.as_ref())
                        .map_err(|error| error.at(located.span))?;
                    mutating = true;
                }
                Statement::Delete { target, returning } => {
                    self.committed
                        .db
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
            .map(|(name, ty)| (name, self.committed.db.catalog.describe(&ty)))
            .collect();
        Ok(PreparedQuery {
            source: source.into(),
            parameters: crate::params::names(&statements).into_iter().collect(),
            parameter_types,
            statements,
            schema: self.committed.db.schema_info(),
            mutating,
        })
    }

    /// Validate the complete response type boundary used by protocol version 1.
    /// Binding runs against a database clone when earlier schema statements must
    /// affect the final statement; no live row or catalog mutation is published.
    pub(crate) fn preflight_protocol_v1(
        &self,
        source: &str,
        parameters: &std::collections::BTreeMap<String, crate::Value>,
        page: Option<PageSpec>,
    ) -> Result<()> {
        let mut statements = syntax::parse(source)?;
        if let Some(page) = page {
            attach_structured_page(&mut statements, page)?;
        }
        crate::params::bind(&mut statements, parameters)?;
        let Some(last) = statements.len().checked_sub(1) else {
            return Ok(());
        };
        let needs_schema_preview = statements[..last]
            .iter()
            .any(|located| located.statement.changes_schema());
        let mut preview = if needs_schema_preview {
            self.mutable_candidate(None)?
        } else {
            self.committed.db.as_ref().clone()
        };
        if needs_schema_preview {
            for located in &statements[..last] {
                preview
                    .execute(located.statement.clone())
                    .map_err(|error| error.at(located.span))?;
            }
        }
        let types = preview
            .prepare_response_types(&mut statements[last].statement)
            .map_err(|error| error.at(statements[last].span))?;
        for ty in types {
            if preview.catalog.requires_protocol_v2(&ty)? {
                return Err(Error::new(
                    "E_PROTOCOL_TYPE",
                    "response type requires protocol version 2",
                )
                .at(statements[last].span));
            }
        }
        Ok(())
    }

    pub(crate) fn preflight_protocol_v1_introspection(&self) -> Result<()> {
        if self.committed.db.has_production_scalars()? {
            Err(Error::new(
                "E_PROTOCOL_TYPE",
                "schema introspection requires protocol version 2",
            ))
        } else {
            Ok(())
        }
    }

    /// Return true when the idempotent request will replay without parsing its
    /// query. This preserves the established retry contract while still
    /// preventing a legacy protocol response from exposing a native scalar.
    pub(crate) fn preflight_protocol_v1_receipt(&self, key: &str, digest: &str) -> Result<bool> {
        let Some(receipt) = self.committed.receipts.get(key) else {
            return Ok(false);
        };
        if receipt.digest != digest {
            return Err(Error::new(
                "E_IDEMPOTENCY_CONFLICT",
                format!(
                    "idempotency key has digest {}; new request has digest {digest}",
                    receipt.digest
                ),
            ));
        }
        if receipt
            .response
            .rows
            .iter()
            .any(|row| row.values().any(crate::Value::requires_protocol_v2))
        {
            return Err(Error::new(
                "E_PROTOCOL_TYPE",
                "stored response requires protocol version 2",
            ));
        }
        Ok(true)
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
        deadline: Option<&ExecutionControl>,
    ) -> QueryResponse {
        self.last_mutation_profile = None;
        if prepared.schema != self.committed.db.schema_info() {
            return self.with_schema(QueryResponse::failure(Error::new(
                "E_SCHEMA_CHANGED",
                format!(
                    "prepared query uses schema revision {} ({}) but the database is at revision {} ({})",
                    prepared.schema.revision,
                    prepared.schema.hash,
                    self.committed.db.schema_info().revision,
                    self.committed.db.schema_info().hash
                ),
            )));
        }
        if prepared.mutating && self.wal.is_some() {
            return self.with_schema(QueryResponse::failure(Error::new(
                "E_CONFIG",
                "parameterized writes require redb or memory mode; the transitional WAL stores source text",
            )));
        }
        if prepared.mutating
            && parameters.values().any(crate::Value::requires_protocol_v2)
            && (self
                .durable
                .as_ref()
                .is_some_and(|durable| !durable.supports_production_scalars())
                || self.snapshot.is_some())
        {
            return self.with_schema(QueryResponse::failure(Error::new(
                "E_STORAGE_UPGRADE_REQUIRED",
                "production scalar writes require storage format 4",
            )));
        }
        let mut statements = prepared.statements.clone();
        let result = crate::params::bind(&mut statements, &parameters)
            .and_then(|()| self.try_execute_statements(statements, None, deadline, None));
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
        let control = ExecutionControl::deadline(deadline);
        self.execute_prepared_with_deadline(prepared, parameters, Some(&control))
    }

    fn try_execute_with_params(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<&ExecutionControl>,
    ) -> Result<QueryResponse> {
        self.try_execute_with_params_and_idempotency(
            source,
            parameters,
            expected_schema,
            deadline,
            None,
            None,
        )
    }

    fn try_execute_with_params_and_idempotency(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<&ExecutionControl>,
        idempotency: Option<PendingIdempotency<'_>>,
        page: Option<PageSpec>,
    ) -> Result<QueryResponse> {
        self.last_mutation_profile = None;
        if let Some(expected) = expected_schema
            && expected != &self.committed.db.schema_info()
        {
            return Err(Error::new(
                "E_SCHEMA_CHANGED",
                format!(
                    "request expects schema revision {} ({}) but the database is at revision {} ({})",
                    expected.revision,
                    expected.hash,
                    self.committed.db.schema_info().revision,
                    self.committed.db.schema_info().hash
                ),
            ));
        }
        let mut statements = syntax::parse(source)?;
        if let Some(page) = page {
            attach_structured_page(&mut statements, page)?;
        }
        let native_parameters = parameters.values().any(crate::Value::requires_protocol_v2);
        if native_parameters
            && (self
                .durable
                .as_ref()
                .is_some_and(|durable| !durable.supports_production_scalars())
                || self.wal.is_some()
                || self.snapshot.is_some())
            && statements
                .iter()
                .any(|located| located.statement.is_mutating())
        {
            return Err(Error::new(
                "E_STORAGE_UPGRADE_REQUIRED",
                "production scalar writes require storage format 4",
            ));
        }
        crate::params::bind(&mut statements, &parameters)?;
        let contains_parameters = !parameters.is_empty();
        let mutating = statements
            .iter()
            .any(|statement| statement.statement.is_mutating());
        let contains_page = statements.iter().any(|located| {
            matches!(
                &located.statement,
                Statement::Pipeline(pipeline) | Statement::Explain(pipeline)
                    if pipeline.stages.iter().any(|stage| matches!(stage, Stage::Page(_)))
            )
        });
        if contains_page && (statements.len() != 1 || mutating) {
            return Err(Error::new(
                "E_PAGE_SHAPE",
                "page requires exactly one read pipeline or explain statement",
            ));
        }
        if idempotency.is_some() && !mutating {
            return Err(Error::new(
                "E_IDEMPOTENCY_NOT_MUTATION",
                "idempotency keys are only valid for scripts containing a mutation",
            ));
        }
        if contains_parameters && mutating && self.wal.is_some() {
            return Err(Error::new(
                "E_CONFIG",
                "parameterized writes require redb or memory mode; the transitional WAL stores source text",
            ));
        }
        self.try_execute_statements(statements, Some(source), deadline, idempotency)
    }

    fn try_execute_statements(
        &mut self,
        statements: Vec<LocatedStatement>,
        wal_source: Option<&str>,
        deadline: Option<&ExecutionControl>,
        idempotency: Option<PendingIdempotency<'_>>,
    ) -> Result<QueryResponse> {
        let mutating = statements.iter().any(|s| s.statement.is_mutating());
        let schema_changing = statements.iter().any(|s| s.statement.changes_schema());
        if self.read_reopen_required {
            return Err(Error::new(
                "E_STORAGE_REOPEN_REQUIRED",
                "the durable effect was committed but no matching read view is available; reopen the database before reading or retrying",
            ));
        }
        if mutating && self.read_only {
            return Err(Error::new(
                "E_READ_ONLY",
                "mutating scripts are disabled by the read-only execution boundary",
            ));
        }
        if mutating && self.snapshot_execution {
            return Err(Error::new(
                "E_READ_SNAPSHOT",
                "mutating scripts cannot execute against an immutable read snapshot",
            ));
        }
        if schema_changing && !self.committed.db.migration_history().is_empty() {
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
        if mutating && self.unfinished_maintenance()? {
            return Err(Error::new(
                "E_MAINTENANCE_REQUIRED",
                "ordinary writes are blocked while a migration generation is unfinished; resume or abort the migration",
            ));
        }
        let bounded_mutation_source = (self.storage_mode == StorageMode::Redb
            && self
                .durable
                .as_ref()
                .is_some_and(|durable| durable.supports_bounded_row_mutation())
            && statements.len() == 1
            && !schema_changing
            && matches!(
                statements[0].statement,
                Statement::Insert { .. }
                    | Statement::InsertMany { .. }
                    | Statement::Upsert { .. }
                    | Statement::UpsertMany { .. }
                    | Statement::Update { .. }
                    | Statement::Delete { .. }
            ))
        .then(|| self.committed.source.clone());
        let candidate_started = mutating.then(std::time::Instant::now);
        let mut candidate = if mutating {
            Some(if bounded_mutation_source.is_some() {
                self.committed.db.metadata_only()?
            } else {
                self.mutable_candidate(deadline)?
            })
        } else {
            None
        };
        let mut response = QueryResponse::ok_message("ok");
        for located in statements {
            ensure_deadline(deadline)?;
            response = match candidate.as_mut() {
                Some(target) => match bounded_mutation_source.as_deref() {
                    Some(source) => {
                        target.execute_bounded_mutation_from(source, located.statement, deadline)
                    }
                    None => target.execute_with_deadline(located.statement, deadline),
                },
                None => self.committed.db.execute_read_from(
                    self.committed.source.as_ref(),
                    located.statement,
                    deadline,
                ),
            }
            .map_err(|e| e.at(located.span))?;
        }
        ensure_deadline(deadline)?;
        if let Some(mut candidate) = candidate {
            if schema_changing {
                candidate.advance_schema_revision()?;
            }
            candidate.sequence = self
                .committed
                .db
                .sequence
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
            let mut write_set = candidate.take_write_set();
            let receipt_state = if let Some(idempotency) = idempotency {
                response.schema = Some(candidate.schema_info());
                let receipt = IdempotencyReceipt {
                    digest: idempotency.digest.to_owned(),
                    committed_sequence: candidate.sequence,
                    completed_at_unix_ms: unix_time_ms()?,
                    response: response.clone(),
                };
                validate_new_receipt(&self.committed.receipts, &receipt)?;
                let mut receipts = self.committed.receipts.as_ref().clone();
                receipts.insert(idempotency.key.to_owned(), receipt);
                write_set.record_receipt(idempotency.key);
                Some(receipts)
            } else {
                None
            };
            let write_set = (!schema_changing).then_some(write_set);
            let mut profile = MutationProfile::from_write_set(
                candidate_started.expect("mutating scripts start candidate timing"),
                write_set.as_ref(),
            );
            let (durable_commit_micros, durable_profile) = self.commit_candidate(
                candidate,
                wal_source,
                &mut response,
                receipt_state,
                write_set,
            )?;
            profile.durable_commit_micros = durable_commit_micros;
            profile.durable = durable_profile;
            self.last_mutation_profile = Some(profile);
        }
        Ok(self.with_schema(response))
    }

    pub fn migration_status(&self, files: &[MigrationFile]) -> Result<MigrationStatus> {
        let applied_count =
            validate_files_against_history(files, self.committed.db.migration_history())?;
        let maintenance = self
            .durable
            .as_ref()
            .map(|durable| durable.maintenance_info())
            .transpose()?
            .flatten()
            .map(migration_maintenance);
        Ok(MigrationStatus {
            schema: self.committed.db.schema_info(),
            applied: self.committed.db.migration_history().to_vec(),
            pending: files[applied_count..]
                .iter()
                .map(|file| file.id.clone())
                .collect(),
            maintenance,
        })
    }

    fn unfinished_maintenance(&self) -> Result<bool> {
        Ok(self
            .durable
            .as_ref()
            .map(|durable| durable.maintenance_info())
            .transpose()?
            .flatten()
            .is_some_and(|info| {
                matches!(
                    info.state,
                    MaintenanceState::Building
                        | MaintenanceState::Ready
                        | MaintenanceState::Aborting
                )
            }))
    }

    pub fn abort_migration(&mut self) -> Result<MigrationAbort> {
        if self.read_only {
            return Err(Error::new(
                "E_READ_ONLY",
                "migration abort is a maintenance mutation",
            ));
        }
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "migration abort requires reopening after an uncertain commit",
            ));
        }
        let existing = self
            .durable
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    "E_CONFIG",
                    "recoverable migration abort requires a redb database",
                )
            })?
            .maintenance_info()?;
        let migration_id = existing.as_ref().map(|info| info.migration_id.clone());
        if !existing
            .as_ref()
            .is_some_and(|info| info.state == MaintenanceState::Reclaimable)
        {
            let result = self
                .durable
                .as_mut()
                .expect("durable backend was checked")
                .begin_maintenance_abort();
            self.finish_maintenance_result(result)?;
        }
        loop {
            let result = self
                .durable
                .as_mut()
                .expect("maintenance abort keeps the durable backend")
                .reclaim_maintenance_step();
            if self.finish_maintenance_result(result)? {
                break;
            }
        }
        Ok(MigrationAbort {
            migration_id,
            cleaned: true,
            schema: self.committed.db.schema_info(),
        })
    }

    pub fn plan_migrations(&self, files: &[MigrationFile]) -> Result<MigrationPlan> {
        let applied_count =
            validate_files_against_history(files, self.committed.db.migration_history())?;
        let current_schema = self.committed.db.schema_info();
        let mut candidate = if self
            .durable
            .as_ref()
            .is_some_and(|durable| durable.versions().format == 6)
        {
            self.committed.db.metadata_only()?
        } else {
            self.mutable_candidate(None)?
        };
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
        self.apply_migrations_controlled(files, None)
    }

    pub fn apply_migrations_until(
        &mut self,
        files: &[MigrationFile],
        deadline: std::time::Instant,
    ) -> Result<MigrationApply> {
        let control = ExecutionControl::deadline(deadline);
        self.apply_migrations_controlled(files, Some(&control))
    }

    pub(crate) fn apply_migrations_controlled(
        &mut self,
        files: &[MigrationFile],
        control: Option<&ExecutionControl>,
    ) -> Result<MigrationApply> {
        ensure_deadline(control)?;
        if self.wal.is_some() {
            return Err(Error::new(
                "E_CONFIG",
                "versioned migrations require redb or an in-memory Engine",
            ));
        }
        let applied_count =
            validate_files_against_history(files, self.committed.db.migration_history())?;
        if self.read_only && applied_count < files.len() {
            return Err(Error::new(
                "E_READ_ONLY",
                "pending migrations cannot be applied through a read-only Engine",
            ));
        }
        let skipped = files[..applied_count]
            .iter()
            .map(|file| file.id.clone())
            .collect();
        let mut applied = Vec::new();
        for file in &files[applied_count..] {
            ensure_deadline(control)?;
            if let Err(error) = self.apply_migration_file(file, control) {
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
            schema: self.committed.db.schema_info(),
        })
    }

    fn apply_migration_file(
        &mut self,
        file: &MigrationFile,
        control: Option<&ExecutionControl>,
    ) -> Result<()> {
        ensure_deadline(control)?;
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "writes are disabled after a storage failure; reopen the database to resolve the commit state",
            ));
        }
        if self
            .durable
            .as_ref()
            .is_some_and(|durable| durable.versions().format == 6)
        {
            return self.apply_migration_file_shadow(file, control);
        }
        let mut candidate = self.mutable_candidate(None)?;
        candidate
            .execute(Statement::Migration {
                name: file.id.clone(),
                parent: file.parent.clone(),
                steps: file.steps.clone(),
            })
            .map_err(|error| migration_file_error(file, error))?;
        candidate.advance_schema_revision()?;
        ensure_deadline(control)?;
        candidate.sequence = self
            .committed
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
        self.commit_candidate(candidate, None, &mut response, None, None)
            .map(|_| ())
    }

    fn apply_migration_file_shadow(
        &mut self,
        file: &MigrationFile,
        control: Option<&ExecutionControl>,
    ) -> Result<()> {
        ensure_deadline(control)?;
        let source_database = self.committed.db.clone();
        let source = self.committed.source.clone();
        let mut target = source_database
            .migration_target(&file.id, &file.steps)
            .map_err(|error| migration_file_error(file, error))?;
        let schema = target.schema_info();
        let applied_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| Error::new("E_TIME", error.to_string()))?
            .as_millis()
            .try_into()
            .map_err(|_| Error::new("E_LIMIT", "migration timestamp exceeds u64"))?;
        target.append_migration(MigrationEntry {
            id: file.id.clone(),
            parent: file.parent.clone(),
            checksum: file.checksum.clone(),
            schema_revision: schema.revision,
            schema_hash: schema.hash,
            applied_at_unix_ms,
        })?;

        loop {
            ensure_deadline(control)?;
            let info = self
                .durable
                .as_ref()
                .expect("format-6 migration has a durable backend")
                .maintenance_info()?;
            if !info.is_some_and(|info| {
                matches!(
                    info.state,
                    MaintenanceState::Aborting | MaintenanceState::Reclaimable
                )
            }) {
                break;
            }
            let result = self
                .durable
                .as_mut()
                .expect("format-6 migration has a durable backend")
                .reclaim_maintenance_step();
            if self.finish_maintenance_result(result)? {
                break;
            }
        }

        let mut info = self
            .durable
            .as_ref()
            .expect("format-6 migration has a durable backend")
            .maintenance_info()?;
        if info.is_none() {
            let result = self
                .durable
                .as_mut()
                .expect("format-6 migration has a durable backend")
                .start_maintenance(&source_database, &target, file);
            info = Some(self.finish_maintenance_result(result)?);
        }
        let mut info = info.expect("maintenance was started or resumed");
        if info.state == MaintenanceState::Building {
            let mut tables = source_database
                .durable_catalog_entries()
                .into_iter()
                .filter_map(|entry| match entry {
                    DurableCatalogEntry::Table(table) => Some((table.id, table.name)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            tables.sort_by_key(|(id, _)| *id);
            for (table_id, table_name) in tables {
                ensure_deadline(control)?;
                if info
                    .checkpoint
                    .is_some_and(|(checkpoint_table, _)| table_id < checkpoint_table)
                {
                    continue;
                }
                let mut cursor = source.scan_rows(&table_name)?;
                let mut observation = crate::ExecutionObservation::default();
                while let Some(batch) = cursor.next_batch(control, &mut observation)? {
                    let rows = batch
                        .rows
                        .into_iter()
                        .filter(|row| {
                            info.checkpoint
                                .is_none_or(|(checkpoint_table, checkpoint_row)| {
                                    table_id > checkpoint_table
                                        || (table_id == checkpoint_table && row.id > checkpoint_row)
                                })
                        })
                        .collect::<Vec<_>>();
                    if rows.is_empty() {
                        continue;
                    }
                    let checkpoint = (
                        table_id,
                        rows.last().expect("nonempty maintenance batch").id,
                    );
                    let target_batch = match source_database.migrate_table_batch(
                        &file.id,
                        &file.steps,
                        &table_name,
                        &rows,
                    ) {
                        Ok(batch) => batch,
                        Err(error) => {
                            let error = migration_file_error(file, error);
                            self.abort_after_deterministic_maintenance_failure();
                            return Err(error);
                        }
                    };
                    ensure_deadline(control)?;
                    let result = self
                        .durable
                        .as_mut()
                        .expect("format-6 migration has a durable backend")
                        .append_maintenance_batch(file, &target_batch, checkpoint, rows.len());
                    match self.finish_maintenance_result(result) {
                        Ok(next) => info = next,
                        Err(error) => {
                            self.abort_after_nonrecoverable_maintenance_failure(&error);
                            return Err(error);
                        }
                    }
                }
            }
            ensure_deadline(control)?;
            let result = self
                .durable
                .as_mut()
                .expect("format-6 migration has a durable backend")
                .mark_maintenance_ready(file, &target);
            info = match self.finish_maintenance_result(result) {
                Ok(next) => next,
                Err(error) => {
                    self.abort_after_nonrecoverable_maintenance_failure(&error);
                    return Err(error);
                }
            };
        }
        if info.state != MaintenanceState::Ready {
            return Err(Error::new(
                "E_MAINTENANCE_CONFLICT",
                "migration checkpoint is not ready for cutover",
            ));
        }
        ensure_deadline(control)?;
        let result = self
            .durable
            .as_mut()
            .expect("format-6 migration has a durable backend")
            .cutover_maintenance(file, &target);
        self.finish_maintenance_result(result)?;
        let view = self
            .durable
            .as_ref()
            .expect("successful cutover keeps the durable backend")
            .committed_view(&target)
            .and_then(|(database, source)| {
                CommittedView::new(database, source, self.committed.receipts.clone())
            });
        match view {
            Ok(view) => self.committed = Arc::new(view),
            Err(error) => {
                self.durable = None;
                self.write_failed = true;
                self.read_reopen_required = true;
                return Err(Error::new(
                    "E_STORAGE_REOPEN_REQUIRED",
                    format!(
                        "migration cutover committed but its read view could not be created: {}; reopen the database before reading or retrying",
                        error.message
                    ),
                ));
            }
        }
        while self
            .durable
            .as_ref()
            .expect("successful cutover keeps the durable backend")
            .maintenance_info()?
            .is_some()
        {
            let result = self
                .durable
                .as_mut()
                .expect("successful cutover keeps the durable backend")
                .reclaim_maintenance_step();
            if self.finish_maintenance_result(result)? {
                break;
            }
            if ensure_deadline(control).is_err() {
                break;
            }
        }
        Ok(())
    }

    fn finish_maintenance_result<T>(
        &mut self,
        result: std::result::Result<T, CommitFailure>,
    ) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(CommitFailure::Definite(error)) => Err(error),
            Err(CommitFailure::Uncertain(error)) => {
                self.durable = None;
                self.write_failed = true;
                self.read_reopen_required = true;
                Err(Error::new(
                    "E_STORAGE",
                    format!(
                        "maintenance commit result is uncertain: {}; reopen the database and inspect migration status before retrying",
                        error.message
                    ),
                ))
            }
        }
    }

    fn abort_after_deterministic_maintenance_failure(&mut self) {
        if self.write_failed {
            return;
        }
        let Some(_) = self.durable.as_mut() else {
            return;
        };
        let begin = self
            .durable
            .as_mut()
            .expect("durable backend was checked")
            .begin_maintenance_abort();
        match begin {
            Ok(_) => {}
            Err(CommitFailure::Definite(_)) => return,
            Err(CommitFailure::Uncertain(_)) => {
                self.durable = None;
                self.write_failed = true;
                self.read_reopen_required = true;
                return;
            }
        };
        loop {
            let result = self
                .durable
                .as_mut()
                .expect("abort cleanup keeps the durable backend")
                .reclaim_maintenance_step();
            match result {
                Ok(true) | Err(CommitFailure::Definite(_)) => break,
                Ok(false) => {}
                Err(CommitFailure::Uncertain(_)) => {
                    self.durable = None;
                    self.write_failed = true;
                    self.read_reopen_required = true;
                    break;
                }
            }
        }
    }

    fn abort_after_nonrecoverable_maintenance_failure(&mut self, error: &Error) {
        if matches!(
            error.code.as_str(),
            "E_MAINTENANCE_CONFLICT"
                | "E_STORAGE"
                | "E_IO"
                | "E_BUSY"
                | "E_TIMEOUT"
                | "E_CANCELLED"
                | "E_SHUTDOWN"
        ) {
            return;
        }
        self.abort_after_deterministic_maintenance_failure();
    }

    fn commit_candidate(
        &mut self,
        mut candidate: Database,
        wal_source: Option<&str>,
        response: &mut QueryResponse,
        receipt_state: Option<ReceiptMap>,
        write_set: Option<LogicalWriteSet>,
    ) -> Result<(u64, Option<DurableCommitProfile>)> {
        // A committed root never carries changes forward into the next
        // candidate. Row-only callers already extracted the supplied set;
        // full-rebuild callers intentionally discard any internal details.
        let _ = candidate.take_write_set();
        let mut durable_commit_micros = 0;
        let mut durable_profile = None;
        let mut durable_view = None;
        if let Some(durable) = &mut self.durable {
            let receipts = receipt_state
                .as_ref()
                .unwrap_or_else(|| self.committed.receipts.as_ref());
            let result = durable.commit(
                &self.committed.db,
                &self.committed.receipts,
                &candidate,
                receipts,
                write_set.as_ref(),
            );
            match result {
                Ok(profile) => {
                    durable_commit_micros = profile.total_micros;
                    durable_profile = Some(profile);
                    match durable.committed_view(&candidate) {
                        Ok(view) => durable_view = Some(view),
                        Err(error) => {
                            self.durable = None;
                            self.write_failed = true;
                            self.read_reopen_required = true;
                            return Err(Error::new(
                                "E_STORAGE_REOPEN_REQUIRED",
                                format!(
                                    "durable commit succeeded but its read view could not be created: {}; reopen the database before reading or retrying",
                                    error.message
                                ),
                            ));
                        }
                    }
                }
                Err(CommitFailure::Definite(error)) => {
                    if error.code == "E_STORAGE_UPGRADE_REQUIRED" {
                        return Err(error);
                    }
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
        let receipts = receipt_state
            .map(Arc::new)
            .unwrap_or_else(|| self.committed.receipts.clone());
        let next_view = if let Some((db, source)) = durable_view {
            match CommittedView::new(db, source, receipts) {
                Ok(view) => view,
                Err(error) => {
                    self.durable = None;
                    self.write_failed = true;
                    self.read_reopen_required = true;
                    return Err(Error::new(
                        "E_STORAGE_REOPEN_REQUIRED",
                        format!(
                            "durable commit succeeded but its read view identity is invalid: {}; reopen the database before reading or retrying",
                            error.message
                        ),
                    ));
                }
            }
        } else {
            CommittedView::memory(candidate, receipts.as_ref().clone())
        };
        self.committed = Arc::new(next_view);
        self.writes_since_snapshot += 1;
        if self.snapshot_every > 0
            && self.writes_since_snapshot >= self.snapshot_every
            && let Err(error) = self.checkpoint()
        {
            response.warnings.push(error.to_string());
        }
        Ok((durable_commit_micros, durable_profile))
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
                .save(&self.committed.db)
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
        let metadata = self.committed.db.clone();
        let receipts_before = self.committed.receipts.clone();
        // redb's physical integrity check requires that this Engine release
        // its active read transaction first. A concurrently retained service
        // snapshot still causes redb to reject the maintenance operation.
        self.committed = Arc::new(CommittedView::memory(
            metadata.as_ref().clone(),
            receipts_before.as_ref().clone(),
        ));
        let durable = self.durable.as_mut().ok_or_else(|| {
            Error::new(
                "E_CONFIG",
                "integrity check requires a database opened with Engine::open_redb",
            )
        })?;
        let (backend_clean, database, receipts, profile) = match durable.check_integrity() {
            Ok(result) => result,
            Err(error) => {
                let (database, source) = durable.committed_view(&metadata)?;
                self.committed = Arc::new(CommittedView::new(database, source, receipts_before)?);
                return Err(error);
            }
        };
        let versions = durable.versions();
        let (database, source) = durable.committed_view(&database)?;
        self.committed = Arc::new(CommittedView::new(database, source, Arc::new(receipts))?);
        Ok(StorageIntegrity {
            backend: "redb",
            backend_clean,
            schema: self.committed.db.schema_info(),
            versions,
            profile,
        })
    }

    pub fn upgrade_storage(&mut self, target: u32) -> Result<StorageUpgrade> {
        if self.write_failed {
            return Err(Error::new(
                "E_STORAGE",
                "storage upgrade requires reopening after an uncertain commit",
            ));
        }
        if self.read_only {
            return Err(Error::new("E_READ_ONLY", "storage upgrade is a mutation"));
        }
        if self.unfinished_maintenance()? {
            return Err(Error::new(
                "E_MAINTENANCE_REQUIRED",
                "storage upgrade is blocked while a migration generation is unfinished",
            ));
        }
        let metadata_only_upgrade = target
            == crate::redb_storage::PRODUCTION_STORAGE_FORMAT_VERSION
            && self
                .durable
                .as_ref()
                .is_some_and(|durable| durable.versions().format == 5);
        let database = if metadata_only_upgrade {
            self.committed.db.as_ref().clone()
        } else {
            self.mutable_candidate(None)?
        };
        let durable = self.durable.as_mut().ok_or_else(|| {
            Error::new(
                "E_CONFIG",
                "storage upgrade requires a database opened with Engine::open_redb",
            )
        })?;
        match durable.upgrade(&database, &self.committed.receipts, target) {
            Ok(result) => {
                let view = durable
                    .committed_view(&database)
                    .and_then(|(database, source)| {
                        CommittedView::new(database, source, self.committed.receipts.clone())
                    });
                match view {
                    Ok(view) => {
                        self.committed = Arc::new(view);
                        Ok(result)
                    }
                    Err(error) => {
                        self.durable = None;
                        self.write_failed = true;
                        self.read_reopen_required = true;
                        Err(Error::new(
                            "E_STORAGE_REOPEN_REQUIRED",
                            format!(
                                "storage upgrade committed but its read view could not be created: {}; reopen the database before reading or retrying",
                                error.message
                            ),
                        ))
                    }
                }
            }
            Err(CommitFailure::Definite(error)) => Err(error),
            Err(CommitFailure::Uncertain(error)) => {
                self.durable = None;
                self.write_failed = true;
                Err(Error::new(
                    "E_STORAGE",
                    format!(
                        "storage upgrade commit result is uncertain: {}; reopen the database and run check before retrying",
                        error.message
                    ),
                ))
            }
        }
    }

    pub fn schema(&self) -> String {
        self.committed.db.schema_text()
    }
    pub fn tables(&self) -> Vec<String> {
        self.committed.db.table_names()
    }

    pub fn introspection(&self) -> Introspection {
        Introspection {
            schema: self.committed.db.schema_info(),
            schema_source: self.committed.db.schema_text(),
            tables: self.committed.db.table_names(),
            types: self.committed.db.type_names(),
            fields: self.committed.db.field_names(),
            storage: self.storage_mode,
            storage_versions: self
                .durable
                .as_ref()
                .map(|durable| durable.versions())
                .or(self.snapshot_storage_versions),
            read_only: self.read_only,
            migration_count: self.committed.db.migration_history().len(),
            migration_head: self
                .committed
                .db
                .migration_history()
                .last()
                .map(|entry| entry.id.clone()),
            maintenance: self
                .durable
                .as_ref()
                .and_then(|durable| durable.maintenance_info().ok().flatten())
                .map(migration_maintenance),
        }
    }

    pub fn schema_info(&self) -> crate::db::SchemaInfo {
        self.committed.db.schema_info()
    }

    pub fn migration_history(&self) -> &[MigrationEntry] {
        self.committed.db.migration_history()
    }

    pub(crate) fn database_snapshot(&self) -> Result<Database> {
        self.committed
            .db
            .materialize_from_source(self.committed.source.as_ref(), None)
    }

    fn mutable_candidate(&self, control: Option<&ExecutionControl>) -> Result<Database> {
        if self.storage_mode == StorageMode::Redb {
            self.committed
                .db
                .materialize_from_source(self.committed.source.as_ref(), control)
        } else {
            Ok(self.committed.db.as_ref().clone())
        }
    }

    /// Capture one complete committed state for execution outside a service's
    /// writer lock. The clone intentionally has no durable handle or file lock:
    /// it can observe and evaluate the captured state but cannot publish writes.
    pub(crate) fn read_snapshot(&self) -> Self {
        Self {
            committed: self.committed.clone(),
            write_failed: self.write_failed,
            read_reopen_required: self.read_reopen_required,
            read_only: self.read_only,
            snapshot_execution: true,
            storage_mode: self.storage_mode,
            snapshot_storage_versions: self
                .durable
                .as_ref()
                .map(|durable| durable.versions())
                .or(self.snapshot_storage_versions),
            ..Self::default()
        }
    }

    pub(crate) fn logical_backup_view(&self) -> (&Database, &dyn TypedRowSource, &ReceiptMap) {
        (
            self.committed.db.as_ref(),
            self.committed.source.as_ref(),
            self.committed.receipts.as_ref(),
        )
    }

    pub(crate) fn restore_redb(
        path: PathBuf,
        database: Database,
        receipts: ReceiptMap,
    ) -> Result<Self> {
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
            engine.commit_candidate(database, None, &mut response, Some(receipts), None)?;
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
        crate::schema::diff(&self.committed.db, target_source, migration_id, parent)
    }

    fn with_schema(&self, mut response: QueryResponse) -> QueryResponse {
        response.schema = Some(self.committed.db.schema_info());
        response
    }
}

fn ensure_deadline(control: Option<&ExecutionControl>) -> Result<()> {
    control.map_or(Ok(()), ExecutionControl::checkpoint)
}

fn elapsed_micros(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn unix_time_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::new("E_TIME", error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| Error::new("E_LIMIT", "timestamp exceeds u64"))
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

fn migration_maintenance(info: MaintenanceInfo) -> MigrationMaintenance {
    let (phase, actions) = match info.state {
        MaintenanceState::Building => (
            MigrationMaintenancePhase::Building,
            vec!["resume".to_owned(), "abort".to_owned()],
        ),
        MaintenanceState::Ready => (
            MigrationMaintenancePhase::Ready,
            vec!["resume".to_owned(), "abort".to_owned()],
        ),
        MaintenanceState::Aborting => (
            MigrationMaintenancePhase::Aborting,
            vec!["abort".to_owned()],
        ),
        MaintenanceState::Reclaimable => (
            MigrationMaintenancePhase::Reclaimable,
            vec!["abort".to_owned()],
        ),
    };
    MigrationMaintenance {
        phase,
        migration_id: info.migration_id,
        source_generation: info.source_generation,
        target_generation: info.target_generation,
        source_rows_seen: info.source_rows_seen,
        target_rows_written: info.target_rows_written,
        index_entries_written: info.index_entries_written,
        logical_bytes: info.logical_bytes,
        updated_at_unix_ms: info.updated_at_unix_ms,
        actions,
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

fn attach_structured_page(statements: &mut [LocatedStatement], page: PageSpec) -> Result<()> {
    let [located] = statements else {
        return Err(Error::new(
            "E_PAGE_SHAPE",
            "a structured page request requires exactly one read pipeline",
        ));
    };
    let Statement::Pipeline(pipeline) = &mut located.statement else {
        return Err(Error::new(
            "E_PAGE_SHAPE",
            "a structured page request requires a read pipeline",
        ));
    };
    if pipeline
        .stages
        .iter()
        .any(|stage| matches!(stage, Stage::Page(_)))
    {
        return Err(Error::new(
            "E_PAGE_SHAPE",
            "query page stage and structured page request cannot be combined",
        ));
    }
    pipeline.stages.push(Stage::Page(page));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailOnce {
        uncertain: Option<bool>,
    }

    struct FailCommittedView {
        committed: Arc<AtomicBool>,
    }

    impl DurableBackend for FailOnce {
        fn commit(
            &mut self,
            _: &Database,
            _: &ReceiptMap,
            _: &Database,
            _: &ReceiptMap,
            _: Option<&LogicalWriteSet>,
        ) -> std::result::Result<DurableCommitProfile, CommitFailure> {
            match self.uncertain.take() {
                Some(false) => Err(CommitFailure::Definite(Error::new(
                    "E_STORAGE",
                    "injected pre-commit failure",
                ))),
                Some(true) => Err(CommitFailure::Uncertain(Error::new(
                    "E_STORAGE",
                    "injected commit failure",
                ))),
                None => Ok(DurableCommitProfile::default()),
            }
        }

        fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap, StorageCheckProfile)> {
            unreachable!()
        }

        fn supports_production_scalars(&self) -> bool {
            true
        }

        fn versions(&self) -> StorageVersions {
            StorageVersions {
                format: 4,
                catalog_codec: 3,
                value_codec: 2,
                index_key_codec: 2,
                migration_codec: 1,
                receipt_codec: 2,
                maintenance_codec: 0,
                backup_codec: 3,
            }
        }

        fn upgrade(
            &mut self,
            _: &Database,
            _: &ReceiptMap,
            _: u32,
        ) -> std::result::Result<StorageUpgrade, CommitFailure> {
            match self.uncertain.take() {
                Some(false) => Err(CommitFailure::Definite(Error::new(
                    "E_STORAGE",
                    "injected upgrade pre-commit failure",
                ))),
                Some(true) => Err(CommitFailure::Uncertain(Error::new(
                    "E_STORAGE",
                    "injected upgrade commit failure",
                ))),
                None => Ok(StorageUpgrade {
                    previous_format: 3,
                    format: 4,
                    changed: true,
                }),
            }
        }
    }

    impl DurableBackend for FailCommittedView {
        fn commit(
            &mut self,
            _: &Database,
            _: &ReceiptMap,
            _: &Database,
            _: &ReceiptMap,
            _: Option<&LogicalWriteSet>,
        ) -> std::result::Result<DurableCommitProfile, CommitFailure> {
            self.committed.store(true, Ordering::SeqCst);
            Ok(DurableCommitProfile::default())
        }

        fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap, StorageCheckProfile)> {
            unreachable!()
        }

        fn supports_production_scalars(&self) -> bool {
            true
        }

        fn versions(&self) -> StorageVersions {
            StorageVersions {
                format: 5,
                catalog_codec: 3,
                value_codec: 2,
                index_key_codec: 3,
                migration_codec: 1,
                receipt_codec: 2,
                maintenance_codec: 1,
                backup_codec: 4,
            }
        }

        fn committed_view(&self, _: &Database) -> Result<(Arc<Database>, Arc<dyn TypedRowSource>)> {
            Err(Error::new("E_STORAGE", "injected committed-view failure"))
        }

        fn upgrade(
            &mut self,
            _: &Database,
            _: &ReceiptMap,
            _: u32,
        ) -> std::result::Result<StorageUpgrade, CommitFailure> {
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
    fn mutation_profile_reports_candidate_shape_and_resets_on_read() {
        let mut engine = Engine::memory();
        let schema = engine.execute("create table entries (id int, value int)");
        assert!(schema.ok, "{}", schema.message);
        assert!(engine.last_mutation_profile().unwrap().full_rebuild);

        let inserted = engine.execute("insert entries {id = 1, value = 2}");
        assert!(inserted.ok, "{}", inserted.message);
        assert_eq!(
            engine.last_mutation_profile(),
            Some(MutationProfile {
                candidate_micros: engine.last_mutation_profile().unwrap().candidate_micros,
                durable_commit_micros: 0,
                full_rebuild: false,
                touched_tables: 1,
                row_inserts: 1,
                row_updates: 0,
                row_deletes: 0,
                index_inserts: 0,
                index_deletes: 0,
                receipt_changes: 0,
                durable: None,
            })
        );

        let read = engine.execute("from entries");
        assert!(read.ok, "{}", read.message);
        assert_eq!(engine.last_mutation_profile(), None);
    }

    #[test]
    fn uncertain_storage_upgrade_disables_the_open_engine() {
        let mut engine = engine_with_failure(true);
        let error = engine.upgrade_storage(4).unwrap_err();
        assert_eq!(error.code, "E_STORAGE");
        assert!(error.message.contains("result is uncertain"));
        assert!(engine.write_failed);
        assert!(engine.durable.is_none());
    }

    #[test]
    fn uncertain_maintenance_commit_requires_reopen_before_reads_or_writes() {
        let mut engine = Engine::memory();
        let error = engine
            .finish_maintenance_result::<()>(Err(CommitFailure::Uncertain(Error::new(
                "E_STORAGE",
                "injected maintenance commit failure",
            ))))
            .unwrap_err();
        assert_eq!(error.code, "E_STORAGE");
        assert!(engine.write_failed);
        assert!(engine.read_reopen_required);
        assert!(engine.durable.is_none());
        let read = engine.execute("from missing");
        assert_eq!(read.error.unwrap().code, "E_STORAGE_REOPEN_REQUIRED");
    }

    #[test]
    fn uncertain_commit_does_not_publish_state_to_read_snapshots() {
        let mut engine = engine_with_failure(true);
        let failed = engine.execute("create table uncertain (id int)");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_STORAGE");
        assert!(engine.write_failed);

        let mut snapshot = engine.read_snapshot();
        let read = snapshot.execute("from uncertain");
        assert!(!read.ok);
        assert_eq!(read.error.unwrap().code, "E_TABLE");
        let rejected = engine.execute("create table later (id int)");
        assert!(!rejected.ok);
        assert_eq!(rejected.error.unwrap().code, "E_STORAGE");
    }

    #[test]
    fn committed_view_failure_requires_reopen_and_blocks_stale_reads() {
        let committed = Arc::new(AtomicBool::new(false));
        let mut engine = Engine {
            durable: Some(Box::new(FailCommittedView {
                committed: committed.clone(),
            })),
            storage_mode: StorageMode::Redb,
            ..Engine::default()
        };
        let mut old_snapshot = engine.read_snapshot();

        let failed = engine.execute("create table committed (id int)");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_STORAGE_REOPEN_REQUIRED");
        assert!(committed.load(Ordering::SeqCst));
        assert!(engine.write_failed);
        assert!(engine.read_reopen_required);
        assert!(engine.durable.is_none());

        let blocked_read = engine.execute("from committed");
        assert!(!blocked_read.ok);
        assert_eq!(
            blocked_read.error.unwrap().code,
            "E_STORAGE_REOPEN_REQUIRED"
        );
        let old_read = old_snapshot.execute("from committed");
        assert!(!old_read.ok);
        assert_eq!(old_read.error.unwrap().code, "E_TABLE");
    }

    #[test]
    fn read_snapshot_captures_one_database_and_receipt_commit_root() {
        let mut engine = Engine::memory();
        assert!(engine.execute("create table entries (id int)").ok);
        let mut snapshot = engine.read_snapshot();
        let captured = snapshot.committed.clone();
        assert!(Arc::ptr_eq(&captured, &engine.committed));

        engine
            .execute_idempotent_with_params(
                "entry-1",
                DIGEST_A,
                "insert entries {id = 1}",
                BTreeMap::new(),
                None,
            )
            .unwrap();

        assert!(!Arc::ptr_eq(&captured, &engine.committed));
        assert_eq!(snapshot.execute("from entries").rows.len(), 0);
        assert_eq!(snapshot.idempotency_status().unwrap().count, 0);
        assert_eq!(engine.execute("from entries").rows.len(), 1);
        assert_eq!(engine.idempotency_status().unwrap().count, 1);
    }

    #[test]
    fn redb_read_snapshot_stays_on_one_mvcc_root_across_commit() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "unionid-redb-snapshot-{}-{nonce}.redb",
            std::process::id()
        ));
        let mut engine = Engine::open_redb(&path).unwrap();
        let setup = engine.execute(
            "type Entry =\n  id int\n  label text\ntable entries Entry\n  key id\ninsert entries {id = 1, label = \"old\"}",
        );
        assert!(setup.ok, "{}", setup.message);
        let mut old_snapshot = engine.read_snapshot();
        let old_sequence = old_snapshot.committed.source.snapshot_identity().sequence;

        let updated = engine
            .execute_idempotent_with_params(
                "mvcc-update",
                DIGEST_A,
                "update entries\nfilter id == 1\nset label = \"new\"",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(updated.response.ok, "{}", updated.response.message);
        let old = old_snapshot.execute("from entries | filter id == 1");
        let new = engine.execute("from entries | filter id == 1");
        assert!(old.ok, "{}", old.message);
        assert!(new.ok, "{}", new.message);
        assert!(old.rows[0]["label"].cmp_eq(&crate::Value::Text("old".into())));
        assert!(new.rows[0]["label"].cmp_eq(&crate::Value::Text("new".into())));
        assert_eq!(old_snapshot.idempotency_status().unwrap().count, 0);
        assert_eq!(engine.idempotency_status().unwrap().count, 1);
        assert!(engine.committed.source.snapshot_identity().sequence > old_sequence);

        drop(old_snapshot);
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn redb_read_snapshot_keeps_old_generation_across_migration_cutover() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "unionid-redb-migration-snapshot-{}-{nonce}.redb",
            std::process::id()
        ));
        let mut engine = Engine::open_redb(&path).unwrap();
        let setup = engine.execute(
            "type Item =\n  id int\n  value int\ntable items Item\n  key id\ninsert items {id = 1, value = 2}",
        );
        assert!(setup.ok, "{}", setup.message);
        let mut old_snapshot = engine.read_snapshot();
        let old_identity = old_snapshot.committed.source.snapshot_identity();
        let migration =
            MigrationFile::parse("migration m0001_enabled\n  add field Item.enabled bool = true\n")
                .unwrap();

        engine
            .apply_migrations(std::slice::from_ref(&migration))
            .unwrap();

        let old = old_snapshot.execute("from items | filter id == 1");
        let new = engine.execute("from items | filter id == 1");
        assert!(old.ok, "{}", old.message);
        assert!(new.ok, "{}", new.message);
        assert!(!old.rows[0].contains_key("enabled"));
        assert!(new.rows[0]["enabled"].cmp_eq(&crate::Value::Bool(true)));
        assert!(engine.committed.source.snapshot_identity().sequence > old_identity.sequence);
        assert!(engine.introspection().maintenance.is_none());

        drop(old_snapshot);
        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn concurrent_redb_cache_misses_are_bounded_and_publish_one_cached_row() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "unionid-redb-cache-race-{}-{nonce}.redb",
            std::process::id()
        ));
        {
            let mut setup = Engine::open_redb(&path).unwrap();
            let response = setup.execute(
                "create table entries (id int, value text)\ncreate index entries (id)\ninsert entries {id = 1, value = \"one\"}",
            );
            assert!(response.ok, "{}", response.message);
        }
        let mut engine = Engine::open_redb(&path).unwrap();
        let mut first = engine.read_snapshot();
        let mut second = engine.read_snapshot();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first.execute("from entries | filter id == 1")
        });
        let second = std::thread::spawn(move || {
            barrier.wait();
            second.execute("from entries | filter id == 1")
        });
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(first.ok, "{}", first.message);
        assert!(second.ok, "{}", second.message);
        assert_eq!(first.rows.len(), 1);
        assert_eq!(second.rows.len(), 1);
        let misses =
            first.execution.unwrap().row_cache_misses + second.execution.unwrap().row_cache_misses;
        assert!((1..=2).contains(&misses));

        let cached = engine.execute("from entries | filter id == 1");
        assert!(cached.ok, "{}", cached.message);
        let observation = cached.execution.unwrap();
        assert_eq!(observation.rows_decoded, 0);
        assert_eq!(observation.row_cache_hits, 1);

        drop(engine);
        let _ = std::fs::remove_file(path);
    }

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn memory_idempotency_replays_original_response_and_rejects_conflicts() {
        let mut engine = Engine::memory();
        assert!(engine.execute("create table entries (id int)").ok);
        let first = engine
            .execute_idempotent_with_params(
                "entry-1",
                DIGEST_A,
                "insert entries {id = 1}\nreturning id",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(!first.replayed);
        assert_eq!(first.durability, IdempotencyDurability::ProcessLocal);
        assert_eq!(first.response.rows.len(), 1);
        assert_eq!(engine.last_mutation_profile().unwrap().receipt_changes, 1);

        let replay = engine
            .execute_idempotent_with_params(
                "entry-1",
                DIGEST_A,
                "this source is deliberately not parsed during replay",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(engine.last_mutation_profile(), None);
        assert_eq!(replay.committed_sequence, first.committed_sequence);
        assert_eq!(replay.response.schema, first.response.schema);
        assert_eq!(engine.execute("from entries").rows.len(), 1);

        let conflict = engine
            .execute_idempotent_with_params(
                "entry-1",
                DIGEST_B,
                "insert entries {id = 2}",
                BTreeMap::new(),
                None,
            )
            .unwrap_err();
        assert_eq!(conflict.code, "E_IDEMPOTENCY_CONFLICT");
        assert_eq!(engine.execute("from entries").rows.len(), 1);
    }

    #[test]
    fn failed_and_read_only_idempotent_requests_do_not_consume_keys() {
        let mut engine = Engine::memory();
        assert!(engine.execute("create table entries (id int)").ok);
        let failed = engine
            .execute_idempotent_with_params(
                "retryable",
                DIGEST_A,
                "insert missing {id = 1}",
                BTreeMap::new(),
                None,
            )
            .unwrap_err();
        assert_eq!(failed.code, "E_TABLE");
        let retried = engine
            .execute_idempotent_with_params(
                "retryable",
                DIGEST_B,
                "insert entries {id = 1}",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(!retried.replayed);

        let read = engine
            .execute_idempotent_with_params("read", DIGEST_A, "from entries", BTreeMap::new(), None)
            .unwrap_err();
        assert_eq!(read.code, "E_IDEMPOTENCY_NOT_MUTATION");

        let mut read_only = engine.with_read_only(true);
        let rejected = read_only
            .execute_idempotent_with_params(
                "read-only",
                DIGEST_A,
                "insert entries {id = 2}",
                BTreeMap::new(),
                None,
            )
            .unwrap_err();
        assert_eq!(rejected.code, "E_READ_ONLY");

        let mut engine = read_only.with_read_only(false);
        let timed_out = engine
            .execute_idempotent_with_params_until(
                "timed-out",
                DIGEST_A,
                "insert entries {id = 2}",
                BTreeMap::new(),
                None,
                std::time::Instant::now(),
            )
            .unwrap_err();
        assert_eq!(timed_out.code, "E_TIMEOUT");
        let retry = engine
            .execute_idempotent_with_params(
                "timed-out",
                DIGEST_B,
                "insert entries {id = 2}",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(!retry.replayed);
    }

    #[test]
    fn storage_failures_do_not_publish_receipts_before_commit_success() {
        let mut definite = engine_with_failure(false);
        let failed = definite
            .execute_idempotent_with_params(
                "retry",
                DIGEST_A,
                "create table entries (id int)",
                BTreeMap::new(),
                None,
            )
            .unwrap_err();
        assert_eq!(failed.code, "E_STORAGE");
        assert!(definite.committed.receipts.is_empty());
        let retry = definite
            .execute_idempotent_with_params(
                "retry",
                DIGEST_B,
                "create table entries (id int)",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(!retry.replayed);

        let mut uncertain = engine_with_failure(true);
        let failed = uncertain
            .execute_idempotent_with_params(
                "uncertain",
                DIGEST_A,
                "create table entries (id int)",
                BTreeMap::new(),
                None,
            )
            .unwrap_err();
        assert_eq!(failed.code, "E_STORAGE");
        assert!(uncertain.committed.receipts.is_empty());
        assert!(uncertain.write_failed);
    }

    #[test]
    fn receipt_pruning_is_bounded_previewed_and_explicit() {
        let mut engine = Engine::memory();
        assert!(
            engine
                .execute("type Entry =\n  id int\n  value text\ntable entries Entry\n  key id")
                .ok
        );
        for (key, id) in [("first", 1), ("second", 2)] {
            engine
                .execute_idempotent_with_params(
                    key,
                    DIGEST_A,
                    &format!("insert entries {{id = {id}, value = \"old\"}}"),
                    BTreeMap::new(),
                    None,
                )
                .unwrap();
        }
        let status = engine.idempotency_status().unwrap();
        assert_eq!(status.count, 2);
        assert!(status.encoded_bytes > 0);
        assert_eq!(status.oldest.unwrap().key, "first");

        let options = IdempotencyPruneOptions {
            completed_before_unix_ms: None,
            committed_through_sequence: Some(2),
            max_receipts: 1,
        };
        let preview = engine.plan_idempotency_prune(options.clone()).unwrap();
        assert_eq!(preview.selected_count, 1);
        assert!(!preview.applied);
        assert_eq!(engine.idempotency_status().unwrap().count, 2);

        let applied = engine.prune_idempotency_receipts(options).unwrap();
        assert!(applied.applied);
        assert_eq!(engine.idempotency_status().unwrap().count, 1);
        let reused = engine
            .execute_idempotent_with_params(
                "first",
                DIGEST_B,
                "update entries\nfilter id == 1\nset value = \"after-prune\"",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(!reused.replayed);
        assert!(
            engine
                .execute_idempotent_with_params(
                    "second",
                    DIGEST_A,
                    "not parsed for retained receipt",
                    BTreeMap::new(),
                    None,
                )
                .unwrap()
                .replayed
        );
        assert_eq!(
            engine
                .plan_idempotency_prune(IdempotencyPruneOptions {
                    completed_before_unix_ms: None,
                    committed_through_sequence: None,
                    max_receipts: 1,
                })
                .unwrap_err()
                .code,
            "E_IDEMPOTENCY_PRUNE"
        );
    }

    #[test]
    fn definite_pre_commit_failure_keeps_old_state_and_allows_retry() {
        let mut engine = engine_with_failure(false);
        let failed = engine.execute("create table entries (id int)");
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "E_STORAGE");
        assert!(failed.message.contains("aborted before commit"));
        assert_eq!(engine.last_mutation_profile(), None);
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
        assert_eq!(engine.last_mutation_profile(), None);
        assert_eq!(
            engine.execute("from entries").error.unwrap().code,
            "E_TABLE"
        );
        let blocked = engine.execute("create table later (id int)");
        assert!(!blocked.ok);
        assert!(blocked.message.contains("writes are disabled"));
    }

    #[test]
    fn read_only_engine_allows_reads_and_atomically_rejects_mutations() {
        let mut engine = Engine::memory();
        assert!(
            engine
                .execute(
                    "type Task =\n  id int\n  title text\ntable tasks Task\n  key id\ninsert tasks {id = 1, title = \"kept\"}"
                )
                .ok
        );
        let schema = engine.schema_info();
        let mut engine = engine.with_read_only(true);

        assert!(engine.is_read_only());
        assert!(engine.introspection().read_only);
        assert_eq!(engine.execute("from tasks").rows.len(), 1);
        assert!(engine.execute("explain from tasks | filter id == 1").ok);

        let rejected =
            engine.execute("from tasks | take 1\ninsert tasks {id = 2, title = \"rejected\"}");
        assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
        assert_eq!(engine.schema_info(), schema);
        let rows = engine.execute("from tasks | sort id");
        assert!(rows.ok, "{}", rows.message);
        assert_eq!(rows.rows.len(), 1);
    }

    #[test]
    fn read_only_prepared_writes_still_validate_parameters_before_rejection() {
        let mut engine = Engine::memory();
        assert!(
            engine
                .execute("type Task =\n  id int\ntable tasks Task\n  key id")
                .ok
        );
        let prepared = engine.prepare("insert tasks $task").unwrap();
        let mut engine = engine.with_read_only(true);

        let missing = engine.execute_prepared(&prepared, BTreeMap::new());
        assert_eq!(missing.error.unwrap().code, "E_PARAM_MISSING");
        let rejected = engine.execute_prepared(
            &prepared,
            BTreeMap::from([(
                "task".into(),
                crate::Value::Record(BTreeMap::from([("id".into(), crate::Value::Int(1))])),
            )]),
        );
        assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
        assert!(engine.execute("from tasks").rows.is_empty());
    }

    #[test]
    fn read_only_engine_plans_but_does_not_apply_pending_migrations() {
        let file = MigrationFile::parse(
            "migration m0001_initial\n  add type Task =\n    id int\n  add table tasks Task key id\n",
        )
        .unwrap();
        let mut read_only = Engine::memory().with_read_only(true);
        assert_eq!(
            read_only
                .plan_migrations(std::slice::from_ref(&file))
                .unwrap()
                .pending
                .len(),
            1
        );
        assert_eq!(
            read_only
                .apply_migrations(std::slice::from_ref(&file))
                .unwrap_err()
                .code,
            "E_READ_ONLY"
        );

        let mut writable = Engine::memory();
        writable
            .apply_migrations(std::slice::from_ref(&file))
            .unwrap();
        let mut read_only = writable.with_read_only(true);
        let repeated = read_only.apply_migrations(&[file]).unwrap();
        assert!(repeated.applied.is_empty());
        assert_eq!(repeated.skipped, ["m0001_initial"]);
    }

    #[test]
    fn protocol_v1_preflight_rejects_native_results_without_publishing_prior_mutations() {
        use crate::model::{Column, ScalarType};
        use crate::query::Statement;
        use crate::scalars::Uuid;

        let mut engine = Engine::memory();
        Arc::make_mut(&mut Arc::make_mut(&mut engine.committed).db)
            .execute(Statement::DefineType {
                name: "NativeRow".into(),
                ty: ScalarType::Record(vec![
                    Column {
                        name: "id".into(),
                        ty: ScalarType::Int,
                        default: None,
                        id: 0,
                    },
                    Column {
                        name: "native".into(),
                        ty: ScalarType::Uuid,
                        default: None,
                        id: 0,
                    },
                ]),
            })
            .unwrap();
        Arc::make_mut(&mut Arc::make_mut(&mut engine.committed).db)
            .execute(Statement::TypedTable {
                table: "native_rows".into(),
                row_type: "NativeRow".into(),
                key: None,
            })
            .unwrap();
        Arc::make_mut(&mut Arc::make_mut(&mut engine.committed).db)
            .execute(Statement::Insert {
                table: "native_rows".into(),
                values: crate::Value::Record(BTreeMap::from([
                    ("id".into(), crate::Value::Int(1)),
                    (
                        "native".into(),
                        crate::Value::Uuid(Uuid::from_bytes([1; 16])),
                    ),
                ])),
                returning: None,
            })
            .unwrap();
        assert!(
            engine
                .execute("type Plain = { id int }\ntable plain Plain\ninsert plain { id = 1 }")
                .ok
        );

        let error = engine
            .preflight_protocol_v1(
                "insert plain { id = 2 }\nfrom native_rows | select { native }",
                &BTreeMap::new(),
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, "E_PROTOCOL_TYPE");
        assert_eq!(engine.execute("from plain").rows.len(), 1);
        assert!(
            engine
                .preflight_protocol_v1("from native_rows | select { id }", &BTreeMap::new(), None,)
                .is_ok()
        );
        for source in [
            "delete native_rows | take 1 | returning { native }",
            "explain from native_rows | select { native }",
        ] {
            assert_eq!(
                engine
                    .preflight_protocol_v1(source, &BTreeMap::new(), None)
                    .unwrap_err()
                    .code,
                "E_PROTOCOL_TYPE",
                "{source}"
            );
        }
        assert_eq!(engine.execute("from native_rows").rows.len(), 1);
        assert_eq!(
            engine
                .preflight_protocol_v1_introspection()
                .unwrap_err()
                .code,
            "E_PROTOCOL_TYPE"
        );
    }

    #[test]
    fn protocol_v1_receipt_preflight_preserves_parse_free_replay() {
        let mut engine = Engine::memory();
        assert!(engine.execute("create table entries (id int)").ok);
        engine
            .execute_idempotent_with_params(
                "replay",
                DIGEST_A,
                "insert entries { id = 1 }",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(
            engine
                .preflight_protocol_v1_receipt("replay", DIGEST_A)
                .unwrap()
        );
        assert_eq!(
            engine
                .preflight_protocol_v1_receipt("replay", DIGEST_B)
                .unwrap_err()
                .code,
            "E_IDEMPOTENCY_CONFLICT"
        );
        let replay = engine
            .execute_idempotent_with_params(
                "replay",
                DIGEST_A,
                "this source is deliberately not parsed",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        assert!(replay.replayed);
    }
}
