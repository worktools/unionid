//! Read-only DML planning uses the mutation binders without building candidates.
use super::*;
use crate::query_contract::{QueryOperation, QueryReferenceCheck};

/// Potential write obligations, not a prediction of affected rows or success.
/// Update/delete access plans are carried by `QueryResponse::plan`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MutationPlan {
    pub operation: QueryOperation,
    pub table: String,
    /// Supplied input count for insert/upsert, absent for row targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_rows: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returning_schema: Vec<ResponseColumn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_checks: Vec<QueryReferenceCheck>,
}

impl Database {
    pub(crate) fn reference_descriptions(
        &self,
    ) -> Vec<(String, String, crate::portable::ReferenceDescription)> {
        let mut references = crate::portable::describe_references(self);
        if references.is_empty() {
            return Vec::new();
        }
        self.schema_tables()
            .into_iter()
            .flat_map(|table| {
                references
                    .remove(&table.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |reference| (table.id.to_string(), table.name.clone(), reference))
            })
            .collect()
    }

    pub(crate) fn bind_mutation_explain(
        &self,
        mutation: &mut Statement,
        control: Option<&ExecutionControl>,
    ) -> Result<MutationPlan> {
        check_deadline(control)?;
        let operation = match mutation {
            Statement::Insert { .. } | Statement::InsertParameter { .. } => QueryOperation::Insert,
            Statement::InsertMany { .. } | Statement::InsertManyParameter { .. } => {
                QueryOperation::InsertMany
            }
            Statement::Upsert { .. } | Statement::UpsertParameter { .. } => QueryOperation::Upsert,
            Statement::UpsertMany { .. } | Statement::UpsertManyParameter { .. } => {
                QueryOperation::UpsertMany
            }
            Statement::Update { .. } => QueryOperation::Update,
            Statement::Delete { .. } => QueryOperation::Delete,
            _ => {
                return Err(Error::new(
                    "E_QUERY",
                    "mutation explain requires insert, upsert, update, or delete",
                ));
            }
        };
        let mut input_rows = None;
        let mut updated_fields = Vec::new();
        let (table, returning) = match mutation {
            Statement::InsertParameter {
                table,
                parameter_type,
                returning,
                ..
            }
            | Statement::InsertManyParameter {
                table,
                parameter_type,
                returning,
                ..
            }
            | Statement::UpsertParameter {
                table,
                parameter_type,
                returning,
                ..
            }
            | Statement::UpsertManyParameter {
                table,
                parameter_type,
                returning,
                ..
            } => {
                *parameter_type = Some(match operation {
                    QueryOperation::Insert => {
                        self.prepare_insert_parameter(table, returning.as_ref())?
                    }
                    QueryOperation::InsertMany => {
                        self.prepare_bulk_insert_parameter(table, returning.as_ref())?
                    }
                    QueryOperation::Upsert => {
                        self.prepare_upsert_parameter(table, returning.as_ref())?
                    }
                    QueryOperation::UpsertMany => {
                        self.prepare_bulk_upsert_parameter(table, returning.as_ref())?
                    }
                    _ => unreachable!("parameterized row operation"),
                });
                (table.clone(), returning.as_ref())
            }
            Statement::Insert {
                table,
                values,
                returning,
            }
            | Statement::InsertMany {
                table,
                values,
                returning,
            }
            | Statement::Upsert {
                table,
                values,
                returning,
            }
            | Statement::UpsertMany {
                table,
                values,
                returning,
            } => {
                if matches!(
                    operation,
                    QueryOperation::Upsert | QueryOperation::UpsertMany
                ) {
                    self.prepare_upsert_parameter(table, returning.as_ref())?
                } else {
                    self.prepare_insert_parameter(table, returning.as_ref())?
                };
                let rows = if matches!(
                    operation,
                    QueryOperation::InsertMany | QueryOperation::UpsertMany
                ) {
                    let Value::List(rows) = values else {
                        return Err(Error::new(
                            "E_TYPE",
                            "bulk mutation explain requires a list of records",
                        ));
                    };
                    rows.as_slice()
                } else {
                    std::slice::from_ref(values)
                };
                if rows.len() > MAX_BULK_INSERT_ROWS {
                    return Err(Error::new(
                        "E_LIMIT",
                        "bulk mutation explain exceeds the row limit",
                    ));
                }
                for (position, row) in rows.iter().enumerate() {
                    check_deadline_periodically(control, position)?;
                    self.coerce_row(
                        table,
                        row,
                        if matches!(
                            operation,
                            QueryOperation::Upsert | QueryOperation::UpsertMany
                        ) {
                            "upsert"
                        } else {
                            "insert"
                        },
                    )?;
                }
                input_rows = Some(rows.len());
                (table.clone(), returning.as_ref())
            }
            Statement::Update {
                target,
                assignments,
                returning,
            } => {
                self.prepare_update(target, assignments, returning.as_ref())?;
                updated_fields = assignments
                    .iter()
                    .map(|assignment| assignment.path.clone())
                    .collect();
                (target.from.clone(), returning.as_ref())
            }
            Statement::Delete { target, returning } => {
                self.prepare_delete(target, returning.as_ref())?;
                (target.from.clone(), returning.as_ref())
            }
            _ => unreachable!("operation was validated"),
        };
        let returning_schema = self
            .bind_returning(&table, returning)?
            .map(|bound| bound.columns)
            .unwrap_or_default();
        check_deadline(control)?;
        Ok(MutationPlan {
            operation,
            table,
            input_rows,
            updated_fields,
            returning_schema,
            // Preparation needs only the bound shape; render reference
            // metadata once when producing a response or static description.
            reference_checks: Vec::new(),
        })
    }

    pub(super) fn explain_mutation_from(
        &self,
        source: &dyn TypedRowSource,
        mut mutation: Statement,
        control: Option<&ExecutionControl>,
    ) -> Result<QueryResponse> {
        let mut plan = self.bind_mutation_explain(&mut mutation, control)?;
        plan.reference_checks =
            crate::query_contract::reference_checks_from(self.reference_descriptions(), &mutation);
        let mut response = match mutation {
            Statement::Update { target, .. } | Statement::Delete { target, .. } => {
                let mut response = self.explain_from(source, target)?;
                response
                    .plan
                    .as_mut()
                    .expect("explain produces a plan")
                    .result_schema = plan.returning_schema.clone();
                response
            }
            _ => QueryResponse::ok_message("mutation plan; no writes executed"),
        };
        response.mutation_plan = Some(plan);
        check_deadline(control)?;
        Ok(response)
    }
}
