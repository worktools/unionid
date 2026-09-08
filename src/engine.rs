use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::db::{Database, QueryResponse};
use crate::error::{Error, Result};
use crate::idempotency::{
    IdempotencyDurability, IdempotencyPruneOptions, IdempotencyPruneResult, IdempotencyReceipt,
    IdempotencyStatus, IdempotentExecution, MAX_IDEMPOTENCY_PRUNE_RECEIPTS,
    MAX_IDEMPOTENCY_RECEIPTS, MAX_IDEMPOTENCY_TOTAL_BYTES, ReceiptMap, boundary,
    receipt_encoded_len, validate_digest, validate_key, validate_new_receipt, validate_receipts,
};
use crate::introspection::{Introspection, StorageMode};
use crate::migration::{
    MigrationApply, MigrationEntry, MigrationFile, MigrationPlan, MigrationPlanItem,
    MigrationStatus, describe_step, validate_files_against_history,
};
use crate::query::{LocatedStatement, PageSpec, Stage, Statement};
use crate::redb_storage::{CommitFailure, RedbStore};
use crate::snapshot::SnapshotStore;
use crate::syntax;
use crate::wal::Wal;

/// The shared execution boundary. A source request is one atomic batch.
/// The preview stages writes by cloning its small in-memory database.
#[derive(Default)]
pub struct Engine {
    db: Database,
    receipts: ReceiptMap,
    wal: Option<Wal>,
    snapshot: Option<SnapshotStore>,
    snapshot_every: usize,
    writes_since_snapshot: usize,
    write_failed: bool,
    read_only: bool,
    durable: Option<Box<dyn DurableBackend>>,
    storage_mode: StorageMode,
    _locks: Vec<DatabaseLock>,
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
        receipts: &ReceiptMap,
    ) -> std::result::Result<(), CommitFailure>;
    fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap)>;
}

impl DurableBackend for RedbStore {
    fn commit(
        &mut self,
        previous: &Database,
        database: &Database,
        receipts: &ReceiptMap,
    ) -> std::result::Result<(), CommitFailure> {
        RedbStore::commit(self, previous, database, receipts)
    }

    fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap)> {
        RedbStore::check_integrity(self)
    }
}

#[derive(Clone, Copy)]
struct PendingIdempotency<'a> {
    key: &'a str,
    digest: &'a str,
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
            db,
            receipts: ReceiptMap::new(),
            wal,
            snapshot,
            snapshot_every,
            writes_since_snapshot: 0,
            write_failed: false,
            read_only: false,
            durable: None,
            storage_mode,
            _locks: locks,
        })
    }

    /// Open the durable redb backend. Every mutating source request is
    /// committed as one synchronous, two-phase redb transaction.
    pub fn open_redb(path: impl Into<PathBuf>) -> Result<Self> {
        let (redb, db, receipts) = RedbStore::open(path)?;
        Ok(Self {
            db,
            receipts,
            durable: Some(Box::new(redb)),
            storage_mode: StorageMode::Redb,
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
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
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
        self.execute_with_params_at_schema_and_deadline(
            source,
            parameters,
            expected_schema,
            Some(deadline),
        )
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
        match self.try_execute_with_params_and_idempotency(
            source,
            parameters,
            expected_schema,
            Some(deadline),
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
        self.execute_idempotent_with_deadline(
            key,
            digest,
            source,
            parameters,
            expected_schema,
            Some(deadline),
        )
    }

    fn execute_idempotent_with_deadline(
        &mut self,
        key: &str,
        digest: &str,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<std::time::Instant>,
    ) -> Result<IdempotentExecution> {
        validate_key(key)?;
        validate_digest(digest)?;
        let durability = if self.durable.is_some() {
            IdempotencyDurability::Durable
        } else {
            IdempotencyDurability::ProcessLocal
        };
        if let Some(receipt) = self.receipts.get(key) {
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
        let encoded_bytes = validate_receipts(&self.receipts, self.db.sequence)?;
        let mut ordered = self.receipts.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(key, receipt)| {
            (
                receipt.committed_sequence,
                receipt.completed_at_unix_ms,
                key.as_str(),
            )
        });
        Ok(IdempotencyStatus {
            count: self.receipts.len(),
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
                .checked_add(receipt_encoded_len(&self.receipts[*key])?)
                .ok_or_else(|| Error::new("E_IDEMPOTENCY_CAPACITY", "receipt size overflow"))
        })?;
        Ok(IdempotencyPruneResult {
            options,
            selected_count: selected.len(),
            selected_encoded_bytes,
            remaining_count: self.receipts.len().saturating_sub(selected.len()),
            applied: false,
            first_selected: selected
                .first()
                .map(|key| boundary(key, &self.receipts[*key])),
            last_selected: selected
                .last()
                .map(|key| boundary(key, &self.receipts[*key])),
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
        let mut receipts = self.receipts.clone();
        for key in keys {
            receipts.remove(&key);
        }
        let mut candidate = self.db.clone();
        candidate.sequence = self
            .db
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::new("E_LIMIT", "commit sequence exhausted"))?;
        let mut response = QueryResponse::ok_message("idempotency receipts pruned");
        self.commit_candidate(candidate, None, &mut response, Some(receipts))?;
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
        let mut preview = self.db.clone();
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
        if self.db.has_production_scalars()? {
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
        let Some(receipt) = self.receipts.get(key) else {
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
        if prepared.mutating
            && parameters.values().any(crate::Value::requires_protocol_v2)
            && (self.durable.is_some() || self.snapshot.is_some())
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
        self.execute_prepared_with_deadline(prepared, parameters, Some(deadline))
    }

    fn try_execute_with_params(
        &mut self,
        source: &str,
        parameters: std::collections::BTreeMap<String, crate::Value>,
        expected_schema: Option<&crate::db::SchemaInfo>,
        deadline: Option<std::time::Instant>,
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
        deadline: Option<std::time::Instant>,
        idempotency: Option<PendingIdempotency<'_>>,
        page: Option<PageSpec>,
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
        if let Some(page) = page {
            attach_structured_page(&mut statements, page)?;
        }
        let native_parameters = parameters.values().any(crate::Value::requires_protocol_v2);
        if native_parameters
            && (self.durable.is_some() || self.wal.is_some() || self.snapshot.is_some())
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
        deadline: Option<std::time::Instant>,
        idempotency: Option<PendingIdempotency<'_>>,
    ) -> Result<QueryResponse> {
        let mutating = statements.iter().any(|s| s.statement.is_mutating());
        let schema_changing = statements.iter().any(|s| s.statement.changes_schema());
        if mutating && self.read_only {
            return Err(Error::new(
                "E_READ_ONLY",
                "mutating scripts are disabled by the read-only execution boundary",
            ));
        }
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
            let receipt_state = if let Some(idempotency) = idempotency {
                response.schema = Some(candidate.schema_info());
                let receipt = IdempotencyReceipt {
                    digest: idempotency.digest.to_owned(),
                    committed_sequence: candidate.sequence,
                    completed_at_unix_ms: unix_time_ms()?,
                    response: response.clone(),
                };
                validate_new_receipt(&self.receipts, &receipt)?;
                let mut receipts = self.receipts.clone();
                receipts.insert(idempotency.key.to_owned(), receipt);
                Some(receipts)
            } else {
                None
            };
            self.commit_candidate(candidate, wal_source, &mut response, receipt_state)?;
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
        self.commit_candidate(candidate, None, &mut response, None)
    }

    fn commit_candidate(
        &mut self,
        candidate: Database,
        wal_source: Option<&str>,
        response: &mut QueryResponse,
        receipt_state: Option<ReceiptMap>,
    ) -> Result<()> {
        if let Some(durable) = &mut self.durable {
            let receipts = receipt_state.as_ref().unwrap_or(&self.receipts);
            match durable.commit(&self.db, &candidate, receipts) {
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
        if let Some(receipts) = receipt_state {
            self.receipts = receipts;
        }
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
        let (backend_clean, database, receipts) = durable.check_integrity()?;
        self.db = database;
        self.receipts = receipts;
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
            read_only: self.read_only,
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

    pub(crate) fn logical_snapshot(&self) -> (Database, ReceiptMap) {
        (self.db.clone(), self.receipts.clone())
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
            engine.commit_candidate(database, None, &mut response, Some(receipts))?;
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

    struct FailOnce {
        uncertain: Option<bool>,
    }

    impl DurableBackend for FailOnce {
        fn commit(
            &mut self,
            _: &Database,
            _: &Database,
            _: &ReceiptMap,
        ) -> std::result::Result<(), CommitFailure> {
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

        fn check_integrity(&mut self) -> Result<(bool, Database, ReceiptMap)> {
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
        assert!(definite.receipts.is_empty());
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
        assert!(uncertain.receipts.is_empty());
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
        engine
            .db
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
        engine
            .db
            .execute(Statement::TypedTable {
                table: "native_rows".into(),
                row_type: "NativeRow".into(),
                key: None,
            })
            .unwrap();
        engine
            .db
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
