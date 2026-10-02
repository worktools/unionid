//! Bounded, value-free script results and mutation guard preflight.
use serde::{Deserialize, Serialize};

use crate::{
    Error,
    query::{CmpOp, LocatedStatement, Statement},
};

pub const MAX_SCRIPT_STATEMENTS: usize = 4_096;
/// Conservative JSON size bound implied by the statement count and fixed fields.
/// Kept for Rust callers and the agent manifest; no separate encoding pass is needed.
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
        Statement::Explain(_) | Statement::ExplainMutation(_) => StatementKind::Explain,
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
    // With <=4096 entries, fixed kinds, indices and usize counts, the complete
    // JSON stays below MAX_STATEMENT_SUMMARY_BYTES. Tests cover the largest
    // valid shape; receipt encoding still checks its complete byte budget.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_summary_shapes_imply_the_public_byte_bound() {
        use StatementKind::*;
        for kind in [
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
        ] {
            let summaries: Vec<_> = (1..=MAX_SCRIPT_STATEMENTS)
                .map(|index| {
                    // An expect has a preceding mutation, including at maximum count.
                    let kind = if kind == Expect && index % 2 == 1 {
                        Update
                    } else {
                        kind
                    };
                    let affected_rows = match kind {
                        Insert | InsertMany | Upsert | UpsertMany | Update | Delete => {
                            Some(usize::MAX)
                        }
                        DefineType | CreateTable | Table | CreateIndex | CreateReference
                        | DropReference | Migration | Explain | ExplainAnalyze | Query | Expect => {
                            None
                        }
                    };
                    StatementSummary {
                        index,
                        kind,
                        affected_rows,
                    }
                })
                .collect();
            validate_summaries(&summaries).unwrap();
            let encoded = serde_json::to_vec(&summaries).unwrap();
            assert!(
                encoded.len() < MAX_STATEMENT_SUMMARY_BYTES,
                "{kind:?}: {} bytes",
                encoded.len()
            );
        }
    }
}
