use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::SchemaInfo;
use crate::error::{Error, Result};
use crate::query::SchemaMigration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationEntry {
    pub id: String,
    pub parent: Option<String>,
    pub checksum: String,
    pub schema_revision: u64,
    pub schema_hash: String,
    #[serde(default)]
    pub applied_at_unix_ms: u64,
}

#[derive(Debug, Clone)]
pub struct MigrationFile {
    pub id: String,
    pub parent: Option<String>,
    pub checksum: String,
    pub source: String,
    pub steps: Vec<SchemaMigration>,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationPlan {
    pub current_schema: SchemaInfo,
    pub target_schema: SchemaInfo,
    pub applied_count: usize,
    pub pending: Vec<MigrationPlanItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationPlanItem {
    pub id: String,
    pub parent: Option<String>,
    pub checksum: String,
    pub before: SchemaInfo,
    pub after: SchemaInfo,
    pub operations: Vec<String>,
    pub destructive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationStatus {
    pub schema: SchemaInfo,
    pub applied: Vec<MigrationEntry>,
    pub pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MigrationMaintenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationMaintenancePhase {
    Building,
    Ready,
    Aborting,
    Reclaimable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationMaintenance {
    pub phase: MigrationMaintenancePhase,
    pub migration_id: String,
    pub source_generation: u64,
    pub target_generation: u64,
    pub source_rows_seen: u64,
    pub target_rows_written: u64,
    pub index_entries_written: u64,
    pub logical_bytes: u64,
    pub updated_at_unix_ms: u64,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationApply {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub schema: SchemaInfo,
}

/// Result of one deterministic, bounded format-6 maintenance call.
///
/// Each step is one successfully committed maintenance transaction: generation
/// creation, a row batch checkpoint, validation, cutover, or one reclamation
/// batch. Callers can inspect `status` and invoke the operation again with the
/// same migration files until `complete` becomes true.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationProgress {
    pub committed_steps: usize,
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub complete: bool,
    pub status: MigrationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationAbort {
    pub migration_id: Option<String>,
    pub cleaned: bool,
    pub schema: SchemaInfo,
}

impl MigrationFile {
    pub fn parse(source: impl Into<String>) -> Result<Self> {
        let source = source.into();
        let statements = crate::syntax::parse(&source)?;
        let [statement] = statements.as_slice() else {
            return Err(Error::new(
                "E_MIGRATION",
                "a migration file must contain exactly one migration block",
            ));
        };
        let crate::query::Statement::Migration {
            name,
            parent,
            steps,
        } = &statement.statement
        else {
            return Err(Error::new(
                "E_MIGRATION",
                "a migration file must contain exactly one migration block",
            ));
        };
        Ok(Self {
            id: name.clone(),
            parent: parent.clone(),
            checksum: checksum(&source),
            source,
            steps: steps.clone(),
            path: None,
        })
    }
}

pub fn load_directory(path: impl AsRef<Path>) -> Result<Vec<MigrationFile>> {
    let path = path.as_ref();
    let entries = std::fs::read_dir(path).map_err(|error| {
        Error::new(
            "E_IO",
            format!("read migration directory '{}': {error}", path.display()),
        )
    })?;
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| Error::new("E_IO", format!("read migration entry: {error}")))
        })
        .collect::<Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "uid"));
    paths.sort();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let source = std::fs::read_to_string(&path).map_err(|error| {
            Error::new(
                "E_IO",
                format!("read migration file '{}': {error}", path.display()),
            )
        })?;
        let mut file = MigrationFile::parse(source).map_err(|error| {
            Error::new(
                &error.code,
                format!("migration file '{}': {}", path.display(), error.message),
            )
        })?;
        file.path = Some(path);
        files.push(file);
    }
    validate_files(&files)?;
    Ok(files)
}

pub fn validate_files(files: &[MigrationFile]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut expected_parent: Option<&str> = None;
    for file in files {
        if !ids.insert(file.id.as_str()) {
            return Err(Error::new(
                "E_MIGRATION",
                format!("duplicate migration ID '{}'", file.id),
            ));
        }
        if file.parent.as_deref() != expected_parent {
            return Err(Error::new(
                "E_MIGRATION",
                format!(
                    "migration '{}' expects parent {:?}, file order requires {:?}",
                    file.id, file.parent, expected_parent
                ),
            ));
        }
        expected_parent = Some(&file.id);
    }
    Ok(())
}

pub fn validate_files_against_history(
    files: &[MigrationFile],
    history: &[MigrationEntry],
) -> Result<usize> {
    validate_files(files)?;
    validate_history(history)?;
    for (index, applied) in history.iter().enumerate() {
        let file = files.get(index).ok_or_else(|| {
            Error::new(
                "E_MIGRATION",
                format!("applied migration file '{}' is missing", applied.id),
            )
        })?;
        if file.id != applied.id
            || file.parent != applied.parent
            || file.checksum != applied.checksum
        {
            return Err(Error::new(
                "E_MIGRATION",
                format!("applied migration '{}' was changed", applied.id),
            ));
        }
    }
    Ok(history.len())
}

pub fn checksum(source: &str) -> String {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim_end_matches('\n');
    format!("sha256:{:x}", Sha256::digest(format!("{normalized}\n")))
}

pub fn describe_step(step: &SchemaMigration) -> (String, bool) {
    match step {
        SchemaMigration::AddType { name, .. } => (format!("add type {name}"), false),
        SchemaMigration::DropType { name } => (format!("drop type {name}"), true),
        SchemaMigration::AddTable {
            table,
            row_type,
            key,
        } => (
            format!(
                "add table {table} {row_type}{}",
                key.as_ref()
                    .map(|key| format!(" key {key}"))
                    .unwrap_or_default()
            ),
            false,
        ),
        SchemaMigration::DropTable { table } => (format!("drop table {table}"), true),
        SchemaMigration::RenameTable { from, to } => {
            (format!("rename table {from} to {to}"), false)
        }
        SchemaMigration::RenameType { from, to } => (format!("rename type {from} to {to}"), false),
        SchemaMigration::AddField { owner, column } => {
            (format!("add field {owner}.{}", column.name), false)
        }
        SchemaMigration::DropField { owner, field } => {
            (format!("drop field {owner}.{field}"), true)
        }
        SchemaMigration::ChangeDefault { owner, field, .. } => {
            (format!("change default {owner}.{field}"), false)
        }
        SchemaMigration::DropDefault { owner, field } => {
            (format!("drop default {owner}.{field}"), false)
        }
        SchemaMigration::RenameField { owner, from, to } => {
            (format!("rename field {owner}.{from} to {to}"), false)
        }
        SchemaMigration::ChangeField { owner, field, .. } => (
            format!("change field {owner}.{field} using conversion"),
            true,
        ),
        SchemaMigration::AddVariant { owner, name, .. } => {
            (format!("add variant {owner}.{name}"), false)
        }
        SchemaMigration::DropVariant {
            owner,
            variant,
            transform,
        } => (
            format!(
                "drop variant {owner}.{variant}{}",
                if transform.is_some() {
                    " using conversion"
                } else {
                    ""
                }
            ),
            true,
        ),
        SchemaMigration::RenameVariant { owner, from, to } => {
            (format!("rename variant {owner}.{from} to {to}"), false)
        }
        SchemaMigration::ChangeVariant { owner, variant, .. } => (
            format!("change variant {owner}.{variant} using conversion"),
            true,
        ),
        SchemaMigration::AddIndex {
            table,
            components,
            unique,
        } => (
            format!(
                "add {}index {table} ({})",
                if *unique { "unique " } else { "" },
                crate::formatter::index_shape(components),
            ),
            false,
        ),
        SchemaMigration::DropIndex { table, components } => (
            format!(
                "drop index {table} ({})",
                crate::formatter::index_shape(components)
            ),
            true,
        ),
        SchemaMigration::SetKey { table, column } => (format!("set key {table}.{column}"), false),
        SchemaMigration::DropKey { table } => (format!("drop key {table}"), true),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDecision {
    Apply,
    AlreadyApplied,
}

/// Validate the durable migration ledger as one connected, linear history.
/// Entries may be supplied in any order; the returned value is the head ID.
pub fn validate_history(entries: &[MigrationEntry]) -> Result<Option<&str>> {
    let mut by_id = BTreeMap::new();
    for entry in entries {
        validate_entry(entry)?;
        if by_id.insert(entry.id.as_str(), entry).is_some() {
            return Err(Error::new(
                "E_MIGRATION",
                format!("duplicate migration ID '{}'", entry.id),
            ));
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }

    let mut roots = Vec::new();
    let mut child_by_parent = BTreeMap::new();
    for entry in entries {
        match &entry.parent {
            None => roots.push(entry.id.as_str()),
            Some(parent) => {
                if !by_id.contains_key(parent.as_str()) {
                    return Err(Error::new(
                        "E_MIGRATION",
                        format!("migration '{}' has missing parent '{parent}'", entry.id),
                    ));
                }
                if let Some(previous) = child_by_parent.insert(parent.as_str(), entry.id.as_str()) {
                    return Err(Error::new(
                        "E_MIGRATION",
                        format!(
                            "migration history forks at '{parent}' into '{previous}' and '{}'",
                            entry.id
                        ),
                    ));
                }
            }
        }
    }
    if roots.len() != 1 {
        let message = if roots.is_empty() {
            "migration history has a cycle and no root".into()
        } else {
            format!("migration history has {} roots", roots.len())
        };
        return Err(Error::new("E_MIGRATION", message));
    }

    let mut seen = BTreeSet::new();
    let mut current = roots[0];
    loop {
        if !seen.insert(current) {
            return Err(Error::new(
                "E_MIGRATION",
                format!("migration history contains a cycle at '{current}'"),
            ));
        }
        let Some(child) = child_by_parent.get(current) else {
            break;
        };
        current = child;
    }
    if seen.len() != entries.len() {
        return Err(Error::new(
            "E_MIGRATION",
            "migration history contains a disconnected cycle",
        ));
    }
    Ok(Some(current))
}

pub fn validate_next(history: &[MigrationEntry], next: &MigrationEntry) -> Result<ApplyDecision> {
    let head = validate_history(history)?;
    validate_entry(next)?;
    if let Some(applied) = history.iter().find(|entry| entry.id == next.id) {
        if applied == next {
            return Ok(ApplyDecision::AlreadyApplied);
        }
        return Err(Error::new(
            "E_MIGRATION",
            format!("applied migration '{}' was changed", next.id),
        ));
    }
    if next.parent.as_deref() != head {
        return Err(Error::new(
            "E_MIGRATION",
            format!(
                "migration '{}' expects parent {:?}, current head is {:?}",
                next.id, next.parent, head
            ),
        ));
    }
    if let Some(previous) = head.and_then(|id| history.iter().find(|entry| entry.id == id))
        && next.schema_revision <= previous.schema_revision
    {
        return Err(Error::new(
            "E_MIGRATION",
            "migration schema revision must increase",
        ));
    }
    Ok(ApplyDecision::Apply)
}

fn validate_entry(entry: &MigrationEntry) -> Result<()> {
    if entry.id.trim().is_empty() {
        return Err(Error::new("E_MIGRATION", "migration ID must not be empty"));
    }
    if entry.checksum.trim().is_empty() {
        return Err(Error::new(
            "E_MIGRATION",
            format!("migration '{}' has an empty checksum", entry.id),
        ));
    }
    if !entry.schema_hash.starts_with("sha256:") {
        return Err(Error::new(
            "E_MIGRATION",
            format!("migration '{}' has an invalid schema hash", entry.id),
        ));
    }
    Ok(())
}
