use crate::error::Span;
use crate::model::{Column, ScalarType, Value};

#[derive(Debug, Clone)]
pub enum Statement {
    DefineType {
        name: String,
        ty: ScalarType,
    },
    CreateTable {
        table: String,
        columns: Vec<Column>,
    },
    TypedTable {
        table: String,
        row_type: String,
        key: Option<String>,
    },
    CreateIndex {
        table: String,
        column: String,
    },
    Insert {
        table: String,
        values: Value,
    },
    Pipeline(Pipeline),
}

impl Statement {
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Self::Pipeline(_))
    }

    pub fn changes_schema(&self) -> bool {
        matches!(
            self,
            Self::DefineType { .. }
                | Self::CreateTable { .. }
                | Self::TypedTable { .. }
                | Self::CreateIndex { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub struct LocatedStatement {
    pub statement: Statement,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub from: String,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone)]
pub enum Stage {
    Filter(Predicate),
    FilterMatch(MatchPredicate),
    Select(Vec<String>),
    Sort { column: String, descending: bool },
    Limit(usize),
}

#[derive(Debug, Clone)]
pub struct MatchPredicate {
    pub column: String,
    pub arms: Vec<MatchArm>,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub condition: MatchCondition,
}

#[derive(Debug, Clone)]
pub enum MatchPattern {
    Wildcard,
    Variant {
        name: String,
        fields: Vec<String>,
        record: bool,
        rest: bool,
        variant_id: Option<u64>,
    },
}

#[derive(Debug, Clone)]
pub enum MatchCondition {
    Bool(bool),
    Binding(String),
    Compare {
        binding: String,
        op: CmpOp,
        value: Value,
    },
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub column: String,
    pub op: CmpOp,
    pub value: Value,
}

#[derive(Debug, Clone, Copy)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

pub fn parse_statement(input: &str) -> Result<Statement, String> {
    let mut statements = crate::syntax::parse(input).map_err(|e| e.to_string())?;
    if statements.len() != 1 {
        return Err("expected exactly one statement".into());
    }
    Ok(statements.remove(0).statement)
}
