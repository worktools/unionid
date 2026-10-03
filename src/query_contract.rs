//! Versioned static-query descriptions built by the runtime parser and binder.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Engine;
use crate::engine::PreparedQuery;
use crate::error::{Error, Result};
use crate::portable::{PortableSchemaIdentity, TypeShape};
use crate::query::{Pipeline, SetOperator, Stage, Statement};

pub const QUERY_DESCRIPTION_VERSION: u32 = 2;

/// A single-operation query file after schema-aware offline binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryDescription {
    /// Query-description format version.
    pub version: u32,
    /// Exact schema identity used for binding.
    pub schema: PortableSchemaIdentity,
    /// SHA-256 of `canonical_source`.
    pub query_digest: String,
    /// Operation performed by this query file.
    pub operation: QueryOperation,
    /// Formatter-canonical, semicolon-free source.
    pub canonical_source: String,
    /// Parameters in deterministic name order.
    pub parameters: Vec<QueryParameterDescription>,
    /// Typed fields and successful-execution row guarantee.
    pub result: QueryResultDescription,
    /// Potential checks of the described mutation; explain never evaluates them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_checks: Vec<QueryReferenceCheck>,
}

/// Prepared operation families supported by static query files.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryOperation {
    Read,
    Explain,
    Insert,
    InsertMany,
    Upsert,
    UpsertMany,
    Update,
    Delete,
}

/// A schema-bound check that a mutation may require at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryReferenceCheck {
    pub kind: QueryReferenceCheckKind,
    pub source_table_id: String,
    pub source_table: String,
    /// Reuse portable reference identity, paths, modes and pinned target key.
    pub reference: crate::portable::ReferenceDescription,
}

/// Checks describe potential obligations, not a prediction that a write fails.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryReferenceCheckKind {
    TargetExists,
    Restrict,
}

/// One inferred external parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryParameterDescription {
    /// Parameter name without the `$` prefix.
    pub name: String,
    /// Lossless portable value shape.
    pub shape: TypeShape,
    /// Table input fields that may be absent, separate from ADT Option values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omittable_fields: Vec<String>,
}

/// The row and mutation metadata produced by a successful operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResultDescription {
    /// Conservative number of returned rows.
    pub cardinality: QueryCardinality,
    /// Ordered row fields, empty when no rows are returned.
    pub fields: Vec<QueryFieldDescription>,
    /// Whether the response carries affected-row metadata.
    pub affected_rows: bool,
}

/// One ordered result-row field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryFieldDescription {
    /// Query-visible field or field-path name.
    pub name: String,
    /// Lossless portable value shape.
    pub shape: TypeShape,
}

/// Conservative successful-execution row guarantees for generated APIs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryCardinality {
    None,
    ExactlyOne,
    AtMostOne,
    Many,
}

/// Describe one static query against one declarative schema file.
pub fn describe(schema_source: &str, query_source: &str) -> Result<QueryDescription> {
    let database = crate::schema::parse(schema_source)?;
    Engine::from_database(database).describe_query(query_source)
}

impl Engine {
    /// Bind and describe one static operation without reading or mutating rows.
    pub fn describe_query(&self, source: &str) -> Result<QueryDescription> {
        let prepared = self.prepare(source)?;
        describe_prepared(self, &prepared)
    }
}

fn describe_prepared(engine: &Engine, prepared: &PreparedQuery) -> Result<QueryDescription> {
    let located = match prepared.statements() {
        [located] => Some(located),
        [located, guard]
            if crate::script::is_dml(&located.statement)
                && matches!(guard.statement, Statement::Expect { .. }) =>
        {
            Some(located)
        }
        _ => None,
    };
    let Some(located) = located else {
        let mut error = Error::new(
            "E_QUERY_FILE",
            "a static query file must contain one prepared operation with an optional trailing expect",
        );
        if let Some(second) = prepared.statements().get(1) {
            error = error.at(second.span);
        }
        return Err(error);
    };
    let canonical_source = crate::format_source(prepared.source())?;
    let query_digest = format!("sha256:{:x}", Sha256::digest(canonical_source.as_bytes()));
    let catalog = engine.catalog();
    let parameters = prepared
        .parameters()
        .iter()
        .map(|name| {
            let ty = prepared.parameter_type_ir().get(name).ok_or_else(|| {
                Error::new(
                    "E_QUERY_CONTRACT",
                    format!("bound parameter '${name}' has no inferred type"),
                )
            })?;
            let mut shape = crate::portable::describe_type(catalog, ty)?;
            let mut omittable_fields = Vec::new();
            let input_statement = match &located.statement {
                Statement::ExplainMutation(statement) => statement.as_ref(),
                statement => statement,
            };
            let input = match input_statement {
                Statement::InsertParameter {
                    table, parameter, ..
                }
                | Statement::InsertManyParameter {
                    table, parameter, ..
                } if parameter == name => Some((table.as_str(), false)),
                Statement::UpsertParameter {
                    table, parameter, ..
                }
                | Statement::UpsertManyParameter {
                    table, parameter, ..
                } if parameter == name => Some((table.as_str(), true)),
                _ => None,
            };
            if let Some((table, upsert)) = input
                && let Some((row, fields)) = engine.generated_input_shape(table, upsert)?
            {
                shape = match shape {
                    TypeShape::List { max_items, .. } => TypeShape::List {
                        item: Box::new(row),
                        max_items,
                    },
                    _ => row,
                };
                omittable_fields = fields;
            }
            Ok(QueryParameterDescription {
                name: name.clone(),
                shape,
                omittable_fields,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let fields = prepared
        .response_columns()
        .iter()
        .map(|column| {
            Ok(QueryFieldDescription {
                name: column.name.clone(),
                shape: crate::portable::describe_type(catalog, &column.ty)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let operation = operation(&located.statement);
    Ok(QueryDescription {
        version: QUERY_DESCRIPTION_VERSION,
        schema: prepared.schema().clone().into(),
        query_digest,
        operation,
        canonical_source,
        parameters,
        reference_checks: reference_checks(engine, &located.statement),
        result: QueryResultDescription {
            cardinality: result_cardinality(&located.statement, !fields.is_empty()),
            fields,
            affected_rows: !matches!(operation, QueryOperation::Read | QueryOperation::Explain),
        },
    })
}

fn reference_checks(engine: &Engine, statement: &Statement) -> Vec<QueryReferenceCheck> {
    if !crate::script::is_dml(statement) && !matches!(statement, Statement::ExplainMutation(_)) {
        return Vec::new();
    }
    reference_checks_from(engine.reference_descriptions(), statement)
}

pub(crate) fn reference_checks_from(
    descriptions: Vec<(String, String, crate::portable::ReferenceDescription)>,
    statement: &Statement,
) -> Vec<QueryReferenceCheck> {
    if let Statement::ExplainMutation(mutation) = statement {
        return reference_checks_from(descriptions, mutation);
    }
    let (table, outgoing, incoming) = match statement {
        Statement::Insert { table, .. }
        | Statement::InsertMany { table, .. }
        | Statement::InsertParameter { table, .. }
        | Statement::InsertManyParameter { table, .. } => (table.as_str(), true, false),
        Statement::Upsert { table, .. }
        | Statement::UpsertMany { table, .. }
        | Statement::UpsertParameter { table, .. }
        | Statement::UpsertManyParameter { table, .. } => (table.as_str(), true, true),
        Statement::Update { target, .. } => (target.from.as_str(), true, true),
        Statement::Delete { target, .. } => (target.from.as_str(), false, true),
        _ => return Vec::new(),
    };
    let mut checks = Vec::new();
    for (source_table_id, source_table, reference) in descriptions {
        for (included, kind) in [
            (
                outgoing && source_table == table,
                QueryReferenceCheckKind::TargetExists,
            ),
            (
                incoming && reference.target_table == table,
                QueryReferenceCheckKind::Restrict,
            ),
        ] {
            if included {
                checks.push(QueryReferenceCheck {
                    kind,
                    source_table_id: source_table_id.clone(),
                    source_table: source_table.clone(),
                    reference: reference.clone(),
                });
            }
        }
    }
    checks
}

fn operation(statement: &Statement) -> QueryOperation {
    match statement {
        Statement::Pipeline(_) => QueryOperation::Read,
        Statement::Explain(_) | Statement::ExplainMutation(_) | Statement::ExplainAnalyze(_) => {
            QueryOperation::Explain
        }
        Statement::InsertParameter { .. } => QueryOperation::Insert,
        Statement::InsertManyParameter { .. } => QueryOperation::InsertMany,
        Statement::UpsertParameter { .. } => QueryOperation::Upsert,
        Statement::UpsertManyParameter { .. } => QueryOperation::UpsertMany,
        Statement::Update { .. } => QueryOperation::Update,
        Statement::Delete { .. } => QueryOperation::Delete,
        _ => unreachable!("Engine::prepare rejects unsupported static operations"),
    }
}

fn result_cardinality(statement: &Statement, has_fields: bool) -> QueryCardinality {
    if !has_fields {
        return QueryCardinality::None;
    }
    match statement {
        Statement::Pipeline(pipeline) => pipeline_cardinality(pipeline),
        Statement::InsertParameter { .. } | Statement::UpsertParameter { .. } => {
            QueryCardinality::ExactlyOne
        }
        Statement::InsertManyParameter { .. } | Statement::UpsertManyParameter { .. } => {
            QueryCardinality::Many
        }
        Statement::Update { target, .. } | Statement::Delete { target, .. } => {
            pipeline_cardinality(target)
        }
        Statement::Explain(_) | Statement::ExplainMutation(_) | Statement::ExplainAnalyze(_) => {
            QueryCardinality::None
        }
        _ => QueryCardinality::None,
    }
}

fn pipeline_cardinality(pipeline: &Pipeline) -> QueryCardinality {
    let mut cardinality = QueryCardinality::Many;
    for stage in &pipeline.stages {
        match stage {
            Stage::Aggregate(aggregate) => {
                cardinality = if aggregate.group_by.is_empty() {
                    QueryCardinality::ExactlyOne
                } else {
                    QueryCardinality::Many
                };
            }
            Stage::Filter(_) | Stage::FilterMatch(_)
                if cardinality == QueryCardinality::ExactlyOne =>
            {
                cardinality = QueryCardinality::AtMostOne;
            }
            Stage::Take { offset, limit } => {
                cardinality = bounded_cardinality(cardinality, *offset, *limit);
            }
            Stage::Page(page) => {
                cardinality = bounded_cardinality(cardinality, 0, page.limit);
            }
            Stage::SetOperation(operation) => {
                cardinality = match operation.operator {
                    SetOperator::Union => QueryCardinality::Many,
                    SetOperator::Intersect | SetOperator::Except
                        if cardinality == QueryCardinality::ExactlyOne =>
                    {
                        QueryCardinality::AtMostOne
                    }
                    SetOperator::Intersect | SetOperator::Except => cardinality,
                };
            }
            _ => {}
        }
    }
    cardinality
}

fn bounded_cardinality(current: QueryCardinality, offset: usize, limit: usize) -> QueryCardinality {
    if limit == 0 {
        QueryCardinality::None
    } else if limit == 1 {
        if current == QueryCardinality::ExactlyOne && offset == 0 {
            QueryCardinality::ExactlyOne
        } else {
            QueryCardinality::AtMostOne
        }
    } else if offset > 0 && current == QueryCardinality::ExactlyOne {
        QueryCardinality::AtMostOne
    } else {
        current
    }
}
