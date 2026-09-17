//! Versioned static-query descriptions built by the runtime parser and binder.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Engine;
use crate::engine::PreparedQuery;
use crate::error::{Error, Result};
use crate::portable::{PortableSchemaIdentity, TypeShape};
use crate::query::{Pipeline, SetOperator, Stage, Statement};

pub const QUERY_DESCRIPTION_VERSION: u32 = 1;

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

/// One inferred external parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryParameterDescription {
    /// Parameter name without the `$` prefix.
    pub name: String,
    /// Lossless portable value shape.
    pub shape: TypeShape,
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
    let [located] = prepared.statements() else {
        let mut error = Error::new(
            "E_QUERY_FILE",
            "a static query file must contain exactly one prepared operation",
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
            Ok(QueryParameterDescription {
                name: name.clone(),
                shape: crate::portable::describe_type(catalog, ty)?,
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
        result: QueryResultDescription {
            cardinality: result_cardinality(&located.statement, !fields.is_empty()),
            fields,
            affected_rows: !matches!(operation, QueryOperation::Read | QueryOperation::Explain),
        },
    })
}

fn operation(statement: &Statement) -> QueryOperation {
    match statement {
        Statement::Pipeline(_) => QueryOperation::Read,
        Statement::Explain(_) | Statement::ExplainAnalyze(_) => QueryOperation::Explain,
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
        Statement::Explain(_) | Statement::ExplainAnalyze(_) => QueryCardinality::None,
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
