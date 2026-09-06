use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationEntry {
    pub id: String,
    pub parent: Option<String>,
    pub checksum: String,
    pub schema_revision: u64,
    pub schema_hash: String,
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
