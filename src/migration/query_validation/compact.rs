//! Version-2 reports retain every failed checkpoint through lossless intervals.
use serde::{Deserialize, Serialize};

use super::{MAX_QUERY_DIAGNOSTICS, MAX_QUERY_REPORT_BYTES, encoded_report_len};
use crate::db::SchemaInfo;
use crate::error::{Error, Result};
use crate::migration::{MigrationApply, MigrationPlan};
use crate::portable::CompatibilityLevel;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryCheckpoint {
    /// None is the current committed schema, before pending migrations.
    pub migration_id: Option<String>,
    pub schema: SchemaInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryFailureInterval {
    /// Inclusive zero-based indices into `QueryValidationV2::checkpoints`.
    pub first_checkpoint: usize,
    pub last_checkpoint: usize,
    /// All checkpoints in this interval failed with this complete error value.
    pub error: Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryFileValidationV2 {
    pub path: String,
    pub valid: bool,
    pub current_valid: bool,
    pub compatibility: CompatibilityLevel,
    /// None when the current or target query cannot be bound.
    pub parameters_changed: Option<bool>,
    pub result_changed: Option<bool>,
    pub failures: Vec<QueryFailureInterval>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryValidationV2 {
    pub version: u32,
    pub current_schema: SchemaInfo,
    pub target_schema: SchemaInfo,
    pub checked_files: usize,
    pub valid: bool,
    /// Ordered current schema followed by each pending migration's schema.
    pub checkpoints: Vec<QueryCheckpoint>,
    pub files: Vec<QueryFileValidationV2>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationQueryPlanV2 {
    #[serde(flatten)]
    pub plan: MigrationPlan,
    pub query_validation: QueryValidationV2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationQueryApplyV2 {
    #[serde(flatten)]
    pub applied: MigrationApply,
    pub query_validation: QueryValidationV2,
}

/// Preserve a compact report after preflight rejection or a later apply error.
#[derive(Debug)]
pub struct MigrationQueryErrorV2 {
    pub error: Box<Error>,
    pub query_validation: Option<Box<QueryValidationV2>>,
}

impl From<Error> for MigrationQueryErrorV2 {
    fn from(error: Error) -> Self {
        Self {
            error: Box::new(error),
            query_validation: None,
        }
    }
}
impl std::fmt::Display for MigrationQueryErrorV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for MigrationQueryErrorV2 {}

pub(super) struct CompactDiagnostics {
    pub checkpoints: Vec<QueryCheckpoint>,
    pub failures: Vec<Vec<QueryFailureInterval>>,
    intervals: usize,
    bytes: usize,
}

impl CompactDiagnostics {
    pub fn new(files: usize) -> Self {
        Self {
            checkpoints: Vec::new(),
            failures: vec![Vec::new(); files],
            intervals: 0,
            bytes: 0,
        }
    }

    pub fn checkpoint(&mut self, migration: Option<&str>, schema: SchemaInfo) -> Result<()> {
        let checkpoint = QueryCheckpoint {
            migration_id: migration.map(str::to_owned),
            schema,
        };
        self.charge(encoded_report_len(&checkpoint)?)?;
        self.checkpoints.push(checkpoint);
        Ok(())
    }

    pub fn failure(&mut self, file: usize, error: Error) -> Result<()> {
        let checkpoint = self.checkpoints.len() - 1;
        if let Some(previous) = self.failures[file].last()
            && previous.last_checkpoint + 1 == checkpoint
            && previous.error == error
        {
            // Charge changes in integer encoding length, not the entire repeated error.
            let added_digits = checkpoint.checked_ilog10().unwrap_or(0)
                - previous.last_checkpoint.checked_ilog10().unwrap_or(0);
            self.charge(added_digits as usize)?;
            self.failures[file].last_mut().unwrap().last_checkpoint = checkpoint;
            return Ok(());
        }
        if self.intervals == MAX_QUERY_DIAGNOSTICS {
            return Err(Error::new(
                "E_LIMIT",
                "saved-query diagnostic interval budget exceeded",
            ));
        }
        let failure = QueryFailureInterval {
            first_checkpoint: checkpoint,
            last_checkpoint: checkpoint,
            error,
        };
        self.charge(encoded_report_len(&failure)?)?;
        self.intervals += 1;
        self.failures[file].push(failure);
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > MAX_QUERY_REPORT_BYTES {
            return Err(Error::new("E_LIMIT", "saved-query report exceeds 1 MiB"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;

    fn schema(revision: u64) -> SchemaInfo {
        SchemaInfo {
            revision,
            hash: "test".into(),
        }
    }

    #[test]
    fn intervals_require_consecutive_complete_errors_and_account_for_index_digits() {
        let mut collector = CompactDiagnostics::new(1);
        let error = Error::new("E_FIELD", "missing field").at(Span { line: 1, column: 2 });
        for checkpoint in 0..=10 {
            collector.checkpoint(None, schema(checkpoint)).unwrap();
            collector.failure(0, error.clone()).unwrap();
        }
        assert_eq!(collector.failures[0].len(), 1);
        assert_eq!(collector.failures[0][0].last_checkpoint, 10);
        // A successful checkpoint has no failure; the same later error starts a new interval.
        collector.checkpoint(None, schema(11)).unwrap();
        collector.checkpoint(None, schema(12)).unwrap();
        collector.failure(0, error.clone()).unwrap();
        let changed = [
            Error::new("E_FIELD", "different field").at(Span { line: 1, column: 2 }),
            Error::new("E_FIELD", "missing field").at(Span { line: 2, column: 2 }),
            error.clone().with_hint("change query"),
            error.clone().at_statement(2),
            Error::constraint(
                Error::new("E_FIELD", "missing field"),
                crate::error::ConstraintKind::Unique,
            ),
        ];
        for (index, error) in changed.into_iter().enumerate() {
            collector
                .checkpoint(None, schema(index as u64 + 13))
                .unwrap();
            collector.failure(0, error).unwrap();
        }
        assert_eq!(collector.intervals, 7);
        assert_eq!(collector.failures[0][1].first_checkpoint, 12);
        let encoded = collector
            .checkpoints
            .iter()
            .map(|c| encoded_report_len(c).unwrap())
            .sum::<usize>()
            + collector.failures[0]
                .iter()
                .map(|f| encoded_report_len(f).unwrap())
                .sum::<usize>();
        assert_eq!(collector.bytes, encoded);
    }

    #[test]
    fn distinct_intervals_and_large_metadata_remain_bounded() {
        let mut collector = CompactDiagnostics::new(1);
        for index in 0..MAX_QUERY_DIAGNOSTICS {
            collector.checkpoint(None, schema(index as u64)).unwrap();
            collector
                .failure(0, Error::new("E_FIELD", format!("field {}", index % 2)))
                .unwrap();
        }
        collector
            .checkpoint(None, schema(MAX_QUERY_DIAGNOSTICS as u64))
            .unwrap();
        let error = collector
            .failure(0, Error::new("E_FIELD", "another field"))
            .unwrap_err();
        assert_eq!(error.code, "E_LIMIT");
        assert!(error.message.contains("interval"));
        assert_eq!(collector.intervals, MAX_QUERY_DIAGNOSTICS);

        let mut collector = CompactDiagnostics::new(1);
        let oversized = SchemaInfo {
            revision: 0,
            hash: "x".repeat(MAX_QUERY_REPORT_BYTES),
        };
        assert_eq!(
            collector.checkpoint(None, oversized).unwrap_err().code,
            "E_LIMIT"
        );
        assert!(collector.checkpoints.is_empty());
        let mut collector = CompactDiagnostics::new(1);
        collector.checkpoint(None, schema(0)).unwrap();
        assert_eq!(
            collector
                .failure(0, Error::new("E_FIELD", "x".repeat(MAX_QUERY_REPORT_BYTES)))
                .unwrap_err()
                .code,
            "E_LIMIT"
        );
        assert!(collector.failures[0].is_empty());
    }
}
