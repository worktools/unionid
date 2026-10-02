//! Bounded, value-free script results and mutation guard preflight.
use serde::{Deserialize, Serialize};

use crate::{
    Error,
    query::{CmpOp, LocatedStatement, Statement},
};

pub const MAX_SCRIPT_STATEMENTS: usize = 4_096;
pub const MAX_STATEMENT_SUMMARY_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    DefineType,
    CreateTable,
    Table,
    CreateIndex,
    CreateReference,
    DropReference,
    Insert,
    InsertMany,
    Upsert,
    UpsertMany,
    Update,
    Delete,
    Migration,
    Explain,
    ExplainAnalyze,
    Query,
    Expect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementSummary {
    pub index: usize,
    pub kind: StatementKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<usize>,
}

pub(crate) fn kind(statement: &Statement) -> StatementKind {
    match statement {
        Statement::DefineType { .. } => StatementKind::DefineType,
        Statement::CreateTable { .. } => StatementKind::CreateTable,
        Statement::TypedTable { .. } => StatementKind::Table,
        Statement::CreateIndex { .. } => StatementKind::CreateIndex,
        Statement::CreateReference(_) => StatementKind::CreateReference,
        Statement::DropReference(_) => StatementKind::DropReference,
        Statement::Insert { .. } | Statement::InsertParameter { .. } => StatementKind::Insert,
        Statement::InsertMany { .. } | Statement::InsertManyParameter { .. } => {
            StatementKind::InsertMany
        }
        Statement::Upsert { .. } | Statement::UpsertParameter { .. } => StatementKind::Upsert,
        Statement::UpsertMany { .. } | Statement::UpsertManyParameter { .. } => {
            StatementKind::UpsertMany
        }
        Statement::Update { .. } => StatementKind::Update,
        Statement::Delete { .. } => StatementKind::Delete,
        Statement::Migration { .. } => StatementKind::Migration,
        Statement::Explain(_) => StatementKind::Explain,
        Statement::ExplainAnalyze(_) => StatementKind::ExplainAnalyze,
        Statement::Pipeline(_) => StatementKind::Query,
        Statement::Expect { .. } => StatementKind::Expect,
    }
}

pub(crate) fn is_dml(statement: &Statement) -> bool {
    matches!(
        kind(statement),
        StatementKind::Insert
            | StatementKind::InsertMany
            | StatementKind::Upsert
            | StatementKind::UpsertMany
            | StatementKind::Update
            | StatementKind::Delete
    )
}

pub(crate) fn preflight(statements: &[LocatedStatement]) -> Result<(), Error> {
    if statements.len() > MAX_SCRIPT_STATEMENTS {
        return Err(Error::new(
            "E_LIMIT",
            "script exceeds the 4096 statement limit",
        ));
    }
    for (index, located) in statements.iter().enumerate() {
        if matches!(located.statement, Statement::Expect { .. })
            && !index
                .checked_sub(1)
                .is_some_and(|previous| is_dml(&statements[previous].statement))
        {
            return Err(Error::new(
                "E_EXPECTATION_CONTEXT",
                "expect must immediately follow a mutation",
            )
            .at(located.span)
            .at_statement(index + 1));
        }
    }
    Ok(())
}

pub(crate) fn satisfies(actual: usize, op: CmpOp, expected: u64) -> bool {
    let actual = actual as u128;
    let expected = u128::from(expected);
    match op {
        CmpOp::Eq => actual == expected,
        CmpOp::Ne => actual != expected,
        CmpOp::Gt => actual > expected,
        CmpOp::Gte => actual >= expected,
        CmpOp::Lt => actual < expected,
        CmpOp::Lte => actual <= expected,
    }
}

pub(crate) fn validate_summaries(summaries: &[StatementSummary]) -> Result<(), Error> {
    if summaries.len() > MAX_SCRIPT_STATEMENTS
        || summaries.iter().enumerate().any(|(i, s)| {
            s.index != i + 1
                || s.affected_rows.is_some()
                    != matches!(
                        s.kind,
                        StatementKind::Insert
                            | StatementKind::InsertMany
                            | StatementKind::Upsert
                            | StatementKind::UpsertMany
                            | StatementKind::Update
                            | StatementKind::Delete
                    )
                || (s.kind == StatementKind::Expect
                    && i.checked_sub(1)
                        .is_none_or(|previous| summaries[previous].affected_rows.is_none()))
        })
    {
        return Err(Error::new(
            "E_LIMIT",
            "invalid or excessive statement summaries",
        ));
    }
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|total| *total <= MAX_STATEMENT_SUMMARY_BYTES)
                .ok_or_else(|| std::io::Error::other("statement summary budget exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(0), summaries)
        .map_err(|_| Error::new("E_LIMIT", "statement summaries exceed the 512 KiB budget"))
}
