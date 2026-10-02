//! Offline, bounded validation of saved queries along a migration plan.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{MigrationApply, MigrationPlan};
use crate::Engine;
use crate::db::{Database, SchemaInfo};
use crate::error::{Error, Result};
use crate::portable::{CompatibilityLevel, TypeDescription, TypeShape};
use crate::query_contract::QueryDescription;

pub const MAX_QUERY_FILES: usize = 1_024;
pub const MAX_QUERY_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_QUERY_BINDS: usize = 65_536;
pub const MAX_QUERY_DIAGNOSTICS: usize = 4_096;
pub const MAX_QUERY_REPORT_BYTES: usize = 1024 * 1024;

/// Immutable caller-supplied source, identified by its relative file path.
#[derive(Debug, Clone)]
pub struct MigrationQuery {
    pub path: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryCheckpointFailure {
    /// None denotes the current committed schema, before pending migrations.
    pub migration_id: Option<String>,
    pub schema: SchemaInfo,
    pub error: Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryFileValidation {
    pub path: String,
    pub valid: bool,
    pub current_valid: bool,
    pub compatibility: CompatibilityLevel,
    /// None when the current or target query cannot be bound.
    pub parameters_changed: Option<bool>,
    pub result_changed: Option<bool>,
    pub failures: Vec<QueryCheckpointFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryValidation {
    pub version: u32,
    pub current_schema: SchemaInfo,
    pub target_schema: SchemaInfo,
    pub checked_files: usize,
    pub valid: bool,
    pub files: Vec<QueryFileValidation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationQueryPlan {
    #[serde(flatten)]
    pub plan: MigrationPlan,
    pub query_validation: QueryValidation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationQueryApply {
    #[serde(flatten)]
    pub applied: MigrationApply,
    pub query_validation: QueryValidation,
}

/// Keep a complete preflight report when validation or a later apply fails.
#[derive(Debug)]
pub struct MigrationQueryError {
    pub error: Box<Error>,
    pub query_validation: Option<Box<QueryValidation>>,
}

impl From<Error> for MigrationQueryError {
    fn from(error: Error) -> Self {
        Self {
            error: Box::new(error),
            query_validation: None,
        }
    }
}

impl std::fmt::Display for MigrationQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for MigrationQueryError {}

pub(crate) struct QueryValidator<'a> {
    queries: Vec<&'a MigrationQuery>,
    baselines: Vec<Option<Signature>>,
    files: Vec<QueryFileValidation>,
    diagnostics: usize,
    diagnostic_bytes: usize,
    first: bool,
}

#[derive(Clone)]
struct Signature {
    parameters: [u8; 32],
    result: [u8; 32],
}

impl<'a> QueryValidator<'a> {
    pub(crate) fn new(queries: &'a [MigrationQuery], checkpoints: usize) -> Result<Self> {
        if queries.is_empty() {
            return Err(Error::new(
                "E_MIGRATION",
                "saved-query preflight requires at least one query",
            ));
        }
        if queries.len() > MAX_QUERY_FILES
            || queries
                .len()
                .checked_mul(checkpoints)
                .is_none_or(|n| n > MAX_QUERY_BINDS)
        {
            return Err(Error::new(
                "E_LIMIT",
                "saved-query file or checkpoint binding budget exceeded",
            ));
        }
        let mut bytes = 0_usize;
        let mut paths = BTreeSet::new();
        for query in queries {
            if query.path.is_empty() || !paths.insert(&query.path) {
                return Err(Error::new(
                    "E_MIGRATION",
                    "saved-query paths must be nonempty and unique",
                ));
            }
            bytes = bytes
                .checked_add(query.source.len())
                .ok_or_else(|| Error::new("E_LIMIT", "saved-query source budget exceeded"))?;
            if bytes > MAX_QUERY_SOURCE_BYTES
                || query.source.len() > crate::syntax::MAX_SOURCE_BYTES
            {
                return Err(Error::new("E_LIMIT", "saved-query source budget exceeded"));
            }
        }
        let mut queries = queries.iter().collect::<Vec<_>>();
        queries.sort_by(|a, b| a.path.cmp(&b.path));
        let files = queries
            .iter()
            .map(|q| QueryFileValidation {
                path: q.path.clone(),
                valid: false,
                current_valid: false,
                compatibility: CompatibilityLevel::Incompatible,
                parameters_changed: None,
                result_changed: None,
                failures: Vec::new(),
            })
            .collect();
        let validator = Self {
            baselines: vec![None; queries.len()],
            queries,
            files,
            diagnostics: 0,
            diagnostic_bytes: 0,
            first: true,
        };
        // Reject excessive path metadata before any binding.
        check_report_size(&validator.files)?;
        Ok(validator)
    }

    pub(crate) fn inspect(&mut self, migration: Option<&str>, database: &Database) -> Result<()> {
        let engine = Engine::from_database(database.clone());
        let contract = engine.portable_contract()?;
        let types = contract
            .description()
            .types
            .iter()
            .map(|t| (t.id.as_str(), t))
            .collect();
        for (index, query) in self.queries.iter().enumerate() {
            let result = engine.describe_query(&query.source);
            let file = &mut self.files[index];
            file.valid = result.is_ok();
            if self.first {
                file.current_valid = file.valid;
            }
            match result {
                Ok(description) => {
                    let signature = signature(&description, &types)?;
                    if self.first {
                        self.baselines[index] = Some(signature.clone());
                    }
                    file.parameters_changed = self.baselines[index]
                        .as_ref()
                        .map(|base| base.parameters != signature.parameters);
                    file.result_changed = self.baselines[index]
                        .as_ref()
                        .map(|base| base.result != signature.result);
                    file.compatibility = if file.parameters_changed == Some(false)
                        && file.result_changed == Some(false)
                    {
                        CompatibilityLevel::Compatible
                    } else {
                        CompatibilityLevel::Conditional
                    };
                }
                Err(error) => {
                    self.diagnostics += 1;
                    if self.diagnostics > MAX_QUERY_DIAGNOSTICS {
                        return Err(Error::new(
                            "E_LIMIT",
                            "saved-query diagnostic budget exceeded",
                        ));
                    }
                    file.parameters_changed = None;
                    file.result_changed = None;
                    file.compatibility = CompatibilityLevel::Incompatible;
                    let failure = QueryCheckpointFailure {
                        migration_id: migration.map(str::to_owned),
                        schema: engine.schema_info(),
                        error,
                    };
                    self.diagnostic_bytes = self
                        .diagnostic_bytes
                        .saturating_add(encoded_report_len(&failure)?);
                    if self.diagnostic_bytes > MAX_QUERY_REPORT_BYTES {
                        return Err(Error::new("E_LIMIT", "saved-query report exceeds 1 MiB"));
                    }
                    file.failures.push(failure);
                }
            }
        }
        self.first = false;
        Ok(())
    }

    pub(crate) fn finish(self, plan: &MigrationPlan) -> Result<QueryValidation> {
        let report = QueryValidation {
            version: 1,
            current_schema: plan.current_schema.clone(),
            target_schema: plan.target_schema.clone(),
            checked_files: self.files.len(),
            valid: self.files.iter().all(|f| f.valid),
            files: self.files,
        };
        check_report_size(&report)?;
        Ok(report)
    }
}

fn signature(
    query: &QueryDescription,
    types: &BTreeMap<&str, &TypeDescription>,
) -> Result<Signature> {
    Ok(Signature {
        parameters: fingerprint(
            &query.parameters,
            query.parameters.iter().map(|p| &p.shape),
            types,
        )?,
        result: fingerprint(
            &query.result,
            query.result.fields.iter().map(|f| &f.shape),
            types,
        )?,
    })
}

/// Include reachable named definitions exactly once, including recursive ADTs.
/// Comparing bare Ref IDs would miss payload changes under a stable type ID.
fn fingerprint<'a>(
    root: &impl Serialize,
    shapes: impl Iterator<Item = &'a TypeShape>,
    types: &BTreeMap<&str, &TypeDescription>,
) -> Result<[u8; 32]> {
    let mut pending = BTreeSet::new();
    for shape in shapes {
        collect_refs(shape, &mut pending);
    }
    let mut selected = BTreeMap::new();
    while let Some(id) = pending.pop_first() {
        if selected.contains_key(&id) {
            continue;
        }
        let definition = types.get(id.as_str()).ok_or_else(|| {
            Error::new(
                "E_MIGRATION",
                "query shape references a missing catalog type",
            )
        })?;
        collect_refs(&definition.shape, &mut pending);
        selected.insert(id, *definition);
    }
    let mut hash = HashWriter(Sha256::new());
    serde_json::to_writer(&mut hash, &(root, selected))
        .map_err(|e| Error::new("E_MIGRATION", format!("encode query contract: {e}")))?;
    Ok(hash.0.finalize().into())
}

fn collect_refs(shape: &TypeShape, refs: &mut BTreeSet<String>) {
    match shape {
        TypeShape::Ref { type_id, .. } => {
            refs.insert(type_id.clone());
        }
        TypeShape::Sum { variants } => {
            for variant in variants {
                for shape in &variant.payload {
                    collect_refs(shape, refs);
                }
            }
        }
        TypeShape::Record { fields } => {
            for field in fields {
                collect_refs(&field.shape, refs);
            }
        }
        TypeShape::Tuple { items } => {
            for item in items {
                collect_refs(item, refs);
            }
        }
        TypeShape::Option { item } | TypeShape::List { item, .. } => collect_refs(item, refs),
        TypeShape::Map { key, value, .. } => {
            collect_refs(key, refs);
            collect_refs(value, refs);
        }
        _ => {}
    }
}

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ReportBudget(usize);
impl Write for ReportBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(io::Error::other("report budget exceeded"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn check_report_size(report: &impl Serialize) -> Result<()> {
    encoded_report_len(report).map(|_| ())
}

fn encoded_report_len(report: &impl Serialize) -> Result<usize> {
    let mut budget = ReportBudget(MAX_QUERY_REPORT_BYTES);
    serde_json::to_writer(&mut budget, report)
        .map_err(|_| Error::new("E_LIMIT", "saved-query report exceeds 1 MiB"))?;
    Ok(MAX_QUERY_REPORT_BYTES - budget.0)
}
