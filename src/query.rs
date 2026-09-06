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
    InsertParameter {
        table: String,
        parameter: String,
    },
    Upsert {
        table: String,
        values: Value,
    },
    UpsertParameter {
        table: String,
        parameter: String,
    },
    Update {
        target: Pipeline,
        assignments: Vec<SetAssignment>,
    },
    Delete {
        target: Pipeline,
    },
    Migration {
        name: String,
        parent: Option<String>,
        steps: Vec<SchemaMigration>,
    },
    Explain(Pipeline),
    Pipeline(Pipeline),
}

#[derive(Debug, Clone)]
pub enum SchemaMigration {
    AddType {
        name: String,
        ty: ScalarType,
    },
    DropType {
        name: String,
    },
    AddTable {
        table: String,
        row_type: String,
        key: Option<String>,
    },
    DropTable {
        table: String,
    },
    RenameTable {
        from: String,
        to: String,
    },
    RenameType {
        from: String,
        to: String,
    },
    AddField {
        owner: String,
        column: Column,
    },
    DropField {
        owner: String,
        field: String,
    },
    ChangeDefault {
        owner: String,
        field: String,
        value: Value,
    },
    DropDefault {
        owner: String,
        field: String,
    },
    RenameField {
        owner: String,
        from: String,
        to: String,
    },
    ChangeField {
        owner: String,
        field: String,
        ty: ScalarType,
        transform: MigrationTransform,
    },
    AddVariant {
        owner: String,
        name: String,
        args: Vec<ScalarType>,
    },
    DropVariant {
        owner: String,
        variant: String,
        transform: Option<MigrationTransform>,
    },
    RenameVariant {
        owner: String,
        from: String,
        to: String,
    },
    ChangeVariant {
        owner: String,
        variant: String,
        args: Vec<ScalarType>,
        transform: MigrationTransform,
    },
    AddIndex {
        table: String,
        column: String,
    },
    DropIndex {
        table: String,
        column: String,
    },
    SetKey {
        table: String,
        column: String,
    },
    DropKey {
        table: String,
    },
}

#[derive(Debug, Clone)]
pub struct MigrationTransform {
    pub binding: String,
    pub value: MatchValue,
}

#[derive(Debug, Clone)]
pub struct SetAssignment {
    pub path: String,
    pub value: ScalarExpression,
}

impl Statement {
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Self::Explain(_) | Self::Pipeline(_))
    }

    pub fn changes_schema(&self) -> bool {
        matches!(
            self,
            Self::DefineType { .. }
                | Self::CreateTable { .. }
                | Self::TypedTable { .. }
                | Self::CreateIndex { .. }
                | Self::Migration { .. }
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
    Let(LocalBinding),
    Filter(BoolExpression),
    FilterMatch(MatchPredicate),
    Derive(DeriveExpression),
    DeriveMatch(DeriveMatch),
    Aggregate(Aggregate),
    Select(Vec<String>),
    Sort(Vec<SortKey>),
    Take { offset: usize, limit: usize },
}

#[derive(Debug, Clone)]
pub struct Aggregate {
    pub group_by: Vec<String>,
    pub assignments: Vec<AggregateAssignment>,
}

#[derive(Debug, Clone)]
pub struct AggregateAssignment {
    pub name: String,
    pub function: AggregateFunction,
    pub input: Option<ScalarExpression>,
    pub output_type: Option<ScalarType>,
}

#[derive(Debug, Clone, Copy)]
pub enum AggregateFunction {
    Count,
    Sum,
    Min,
    Max,
}

#[derive(Debug, Clone)]
pub struct LocalBinding {
    pub name: String,
    pub span: Span,
    pub annotation: Option<ScalarType>,
    pub parameters: Vec<LocalParameter>,
    pub expression: BoolExpression,
}

#[derive(Debug, Clone)]
pub struct LocalParameter {
    pub name: String,
    pub annotation: Option<ScalarType>,
}

#[derive(Debug, Clone)]
pub struct DeriveExpression {
    pub name: String,
    pub expression: BoolExpression,
    pub output_type: Option<ScalarType>,
}

#[derive(Debug, Clone)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub struct MatchPredicate {
    pub column: String,
    pub arms: Vec<MatchArm>,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub condition: BoolExpression,
}

#[derive(Debug, Clone)]
pub enum MatchPattern {
    Wildcard,
    Binding(String),
    Constructor {
        name: String,
        payload: MatchPayload,
        tag: Option<MatchTag>,
    },
    Record {
        fields: Vec<MatchField>,
        rest: bool,
    },
    Tuple(Vec<MatchPattern>),
}

#[derive(Debug, Clone)]
pub enum MatchPayload {
    Unit,
    Record { fields: Vec<MatchField>, rest: bool },
    Positional(Vec<MatchPattern>),
}

#[derive(Debug, Clone)]
pub struct MatchField {
    pub field: String,
    pub pattern: MatchPattern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTag {
    Variant(u64),
    None,
    Some,
}

#[derive(Debug, Clone)]
pub struct DeriveMatch {
    pub name: String,
    pub source: String,
    pub arms: Vec<MatchValueArm>,
    pub output_type: Option<ScalarType>,
}

#[derive(Debug, Clone)]
pub struct MatchValueArm {
    pub pattern: MatchPattern,
    pub result: MatchValue,
}

#[derive(Debug, Clone)]
pub enum MatchValue {
    Binding(String),
    Literal(Value),
    Expression(ScalarExpression),
    Constructor {
        name: String,
        payload: MatchValuePayload,
    },
    Record(Vec<MatchValueField>),
    Tuple(Vec<MatchValue>),
    List(Vec<MatchValue>),
}

#[derive(Debug, Clone)]
pub enum MatchValuePayload {
    Unit,
    Record(Vec<MatchValueField>),
    Positional(Vec<MatchValue>),
}

#[derive(Debug, Clone)]
pub struct MatchValueField {
    pub name: String,
    pub value: MatchValue,
}

#[derive(Debug, Clone)]
pub enum BoolExpression {
    Value(ScalarExpression),
    Compare {
        left: ScalarExpression,
        op: CmpOp,
        right: ScalarExpression,
    },
    Contains {
        collection: ScalarExpression,
        item: ScalarExpression,
    },
    Any {
        collection: ScalarExpression,
        binding: String,
        predicate: Box<BoolExpression>,
    },
    All {
        collection: ScalarExpression,
        binding: String,
        predicate: Box<BoolExpression>,
    },
    IsSome(ScalarExpression),
    IsNone(ScalarExpression),
    Not(Box<BoolExpression>),
    And(Box<BoolExpression>, Box<BoolExpression>),
    Or(Box<BoolExpression>, Box<BoolExpression>),
}

#[derive(Debug, Clone)]
pub enum ScalarExpression {
    Reference(String),
    Parameter {
        name: String,
        ty: Option<ScalarType>,
    },
    Literal(Value),
    Ascribed {
        value: Box<ScalarExpression>,
        ty: ScalarType,
    },
    Call {
        name: String,
        arguments: Vec<ScalarExpression>,
        span: Span,
    },
    Length(Box<ScalarExpression>),
    Negate {
        value: Box<ScalarExpression>,
        ty: Option<ScalarType>,
    },
    Arithmetic {
        left: Box<ScalarExpression>,
        op: ArithmeticOp,
        right: Box<ScalarExpression>,
        ty: Option<ScalarType>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum ArithmeticOp {
    Add,
    Subtract,
    Multiply,
    Divide,
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
