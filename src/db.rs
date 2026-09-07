use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::migration::MigrationEntry;
use crate::model::{
    Catalog, Column, DbObject, Row, RowId, ScalarType, Table, TypeDefinition, Value,
};
use crate::query::{
    Aggregate, AggregateAssignment, AggregateFunction, Pipeline, Returning, SetAssignment,
    SetValue, Stage, Statement,
};

mod migration;

type Indexes = BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<RowId>>>>;

pub const MAX_QUERY_WORKING_ROWS: usize = 250_000;
pub const MAX_RESULT_ROWS: usize = 100_000;
pub const MAX_BULK_INSERT_ROWS: usize = 100_000;
pub const MAX_RETURNING_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_GROUPS: usize = 100_000;
pub const MAX_AGGREGATE_OUTPUTS: usize = 256;
pub const MAX_AGGREGATE_CELLS: usize = 1_000_000;
pub const MAX_GROUP_WORKING_BYTES: usize = 64 * 1024 * 1024;

struct PlannedAccess {
    plan: QueryAccessPlan,
    candidates: Option<Vec<RowId>>,
}

struct BoundReturning {
    fields: Vec<String>,
    columns: Vec<ResponseColumn>,
}

struct GroupAccumulator {
    fields: Vec<(String, Value)>,
    states: Vec<AggregateState>,
}

enum AggregateState {
    Count(i64),
    Sum(Option<Value>),
    Min(Option<Value>),
    Max(Option<Value>),
}

impl GroupAccumulator {
    fn new(fields: Vec<(String, Value)>, assignments: &[AggregateAssignment]) -> Self {
        let states = assignments
            .iter()
            .map(|assignment| match assignment.function {
                AggregateFunction::Count => AggregateState::Count(0),
                AggregateFunction::Sum => AggregateState::Sum(None),
                AggregateFunction::Min => AggregateState::Min(None),
                AggregateFunction::Max => AggregateState::Max(None),
            })
            .collect();
        Self { fields, states }
    }

    fn update(
        &mut self,
        catalog: &Catalog,
        row: &BTreeMap<String, Value>,
        assignments: &[AggregateAssignment],
    ) -> Result<()> {
        for (state, assignment) in self.states.iter_mut().zip(assignments) {
            match state {
                AggregateState::Count(count) => {
                    *count = count
                        .checked_add(1)
                        .ok_or_else(|| Error::new("E_ARITH", "count overflowed int"))?;
                }
                AggregateState::Sum(total) => {
                    let value = aggregate_input(catalog, row, assignment)?;
                    *total = Some(match (total.take(), value.unwrapped()) {
                        (None, Value::Int(value)) => Value::Int(*value),
                        (None, Value::Float(value)) => Value::Float(*value),
                        (Some(Value::Int(total)), Value::Int(value)) => Value::Int(
                            total
                                .checked_add(*value)
                                .ok_or_else(|| Error::new("E_ARITH", "integer sum overflow"))?,
                        ),
                        (Some(Value::Float(total)), Value::Float(value)) => {
                            let result = total + value;
                            if !result.is_finite() {
                                return Err(Error::new(
                                    "E_ARITH",
                                    "float sum produced a non-finite value",
                                ));
                            }
                            Value::Float(result)
                        }
                        _ => {
                            return Err(Error::new(
                                "E_TYPE",
                                "bound sum received values with inconsistent numeric types",
                            ));
                        }
                    });
                }
                AggregateState::Min(current) => {
                    update_extreme(current, &aggregate_input(catalog, row, assignment)?, true)?;
                }
                AggregateState::Max(current) => {
                    update_extreme(current, &aggregate_input(catalog, row, assignment)?, false)?;
                }
            }
        }
        Ok(())
    }

    fn finish(
        self,
        catalog: &Catalog,
        assignments: &[AggregateAssignment],
    ) -> Result<BTreeMap<String, Value>> {
        let mut row = self.fields.into_iter().collect::<BTreeMap<_, _>>();
        for (state, assignment) in self.states.into_iter().zip(assignments) {
            let output_type = assignment
                .output_type
                .as_ref()
                .ok_or_else(|| Error::new("E_TYPE", "aggregate output has no bound result type"))?;
            let raw = match state {
                AggregateState::Count(count) => Value::Int(count),
                AggregateState::Sum(Some(Value::Float(value))) => {
                    Value::Float(if value == 0.0 { 0.0 } else { value })
                }
                AggregateState::Sum(Some(value)) => value,
                AggregateState::Sum(None) => match catalog.underlying(output_type)? {
                    ScalarType::Int => Value::Int(0),
                    ScalarType::Float => Value::Float(0.0),
                    _ => {
                        return Err(Error::new("E_TYPE", "bound sum output is not numeric"));
                    }
                },
                AggregateState::Min(value) | AggregateState::Max(value) => {
                    Value::Option(value.map(Box::new))
                }
            };
            let value = catalog.coerce(
                &raw,
                output_type,
                &format!("aggregate '{}' result", assignment.name),
            )?;
            row.insert(assignment.name.clone(), value);
        }
        Ok(row)
    }
}

fn aggregate_input(
    catalog: &Catalog,
    row: &BTreeMap<String, Value>,
    assignment: &AggregateAssignment,
) -> Result<Value> {
    let input = assignment
        .input
        .as_ref()
        .ok_or_else(|| Error::new("E_QUERY", "aggregate input expression is missing"))?;
    crate::expression::evaluate_value(catalog, input, |path| row_field(row, path))
}

fn update_extreme(current: &mut Option<Value>, value: &Value, minimum: bool) -> Result<()> {
    let replace = if let Some(existing) = current.as_ref() {
        let order = value.cmp_ord(existing).ok_or_else(|| {
            Error::new(
                "E_TYPE",
                "bound min/max received values without a shared ordering",
            )
        })?;
        if minimum {
            order == std::cmp::Ordering::Less
        } else {
            order == std::cmp::Ordering::Greater
        }
    } else {
        true
    };
    if replace {
        *current = Some(value.clone());
    }
    Ok(())
}

fn aggregate_function_name(function: AggregateFunction) -> &'static str {
    match function {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
    }
}

fn check_group_limits(
    group_count: usize,
    aggregate_count: usize,
    working_bytes: usize,
) -> Result<()> {
    if group_count > MAX_GROUPS
        || group_count.saturating_mul(aggregate_count.max(1)) > MAX_AGGREGATE_CELLS
    {
        return Err(Error::new(
            "E_LIMIT",
            format!(
                "aggregate needs more than {MAX_GROUPS} groups or {MAX_AGGREGATE_CELLS} accumulator cells"
            ),
        ));
    }
    if working_bytes > MAX_GROUP_WORKING_BYTES {
        return Err(Error::new(
            "E_LIMIT",
            format!(
                "aggregate group state exceeds {} bytes",
                MAX_GROUP_WORKING_BYTES
            ),
        ));
    }
    Ok(())
}

fn check_deadline(deadline: Option<std::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(Error::new(
            "E_TIMEOUT",
            "request execution deadline exceeded",
        ))
    } else {
        Ok(())
    }
}

fn check_deadline_periodically(
    deadline: Option<std::time::Instant>,
    position: usize,
) -> Result<()> {
    if position.is_multiple_of(1024) {
        check_deadline(deadline)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDefinition {
    pub id: u64,
    pub table_id: u64,
    pub column: String,
    pub field_path: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurableMeta {
    pub sequence: u64,
    pub schema_revision: u64,
    pub next_catalog_id: u64,
    pub schema_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurableTable {
    pub id: u64,
    pub name: String,
    pub schema: Vec<Column>,
    pub row_type: Option<u64>,
    pub primary_key: Option<String>,
    #[serde(default)]
    pub next_row_id: RowId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub(crate) enum DurableCatalogEntry {
    Type(TypeDefinition),
    Table(DurableTable),
    Index {
        table: String,
        definition: IndexDefinition,
    },
}

impl DurableCatalogEntry {
    pub(crate) fn kind_tag(&self) -> u8 {
        match self {
            Self::Type(_) => 1,
            Self::Table(_) => 2,
            Self::Index { .. } => 3,
        }
    }

    pub(crate) fn stable_id(&self) -> u64 {
        match self {
            Self::Type(definition) => definition.id,
            Self::Table(table) => table.id,
            Self::Index { definition, .. } => definition.id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaInfo {
    pub revision: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseColumn {
    pub name: String,
    pub ty: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryAccessKind {
    FullScan,
    PrimaryKeyLookup,
    SecondaryIndexLookup,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryAccessPlan {
    pub kind: QueryAccessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub estimated_rows: usize,
    pub table_rows: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryStageKind {
    Let,
    Filter,
    FilterMatch,
    Derive,
    DeriveMatch,
    Aggregate,
    Select,
    Sort,
    Take,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryPlanStage {
    pub position: usize,
    pub kind: QueryStageKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    pub table: String,
    pub access: QueryAccessPlan,
    pub stages: Vec<QueryPlanStage>,
    pub result_schema: Vec<ResponseColumn>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpsertAction {
    Inserted,
    Updated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub ok: bool,
    pub message: String,
    pub rows: Vec<BTreeMap<String, Value>>,
    #[serde(default)]
    pub columns: Vec<ResponseColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upsert_action: Option<UpsertAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<QueryPlan>,
}

impl QueryResponse {
    pub fn ok_message(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            rows: Vec::new(),
            columns: Vec::new(),
            error: None,
            warnings: Vec::new(),
            schema: None,
            affected_rows: None,
            upsert_action: None,
            plan: None,
        }
    }

    pub fn failure(error: Error) -> Self {
        Self {
            ok: false,
            message: error.to_string(),
            rows: Vec::new(),
            columns: Vec::new(),
            error: Some(error),
            warnings: Vec::new(),
            schema: None,
            affected_rows: None,
            upsert_action: None,
            plan: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self::failure(Error::new("E_QUERY", message))
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Database {
    objects: BTreeMap<String, DbObject>,
    #[serde(default)]
    indexes: Indexes,
    #[serde(default)]
    index_definitions: BTreeMap<String, BTreeMap<String, IndexDefinition>>,
    #[serde(default)]
    pub catalog: Catalog,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    schema_revision: u64,
    #[serde(default)]
    migration_history: Vec<MigrationEntry>,
}

impl Database {
    pub fn execute(&mut self, stmt: Statement) -> Result<QueryResponse> {
        self.execute_with_deadline(stmt, None)
    }

    pub(crate) fn execute_with_deadline(
        &mut self,
        stmt: Statement,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        match stmt {
            Statement::DefineType { name, ty } => {
                if self.objects.contains_key(&name) {
                    return Err(Error::new(
                        "E_SCHEMA",
                        format!("name '{name}' is already used by a table"),
                    ));
                }
                self.catalog.define(name.clone(), ty)?;
                Ok(QueryResponse::ok_message(format!("type '{name}' defined")))
            }
            Statement::CreateTable { table, columns } => {
                let ScalarType::Record(columns) =
                    self.catalog.resolve(ScalarType::Record(columns), 0)?
                else {
                    unreachable!()
                };
                self.create_table(table, columns, None, None)
            }
            Statement::TypedTable {
                table,
                row_type,
                key,
            } => {
                let def = self.catalog.types.get(&row_type).ok_or_else(|| {
                    Error::new("E_SCHEMA", format!("unknown row type '{row_type}'"))
                })?;
                let ScalarType::Record(columns) = self.catalog.underlying(&def.ty)? else {
                    return Err(Error::new("E_TYPE", "a table's row type must be a record"));
                };
                self.create_table(table, columns.clone(), Some(def.id), key)
            }
            Statement::CreateIndex { table, column } => self.create_index(&table, &column),
            Statement::Insert {
                table,
                values,
                returning,
            } => self.insert(&table, values, returning.as_ref()),
            Statement::InsertMany {
                table,
                values,
                returning,
            } => self.insert_many(&table, values, returning.as_ref(), deadline),
            Statement::InsertParameter { parameter, .. }
            | Statement::InsertManyParameter { parameter, .. }
            | Statement::UpsertParameter { parameter, .. } => Err(Error::new(
                "E_PARAM_MISSING",
                format!("parameter '${parameter}' was not bound"),
            )),
            Statement::Upsert {
                table,
                values,
                returning,
            } => self.upsert(&table, values, returning.as_ref()),
            Statement::Update {
                mut target,
                mut assignments,
                returning,
            } => self.update(&mut target, &mut assignments, returning.as_ref(), deadline),
            Statement::Delete {
                mut target,
                returning,
            } => self.delete(&mut target, returning.as_ref(), deadline),
            Statement::Migration {
                name,
                parent: _,
                steps,
            } => self.migrate(&name, steps),
            Statement::Explain(pipeline) => self.explain(pipeline),
            Statement::Pipeline(pipeline) => self.query(pipeline, deadline),
        }
    }

    fn table(&self, name: &str) -> Result<&Table> {
        let Some(DbObject::Table(table)) = self.objects.get(name) else {
            return Err(Error::new("E_TABLE", format!("table '{name}' not found")));
        };
        Ok(table)
    }

    fn create_table(
        &mut self,
        name: String,
        columns: Vec<Column>,
        row_type: Option<u64>,
        key: Option<String>,
    ) -> Result<QueryResponse> {
        if self.objects.contains_key(&name) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("table '{name}' already exists"),
            ));
        }
        if self.catalog.types.contains_key(&name) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("name '{name}' is already used by a type"),
            ));
        }
        if let Some(key) = &key {
            let ty = self.catalog.field_type(&columns, key)?;
            if !matches!(
                self.catalog.underlying(ty)?,
                ScalarType::Int | ScalarType::Text
            ) {
                return Err(Error::new(
                    "E_TYPE",
                    "primary keys currently require int or text",
                ));
            }
        }
        let table_id = self.catalog.allocate()?;
        self.objects.insert(
            name.clone(),
            DbObject::Table(Table {
                id: table_id,
                name: name.clone(),
                schema: columns,
                rows: Vec::new(),
                next_row_id: 0,
                row_type,
                primary_key: key.clone(),
            }),
        );
        if let Some(key) = key {
            self.create_index(&name, &key)?;
        }
        Ok(QueryResponse::ok_message(format!("table '{name}' created")))
    }

    fn create_index(&mut self, name: &str, column: &str) -> Result<QueryResponse> {
        let (table_id, field_path) = {
            let table = self.table(name)?;
            self.catalog.field_type(&table.schema, column)?;
            (
                table.id,
                self.catalog.field_path_ids(&table.schema, column)?,
            )
        };
        if self
            .indexes
            .get(name)
            .is_some_and(|cols| cols.contains_key(column))
        {
            return Err(Error::new(
                "E_INDEX",
                format!("index on '{name}.{column}' already exists"),
            ));
        }
        let posting = self.build_index(name, column)?;
        let definition = IndexDefinition {
            id: self.catalog.allocate()?,
            table_id,
            column: column.into(),
            field_path,
        };
        self.index_definitions
            .entry(name.into())
            .or_default()
            .insert(column.into(), definition);
        self.indexes
            .entry(name.into())
            .or_default()
            .insert(column.into(), posting);
        Ok(QueryResponse::ok_message(format!(
            "index created on '{name}.{column}'"
        )))
    }

    fn build_index(&self, name: &str, column: &str) -> Result<BTreeMap<String, Vec<RowId>>> {
        let table = self.table(name)?;
        Ok(build_posting(&table.rows, column))
    }

    fn insert(
        &mut self,
        name: &str,
        values: Value,
        returning: Option<&Returning>,
    ) -> Result<QueryResponse> {
        let returning = self.bind_returning(name, returning)?;
        let fields = self.coerce_row(name, &values, "insert")?;
        let returned = self.returning_rows(returning.as_ref(), &[&fields])?;
        self.insert_fields(name, fields)?;
        let mut response = QueryResponse::ok_message(format!("inserted into '{name}'"));
        response.affected_rows = Some(1);
        apply_returning(&mut response, returning, returned);
        Ok(response)
    }

    fn insert_many(
        &mut self,
        name: &str,
        values: Value,
        returning: Option<&Returning>,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        check_deadline(deadline)?;
        let returning = self.bind_returning(name, returning)?;
        let Value::List(values) = values else {
            return Err(Error::new(
                "E_TYPE",
                "insert many requires a list of complete records",
            ));
        };
        if values.len() > MAX_BULK_INSERT_ROWS {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "bulk insert contains {} rows; limit is {MAX_BULK_INSERT_ROWS}",
                    values.len()
                ),
            ));
        }

        let mut fields = Vec::with_capacity(values.len());
        for (position, value) in values.iter().enumerate() {
            check_deadline_periodically(deadline, position)?;
            fields.push(self.coerce_row(name, value, "insert many")?);
        }
        let returned_fields = fields.iter().collect::<Vec<_>>();
        let returned = self.returning_rows(returning.as_ref(), &returned_fields)?;

        let table = self.table(name)?;
        let count = u64::try_from(fields.len())
            .map_err(|_| Error::new("E_LIMIT", "bulk insert row count exceeds u64"))?;
        let next_row_id = table
            .next_row_id
            .checked_add(count)
            .ok_or_else(|| Error::new("E_LIMIT", "row ID space exhausted"))?;
        let mut rows = table.rows.clone();
        let first_row_id = table.next_row_id;
        for (position, fields) in fields.into_iter().enumerate() {
            check_deadline_periodically(deadline, position)?;
            let offset = u64::try_from(position)
                .map_err(|_| Error::new("E_LIMIT", "bulk insert row count exceeds u64"))?;
            rows.push(Row {
                id: first_row_id + offset,
                fields,
            });
        }
        self.validate_primary_keys(name, &rows)?;
        check_deadline(deadline)?;
        self.replace_rows_and_indexes(name, rows)?;
        let Some(DbObject::Table(table)) = self.objects.get_mut(name) else {
            unreachable!("bulk insert table was validated")
        };
        table.next_row_id = next_row_id;

        let affected = usize::try_from(count)
            .map_err(|_| Error::new("E_LIMIT", "bulk insert row count exceeds usize"))?;
        let mut response =
            QueryResponse::ok_message(format!("inserted {affected} rows into '{name}'"));
        response.affected_rows = Some(affected);
        apply_returning(&mut response, returning, returned);
        Ok(response)
    }

    fn upsert(
        &mut self,
        name: &str,
        values: Value,
        returning: Option<&Returning>,
    ) -> Result<QueryResponse> {
        let returning = self.bind_returning(name, returning)?;
        let table = self.table(name)?;
        let key = table.primary_key.clone().ok_or_else(|| {
            Error::new(
                "E_CONSTRAINT",
                format!("upsert requires a primary key on table '{name}'"),
            )
        })?;
        let fields = self.coerce_row(name, &values, "upsert")?;
        let returned = self.returning_rows(returning.as_ref(), &[&fields])?;
        let key_value = row_field(&fields, &key)
            .ok_or_else(|| Error::new("E_FIELD", format!("missing key '{key}'")))?;
        let existing_id = self
            .indexes
            .get(name)
            .and_then(|columns| columns.get(&key))
            .and_then(|posting| posting.get(&key_value.index_key()))
            .and_then(|ids| ids.first())
            .copied();
        let action = if let Some(id) = existing_id {
            let mut rows = table.rows.clone();
            let position = rows.binary_search_by_key(&id, |row| row.id).map_err(|_| {
                Error::new(
                    "E_INDEX",
                    format!("primary-key index for '{name}.{key}' references a missing row"),
                )
            })?;
            rows[position].fields = fields;
            self.validate_primary_keys(name, &rows)?;
            self.replace_rows_and_indexes(name, rows)?;
            UpsertAction::Updated
        } else {
            self.insert_fields(name, fields)?;
            UpsertAction::Inserted
        };
        let message = match action {
            UpsertAction::Inserted => format!("inserted one row into '{name}'"),
            UpsertAction::Updated => format!("updated one row in '{name}'"),
        };
        let mut response = QueryResponse::ok_message(message);
        response.affected_rows = Some(1);
        response.upsert_action = Some(action);
        apply_returning(&mut response, returning, returned);
        Ok(response)
    }

    fn coerce_row(
        &self,
        name: &str,
        values: &Value,
        operation: &str,
    ) -> Result<BTreeMap<String, Value>> {
        let ty = self.table_row_type(name)?;
        let value = self.catalog.coerce(values, &ty, name)?;
        let Value::Record(fields) = value.unwrapped() else {
            return Err(Error::new(
                "E_TYPE",
                format!("{operation} requires a complete record"),
            ));
        };
        Ok(fields.clone())
    }

    fn table_row_type(&self, name: &str) -> Result<ScalarType> {
        let table = self.table(name)?;
        Ok(table
            .row_type
            .map(ScalarType::Ref)
            .unwrap_or_else(|| ScalarType::Record(table.schema.clone())))
    }

    pub(crate) fn prepare_bulk_insert_parameter(
        &self,
        table: &str,
        returning: Option<&Returning>,
    ) -> Result<ScalarType> {
        self.bind_returning(table, returning)?;
        Ok(ScalarType::List(Box::new(self.table_row_type(table)?)))
    }

    fn insert_fields(&mut self, name: &str, fields: BTreeMap<String, Value>) -> Result<RowId> {
        let table = self.table(name)?;
        if let Some(key) = &table.primary_key {
            let value = row_field(&fields, key)
                .ok_or_else(|| Error::new("E_FIELD", format!("missing key '{key}'")))?;
            if self
                .indexes
                .get(name)
                .and_then(|cols| cols.get(key))
                .is_some_and(|posting| posting.contains_key(&value.index_key()))
            {
                return Err(Error::new(
                    "E_CONSTRAINT",
                    format!("duplicate primary key '{name}.{key}'"),
                ));
            }
        }
        let id = table.next_row_id;
        let next_row_id = id
            .checked_add(1)
            .ok_or_else(|| Error::new("E_LIMIT", "row ID space exhausted"))?;
        if let Some(cols) = self.indexes.get_mut(name) {
            for (column, posting) in cols {
                if let Some(value) = row_field(&fields, column) {
                    posting.entry(value.index_key()).or_default().push(id);
                }
            }
        }
        let Some(DbObject::Table(table)) = self.objects.get_mut(name) else {
            unreachable!()
        };
        table.rows.push(Row { id, fields });
        table.next_row_id = next_row_id;
        Ok(id)
    }

    fn update(
        &mut self,
        target: &mut Pipeline,
        assignments: &mut [SetAssignment],
        returning: Option<&Returning>,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        let returning = self.bind_returning(&target.from, returning)?;
        let table = self.table(&target.from)?;
        let schema = table.schema.clone();
        let row_type = table
            .row_type
            .map(ScalarType::Ref)
            .unwrap_or_else(|| ScalarType::Record(schema.clone()));
        self.bind_mutation_target(target, &schema)?;
        let mut paths: Vec<String> = Vec::new();
        for assignment in assignments.iter_mut() {
            if let Some(earlier) = paths
                .iter()
                .find(|earlier| paths_overlap(earlier, &assignment.path))
            {
                return Err(Error::new(
                    "E_QUERY",
                    format!(
                        "update fields '{}' and '{}' overlap",
                        earlier, assignment.path
                    ),
                ));
            }
            paths.push(assignment.path.clone());
            let expected = self.catalog.field_type(&schema, &assignment.path)?.clone();
            match &mut assignment.value {
                SetValue::Expression(value) => {
                    crate::expression::bind_scalar(
                        &self.catalog,
                        &schema,
                        value,
                        Some(&expected),
                        "field",
                    )?;
                }
                SetValue::Match(value) => {
                    crate::matching::bind_assignment(&self.catalog, &schema, &expected, value)?
                }
            }
        }
        let target_order = self.mutation_target_ids(target, deadline)?;
        let mut rows = table.rows.clone();
        let target_ids = target_order.iter().copied().collect::<BTreeSet<_>>();
        for row in rows.iter_mut().filter(|row| target_ids.contains(&row.id)) {
            let original = row.fields.clone();
            let mut values = Vec::with_capacity(assignments.len());
            for assignment in assignments.iter() {
                let expected = self.catalog.field_type(&schema, &assignment.path)?.clone();
                let value = match &assignment.value {
                    SetValue::Expression(value) => {
                        crate::expression::evaluate_value(&self.catalog, value, |path| {
                            row_field(&original, path)
                        })?
                    }
                    SetValue::Match(value) => {
                        crate::matching::evaluate_assignment(&self.catalog, &original, value)?
                    }
                };
                values.push((
                    assignment.path.as_str(),
                    self.catalog.coerce(&value, &expected, "update value")?,
                ));
            }
            for (path, value) in values {
                set_row_field(&mut row.fields, path, value)?;
            }
            let checked = self.catalog.coerce(
                &Value::Record(row.fields.clone()),
                &row_type,
                "updated row",
            )?;
            let Value::Record(fields) = checked.unwrapped() else {
                unreachable!()
            };
            row.fields = fields.clone();
        }
        self.validate_primary_keys(&target.from, &rows)?;
        let affected = target_ids.len();
        let returned_fields = target_order
            .iter()
            .filter_map(|id| {
                rows.binary_search_by_key(id, |row| row.id)
                    .ok()
                    .map(|position| &rows[position].fields)
            })
            .collect::<Vec<_>>();
        let returned = self.returning_rows(returning.as_ref(), &returned_fields)?;
        self.replace_rows_and_indexes(&target.from, rows)?;
        let mut response =
            QueryResponse::ok_message(format!("updated {affected} row(s) in '{}'", target.from));
        response.affected_rows = Some(affected);
        apply_returning(&mut response, returning, returned);
        Ok(response)
    }

    fn delete(
        &mut self,
        target: &mut Pipeline,
        returning: Option<&Returning>,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        let returning = self.bind_returning(&target.from, returning)?;
        let schema = self.table(&target.from)?.schema.clone();
        self.bind_mutation_target(target, &schema)?;
        let target_order = self.mutation_target_ids(target, deadline)?;
        let target_ids = target_order.iter().copied().collect::<BTreeSet<_>>();
        let mut rows = self.table(&target.from)?.rows.clone();
        let returned_fields = target_order
            .iter()
            .filter_map(|id| {
                rows.binary_search_by_key(id, |row| row.id)
                    .ok()
                    .map(|position| &rows[position].fields)
            })
            .collect::<Vec<_>>();
        let returned = self.returning_rows(returning.as_ref(), &returned_fields)?;
        rows.retain(|row| !target_ids.contains(&row.id));
        self.replace_rows_and_indexes(&target.from, rows)?;
        let affected = target_ids.len();
        let mut response =
            QueryResponse::ok_message(format!("deleted {affected} row(s) from '{}'", target.from));
        response.affected_rows = Some(affected);
        apply_returning(&mut response, returning, returned);
        Ok(response)
    }

    fn bind_returning(
        &self,
        table_name: &str,
        returning: Option<&Returning>,
    ) -> Result<Option<BoundReturning>> {
        let Some(returning) = returning else {
            return Ok(None);
        };
        let table = self.table(table_name)?;
        let fields = if returning.fields.is_empty() {
            table
                .schema
                .iter()
                .map(|column| column.name.clone())
                .collect()
        } else {
            returning.fields.clone()
        };
        let columns = fields
            .iter()
            .map(|field| {
                self.catalog
                    .field_type(&table.schema, field)
                    .map(|ty| ResponseColumn {
                        name: field.clone(),
                        ty: self.catalog.describe(ty),
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(BoundReturning { fields, columns }))
    }

    fn returning_rows(
        &self,
        returning: Option<&BoundReturning>,
        rows: &[&BTreeMap<String, Value>],
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        let Some(returning) = returning else {
            return Ok(Vec::new());
        };
        if rows.len() > MAX_RESULT_ROWS {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "mutation returns {} rows; limit is {MAX_RESULT_ROWS}; add a selective filter",
                    rows.len()
                ),
            ));
        }
        let returned = rows
            .iter()
            .map(|row| {
                returning
                    .fields
                    .iter()
                    .map(|field| {
                        let value = row_field(row, field).expect("returning field was bound");
                        (field.clone(), value.clone())
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .collect::<Vec<_>>();
        let wire = returned
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(name, value)| (name, crate::protocol::WireValue::from(value)))
                    .collect::<BTreeMap<_, _>>()
            })
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&wire)
            .map_err(|error| Error::new("E_PROTOCOL", error.to_string()))?;
        if encoded.len() > MAX_RETURNING_BYTES {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "mutation returning rows encode to {} bytes; limit is {MAX_RETURNING_BYTES}",
                    encoded.len()
                ),
            ));
        }
        Ok(returned)
    }

    fn bind_mutation_target(&self, target: &mut Pipeline, schema: &[Column]) -> Result<()> {
        for stage in &mut target.stages {
            match stage {
                Stage::Filter(expression) => {
                    crate::expression::bind(&self.catalog, schema, expression)?
                }
                Stage::FilterMatch(predicate) => {
                    crate::matching::bind(&self.catalog, schema, predicate)?
                }
                Stage::Sort(keys) => {
                    for key in keys {
                        let ty = self.catalog.field_type(schema, &key.column)?;
                        if !self.orderable(ty)? {
                            return Err(Error::new(
                                "E_TYPE",
                                format!("field '{}' has no ordering", key.column),
                            ));
                        }
                    }
                }
                Stage::Take { .. } => {}
                Stage::Let(_)
                | Stage::Derive(_)
                | Stage::DeriveMatch(_)
                | Stage::Aggregate(_)
                | Stage::Select(_) => {
                    return Err(Error::new(
                        "E_QUERY",
                        "update and delete targets support only filter, sort, and take stages",
                    ));
                }
            }
        }
        Ok(())
    }

    fn mutation_target_ids(
        &self,
        target: &Pipeline,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<RowId>> {
        check_deadline(deadline)?;
        let table = self.table(&target.from)?;
        let candidates = self.plan_access(target)?.candidates;
        let working_rows = candidates
            .as_ref()
            .map_or(table.rows.len(), std::vec::Vec::len);
        if working_rows > MAX_QUERY_WORKING_ROWS {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "mutation needs {working_rows} working rows; limit is {MAX_QUERY_WORKING_ROWS}; add a selective indexed filter"
                ),
            ));
        }
        let mut rows = match candidates {
            Some(ids) => ids
                .into_iter()
                .filter_map(|id| {
                    table
                        .rows
                        .binary_search_by_key(&id, |row| row.id)
                        .ok()
                        .map(|position| &table.rows[position])
                })
                .collect::<Vec<_>>(),
            None => table.rows.iter().collect(),
        };
        let mut evaluation_budget = crate::expression::EvaluationBudget::new();
        for stage in &target.stages {
            match stage {
                Stage::Filter(expression) => {
                    let mut filtered = Vec::with_capacity(rows.len());
                    for (position, row) in rows.into_iter().enumerate() {
                        check_deadline_periodically(deadline, position)?;
                        if crate::expression::evaluate(
                            &self.catalog,
                            expression,
                            |path| row_field(&row.fields, path),
                            &mut evaluation_budget,
                        )? {
                            filtered.push(row);
                        }
                    }
                    rows = filtered;
                }
                Stage::FilterMatch(predicate) => {
                    let mut filtered = Vec::with_capacity(rows.len());
                    for (position, row) in rows.into_iter().enumerate() {
                        check_deadline_periodically(deadline, position)?;
                        if crate::matching::evaluate(
                            &self.catalog,
                            &row.fields,
                            predicate,
                            &mut evaluation_budget,
                        )? {
                            filtered.push(row);
                        }
                    }
                    rows = filtered;
                }
                Stage::Sort(keys) => {
                    check_deadline(deadline)?;
                    rows.sort_by(|a, b| {
                        for key in keys {
                            let order = row_field(&a.fields, &key.column)
                                .zip(row_field(&b.fields, &key.column))
                                .and_then(|(a, b)| a.cmp_ord(b))
                                .unwrap_or(std::cmp::Ordering::Equal);
                            let order = if key.descending {
                                order.reverse()
                            } else {
                                order
                            };
                            if order != std::cmp::Ordering::Equal {
                                return order;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                    check_deadline(deadline)?;
                }
                Stage::Take { offset, limit } => {
                    rows = rows.into_iter().skip(*offset).take(*limit).collect();
                }
                Stage::Let(_)
                | Stage::Derive(_)
                | Stage::DeriveMatch(_)
                | Stage::Aggregate(_)
                | Stage::Select(_) => unreachable!("mutation target stages were bound"),
            }
        }
        check_deadline(deadline)?;
        Ok(rows.into_iter().map(|row| row.id).collect())
    }

    fn validate_primary_keys(&self, name: &str, rows: &[Row]) -> Result<()> {
        let table = self.table(name)?;
        let Some(key) = &table.primary_key else {
            return Ok(());
        };
        let mut seen = BTreeSet::new();
        for row in rows {
            let value = row_field(&row.fields, key).ok_or_else(|| {
                Error::new("E_FIELD", format!("missing primary key '{name}.{key}'"))
            })?;
            if !seen.insert(value.index_key()) {
                return Err(Error::new(
                    "E_CONSTRAINT",
                    format!("duplicate primary key '{name}.{key}'"),
                ));
            }
        }
        Ok(())
    }

    fn replace_rows_and_indexes(&mut self, name: &str, rows: Vec<Row>) -> Result<()> {
        let columns = self
            .indexes
            .get(name)
            .map(|columns| columns.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let indexes = columns
            .into_iter()
            .map(|column| {
                let posting = build_posting(&rows, &column);
                (column, posting)
            })
            .collect();
        let Some(DbObject::Table(table)) = self.objects.get_mut(name) else {
            return Err(Error::new("E_TABLE", format!("table '{name}' not found")));
        };
        table.rows = rows;
        if let Some(existing) = self.indexes.get_mut(name) {
            *existing = indexes;
        }
        Ok(())
    }

    pub(crate) fn prepare_pipeline(&self, pipeline: &mut Pipeline) -> Result<Vec<Column>> {
        let table = self.table(&pipeline.from)?;
        let mut schema = table.schema.clone();
        let mut locals = crate::local::LocalScope::new();
        // Validate and bind every stage without touching rows, including for empty tables.
        for stage in &mut pipeline.stages {
            match stage {
                Stage::Let(binding) => locals.define(&self.catalog, &schema, binding)?,
                Stage::Filter(expression) => {
                    locals.expand_bool(&self.catalog, &schema, expression)?;
                    crate::expression::bind(&self.catalog, &schema, expression)?
                }
                Stage::FilterMatch(pred) => {
                    for arm in &mut pred.arms {
                        locals.expand_bool(&self.catalog, &schema, &mut arm.condition)?;
                    }
                    crate::matching::bind(&self.catalog, &schema, pred)?
                }
                Stage::Derive(derive) => {
                    if schema.iter().any(|column| column.name == derive.name) {
                        return Err(Error::new(
                            "E_FIELD",
                            format!(
                                "derive field '{}' already exists; choose a new field name",
                                derive.name
                            ),
                        ));
                    }
                    locals.expand_bool(&self.catalog, &schema, &mut derive.expression)?;
                    let ty = crate::expression::bind_derive(
                        &self.catalog,
                        &schema,
                        &mut derive.expression,
                    )?;
                    derive.output_type = Some(ty.clone());
                    schema.push(Column {
                        name: derive.name.clone(),
                        ty,
                        default: None,
                        id: 0,
                    });
                }
                Stage::DeriveMatch(derive) => {
                    for arm in &mut derive.arms {
                        locals.expand_match_value(&self.catalog, &schema, &mut arm.result)?;
                    }
                    schema.push(crate::matching::bind_derive(
                        &self.catalog,
                        &schema,
                        derive,
                    )?);
                }
                Stage::Aggregate(aggregate) => {
                    for assignment in &mut aggregate.assignments {
                        if let Some(input) = &mut assignment.input {
                            locals.expand_scalar(&self.catalog, &schema, input)?;
                        }
                    }
                    schema = self.bind_aggregate(&schema, aggregate)?;
                }
                Stage::Select(columns) => {
                    schema = columns
                        .iter()
                        .map(|name| {
                            Ok(Column {
                                name: name.clone(),
                                ty: self.catalog.field_type(&schema, name)?.clone(),
                                default: None,
                                id: 0,
                            })
                        })
                        .collect::<Result<_>>()?;
                }
                Stage::Sort(keys) => {
                    for key in keys {
                        let ty = self.catalog.field_type(&schema, &key.column)?;
                        if !self.orderable(ty)? {
                            return Err(Error::new(
                                "E_TYPE",
                                format!("field '{}' has no ordering", key.column),
                            ));
                        }
                    }
                }
                Stage::Take { .. } => {}
            }
        }
        Ok(schema)
    }

    fn bind_aggregate(&self, schema: &[Column], aggregate: &mut Aggregate) -> Result<Vec<Column>> {
        if aggregate.assignments.len() > MAX_AGGREGATE_OUTPUTS {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "aggregate has {} output fields; limit is {MAX_AGGREGATE_OUTPUTS}",
                    aggregate.assignments.len()
                ),
            ));
        }
        let mut output = Vec::with_capacity(aggregate.group_by.len() + aggregate.assignments.len());
        let mut names = BTreeSet::new();
        for path in &aggregate.group_by {
            if !names.insert(path.clone()) {
                return Err(Error::new(
                    "E_QUERY",
                    format!("duplicate group field '{path}'"),
                ));
            }
            output.push(Column {
                name: path.clone(),
                ty: self.catalog.field_type(schema, path)?.clone(),
                default: None,
                id: 0,
            });
        }
        for assignment in &mut aggregate.assignments {
            if !names.insert(assignment.name.clone()) {
                return Err(Error::new(
                    "E_FIELD",
                    format!(
                        "aggregate field '{}' conflicts with a group or aggregate field",
                        assignment.name
                    ),
                ));
            }
            let ty = match assignment.function {
                AggregateFunction::Count => ScalarType::Int,
                AggregateFunction::Sum => {
                    let ty = self.aggregate_input_type(schema, assignment)?;
                    if !matches!(
                        self.catalog.underlying(&ty)?,
                        ScalarType::Int | ScalarType::Float
                    ) {
                        return Err(Error::new(
                            "E_TYPE",
                            format!(
                                "sum expects int or float, got {}",
                                self.catalog.describe(&ty)
                            ),
                        ));
                    }
                    ty
                }
                AggregateFunction::Min | AggregateFunction::Max => {
                    let ty = self.aggregate_input_type(schema, assignment)?;
                    if !self.orderable(&ty)? {
                        return Err(Error::new(
                            "E_TYPE",
                            format!(
                                "{} expects an orderable int, float, or text value, got {}",
                                aggregate_function_name(assignment.function),
                                self.catalog.describe(&ty)
                            ),
                        ));
                    }
                    ScalarType::Option(Box::new(ty))
                }
            };
            assignment.output_type = Some(ty.clone());
            output.push(Column {
                name: assignment.name.clone(),
                ty,
                default: None,
                id: 0,
            });
        }
        Ok(output)
    }

    fn aggregate_input_type(
        &self,
        schema: &[Column],
        assignment: &mut AggregateAssignment,
    ) -> Result<ScalarType> {
        let input = assignment.input.as_mut().ok_or_else(|| {
            Error::new(
                "E_QUERY",
                format!(
                    "{} requires an input expression",
                    aggregate_function_name(assignment.function)
                ),
            )
        })?;
        crate::expression::bind_scalar(&self.catalog, schema, input, None, "aggregate input")
    }

    fn aggregate_rows(
        &self,
        rows: Vec<BTreeMap<String, Value>>,
        aggregate: &Aggregate,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        let mut groups: BTreeMap<Vec<String>, GroupAccumulator> = BTreeMap::new();
        let mut working_bytes = 0usize;
        if aggregate.group_by.is_empty() {
            groups.insert(
                Vec::new(),
                GroupAccumulator::new(Vec::new(), &aggregate.assignments),
            );
        }
        for (position, row) in rows.iter().enumerate() {
            check_deadline_periodically(deadline, position)?;
            let mut key = Vec::with_capacity(aggregate.group_by.len());
            let mut values = Vec::with_capacity(aggregate.group_by.len());
            for path in &aggregate.group_by {
                let value = row_field(row, path).ok_or_else(|| {
                    Error::new(
                        "E_FIELD",
                        format!("missing group field '{path}' during execution"),
                    )
                })?;
                key.push(value.index_key());
                values.push((path.clone(), value.clone()));
            }
            let next_group_count = groups.len().saturating_add(1);
            if !groups.contains_key(&key) {
                let group_bytes = key
                    .iter()
                    .map(String::len)
                    .sum::<usize>()
                    .saturating_mul(2)
                    .saturating_add(aggregate.assignments.len().saturating_mul(32));
                working_bytes = working_bytes.saturating_add(group_bytes);
                check_group_limits(next_group_count, aggregate.assignments.len(), working_bytes)?;
                groups.insert(
                    key.clone(),
                    GroupAccumulator::new(values, &aggregate.assignments),
                );
            }
            let group = groups.get_mut(&key).expect("group was initialized");
            group.update(&self.catalog, row, &aggregate.assignments)?;
        }
        let mut output = Vec::with_capacity(groups.len());
        for (position, (_, group)) in groups.into_iter().enumerate() {
            check_deadline_periodically(deadline, position)?;
            output.push(group.finish(&self.catalog, &aggregate.assignments)?);
        }
        Ok(output)
    }

    fn explain(&self, mut pipeline: Pipeline) -> Result<QueryResponse> {
        let schema = self.prepare_pipeline(&mut pipeline)?;
        let access = self.plan_access(&pipeline)?.plan;
        let result_schema = schema
            .iter()
            .map(|column| ResponseColumn {
                name: column.name.clone(),
                ty: self.catalog.describe(&column.ty),
            })
            .collect();
        let stages = pipeline
            .stages
            .iter()
            .enumerate()
            .map(|(position, stage)| QueryPlanStage {
                position: position + 1,
                kind: match stage {
                    Stage::Let(_) => QueryStageKind::Let,
                    Stage::Filter(_) => QueryStageKind::Filter,
                    Stage::FilterMatch(_) => QueryStageKind::FilterMatch,
                    Stage::Derive(_) => QueryStageKind::Derive,
                    Stage::DeriveMatch(_) => QueryStageKind::DeriveMatch,
                    Stage::Aggregate(_) => QueryStageKind::Aggregate,
                    Stage::Select(_) => QueryStageKind::Select,
                    Stage::Sort(_) => QueryStageKind::Sort,
                    Stage::Take { .. } => QueryStageKind::Take,
                },
            })
            .collect();
        let mut response = QueryResponse::ok_message("query plan");
        response.plan = Some(QueryPlan {
            table: pipeline.from,
            access,
            stages,
            result_schema,
        });
        Ok(response)
    }

    fn plan_access(&self, pipeline: &Pipeline) -> Result<PlannedAccess> {
        let table = self.table(&pipeline.from)?;
        let indexed_filter = pipeline
            .stages
            .iter()
            .find(|stage| !matches!(stage, Stage::Let(_)))
            .and_then(|stage| match stage {
                Stage::Filter(expression) => crate::expression::simple_index_equality(expression),
                _ => None,
            });
        if let Some((column, value)) = indexed_filter
            && let Some(posting) = self
                .indexes
                .get(&pipeline.from)
                .and_then(|columns| columns.get(column))
        {
            let candidates = posting.get(&value.index_key()).cloned().unwrap_or_default();
            let kind = if table.primary_key.as_deref() == Some(column) {
                QueryAccessKind::PrimaryKeyLookup
            } else {
                QueryAccessKind::SecondaryIndexLookup
            };
            return Ok(PlannedAccess {
                plan: QueryAccessPlan {
                    kind,
                    index: Some(format!("{}.{}", pipeline.from, column)),
                    condition: Some(format!("{} == {}", column, value.source_text())),
                    estimated_rows: candidates.len(),
                    table_rows: table.rows.len(),
                },
                candidates: Some(candidates),
            });
        }
        Ok(PlannedAccess {
            plan: QueryAccessPlan {
                kind: QueryAccessKind::FullScan,
                index: None,
                condition: None,
                estimated_rows: table.rows.len(),
                table_rows: table.rows.len(),
            },
            candidates: None,
        })
    }

    fn query(
        &self,
        mut pipeline: Pipeline,
        deadline: Option<std::time::Instant>,
    ) -> Result<QueryResponse> {
        check_deadline(deadline)?;
        let schema = self.prepare_pipeline(&mut pipeline)?;
        let table = self.table(&pipeline.from)?;
        let candidates = self.plan_access(&pipeline)?.candidates;
        let working_rows = candidates
            .as_ref()
            .map_or(table.rows.len(), std::vec::Vec::len);
        if working_rows > MAX_QUERY_WORKING_ROWS {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "query needs {working_rows} working rows; limit is {MAX_QUERY_WORKING_ROWS}; add a selective indexed filter"
                ),
            ));
        }
        let mut rows = match candidates {
            Some(ids) => {
                let mut rows = Vec::with_capacity(ids.len());
                for (position, id) in ids.into_iter().enumerate() {
                    check_deadline_periodically(deadline, position)?;
                    if let Ok(position) = table.rows.binary_search_by_key(&id, |row| row.id) {
                        rows.push(table.rows[position].fields.clone());
                    }
                }
                rows
            }
            None => {
                let mut rows = Vec::with_capacity(table.rows.len());
                for (position, row) in table.rows.iter().enumerate() {
                    check_deadline_periodically(deadline, position)?;
                    rows.push(row.fields.clone());
                }
                rows
            }
        };
        let mut evaluation_budget = crate::expression::EvaluationBudget::new();
        for stage in pipeline.stages {
            match stage {
                Stage::Let(_) => {}
                Stage::Filter(expression) => {
                    let mut filtered = Vec::with_capacity(rows.len());
                    for (position, row) in rows.into_iter().enumerate() {
                        check_deadline_periodically(deadline, position)?;
                        if crate::expression::evaluate(
                            &self.catalog,
                            &expression,
                            |path| row_field(&row, path),
                            &mut evaluation_budget,
                        )? {
                            filtered.push(row);
                        }
                    }
                    rows = filtered;
                }
                Stage::FilterMatch(pred) => {
                    let mut filtered = Vec::with_capacity(rows.len());
                    for (position, row) in rows.into_iter().enumerate() {
                        check_deadline_periodically(deadline, position)?;
                        if crate::matching::evaluate(
                            &self.catalog,
                            &row,
                            &pred,
                            &mut evaluation_budget,
                        )? {
                            filtered.push(row);
                        }
                    }
                    rows = filtered;
                }
                Stage::DeriveMatch(derive) => {
                    for (position, row) in rows.iter_mut().enumerate() {
                        check_deadline_periodically(deadline, position)?;
                        let value = crate::matching::evaluate_derive(&self.catalog, row, &derive)?;
                        row.insert(derive.name.clone(), value);
                    }
                }
                Stage::Derive(derive) => {
                    let output_type = derive.output_type.as_ref().ok_or_else(|| {
                        Error::new("E_TYPE", "derived expression has no bound output type")
                    })?;
                    for (position, row) in rows.iter_mut().enumerate() {
                        check_deadline_periodically(deadline, position)?;
                        let raw = crate::expression::evaluate_derive(
                            &self.catalog,
                            &derive.expression,
                            |path| row_field(row, path),
                            &mut evaluation_budget,
                        )?;
                        let value = self.catalog.coerce(
                            &raw,
                            output_type,
                            &format!("derive '{}' result", derive.name),
                        )?;
                        row.insert(derive.name.clone(), value);
                    }
                }
                Stage::Aggregate(aggregate) => {
                    rows = self.aggregate_rows(rows, &aggregate, deadline)?;
                }
                Stage::Select(columns) => {
                    rows = rows
                        .into_iter()
                        .map(|row| {
                            columns
                                .iter()
                                .filter_map(|name| {
                                    row_field(&row, name).map(|v| (name.clone(), v.clone()))
                                })
                                .collect()
                        })
                        .collect();
                }
                Stage::Sort(keys) => {
                    check_deadline(deadline)?;
                    rows.sort_by(|a, b| {
                        for key in &keys {
                            let order = row_field(a, &key.column)
                                .zip(row_field(b, &key.column))
                                .and_then(|(a, b)| a.cmp_ord(b))
                                .unwrap_or(std::cmp::Ordering::Equal);
                            let order = if key.descending {
                                order.reverse()
                            } else {
                                order
                            };
                            if order != std::cmp::Ordering::Equal {
                                return order;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                    check_deadline(deadline)?;
                }
                Stage::Take { offset, limit } => {
                    rows = rows.into_iter().skip(offset).take(limit).collect()
                }
            }
        }
        check_deadline(deadline)?;
        if rows.len() > MAX_RESULT_ROWS {
            return Err(Error::new(
                "E_LIMIT",
                format!(
                    "query returns {} rows; limit is {MAX_RESULT_ROWS}; add filter or take",
                    rows.len()
                ),
            ));
        }
        Ok(QueryResponse {
            ok: true,
            message: format!("{} row(s)", rows.len()),
            rows,
            columns: schema
                .iter()
                .map(|c| ResponseColumn {
                    name: c.name.clone(),
                    ty: self.catalog.describe(&c.ty),
                })
                .collect(),
            error: None,
            warnings: Vec::new(),
            schema: None,
            affected_rows: None,
            upsert_action: None,
            plan: None,
        })
    }

    fn orderable(&self, ty: &ScalarType) -> Result<bool> {
        Ok(matches!(
            self.catalog.underlying(ty)?,
            ScalarType::Int | ScalarType::Float | ScalarType::Text
        ))
    }
    /// Indexes are derived data. Rebuild on load so older key encodings cannot
    /// change query semantics after a numeric comparison fix.
    pub fn rebuild_indexes(&mut self) -> Result<()> {
        let mut keys = self
            .indexes
            .iter()
            .flat_map(|(table, cols)| cols.keys().map(move |col| (table.clone(), col.clone())))
            .collect::<BTreeSet<_>>();
        keys.extend(
            self.index_definitions
                .iter()
                .flat_map(|(table, definitions)| {
                    definitions
                        .keys()
                        .map(move |column| (table.clone(), column.clone()))
                }),
        );
        for (name, DbObject::Table(table)) in &self.objects {
            if let Some(key) = &table.primary_key {
                keys.insert((name.clone(), key.clone()));
            }
        }
        self.repair_catalog_ids()?;
        self.repair_row_ids()?;
        self.indexes.clear();
        for (table, column) in keys {
            if !self
                .index_definitions
                .get(&table)
                .is_some_and(|definitions| definitions.contains_key(&column))
            {
                let (table_id, field_path) = {
                    let source = self.table(&table)?;
                    (
                        source.id,
                        self.catalog.field_path_ids(&source.schema, &column)?,
                    )
                };
                let definition = IndexDefinition {
                    id: self.catalog.allocate()?,
                    table_id,
                    column: column.clone(),
                    field_path,
                };
                self.index_definitions
                    .entry(table.clone())
                    .or_default()
                    .insert(column.clone(), definition);
            }
            let posting = self.build_index(&table, &column)?;
            self.indexes
                .entry(table)
                .or_default()
                .insert(column, posting);
        }
        Ok(())
    }

    fn repair_catalog_ids(&mut self) -> Result<()> {
        let missing = self
            .objects
            .iter()
            .filter_map(|(name, object)| match object {
                DbObject::Table(table) if table.id == 0 => Some(name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for name in missing {
            let id = self.catalog.allocate()?;
            let Some(DbObject::Table(table)) = self.objects.get_mut(&name) else {
                unreachable!()
            };
            table.id = id;
        }
        if self.schema_revision == 0 && (!self.catalog.types.is_empty() || !self.objects.is_empty())
        {
            // Old snapshots did not record revisions. Treat their complete
            // catalog as one imported baseline instead of pretending it is empty.
            self.schema_revision = 1;
        }
        Ok(())
    }

    fn repair_row_ids(&mut self) -> Result<()> {
        for object in self.objects.values_mut() {
            let DbObject::Table(table) = object;
            if table.next_row_id == 0 && !table.rows.is_empty() {
                // Transitional snapshots did not store row identities. Their
                // vector order was also their durable row identity.
                for (id, row) in table.rows.iter_mut().enumerate() {
                    row.id = u64::try_from(id)
                        .map_err(|_| Error::new("E_LIMIT", "row ID space exhausted"))?;
                }
                table.next_row_id = u64::try_from(table.rows.len())
                    .map_err(|_| Error::new("E_LIMIT", "row ID space exhausted"))?;
            }
            let mut previous = None;
            for row in &table.rows {
                if previous.is_some_and(|id| row.id <= id) || row.id >= table.next_row_id {
                    return Err(Error::new(
                        "E_STORAGE",
                        format!("table '{}' has invalid stable row IDs", table.name),
                    ));
                }
                previous = Some(row.id);
            }
        }
        Ok(())
    }

    pub(crate) fn advance_schema_revision(&mut self) -> Result<()> {
        self.schema_revision = self
            .schema_revision
            .checked_add(1)
            .ok_or_else(|| Error::new("E_LIMIT", "schema revision exhausted"))?;
        Ok(())
    }

    pub fn schema_info(&self) -> SchemaInfo {
        #[derive(Serialize)]
        struct SchemaTable<'a> {
            id: u64,
            name: &'a str,
            schema: &'a [Column],
            row_type: Option<u64>,
            primary_key: &'a Option<String>,
        }

        #[derive(Serialize)]
        struct SchemaManifest<'a> {
            format_version: u32,
            types: Vec<&'a crate::model::TypeDefinition>,
            tables: Vec<SchemaTable<'a>>,
            indexes: Vec<&'a IndexDefinition>,
        }

        let mut types = self.catalog.types.values().collect::<Vec<_>>();
        types.sort_by_key(|definition| definition.id);
        let mut table_values = self
            .objects
            .values()
            .map(|object| match object {
                DbObject::Table(table) => table,
            })
            .collect::<Vec<_>>();
        table_values.sort_by_key(|table| table.id);
        let tables = table_values
            .into_iter()
            .map(|table| SchemaTable {
                id: table.id,
                name: &table.name,
                schema: &table.schema,
                row_type: table.row_type,
                primary_key: &table.primary_key,
            })
            .collect();
        let mut indexes = self
            .index_definitions
            .values()
            .flat_map(|definitions| definitions.values())
            .collect::<Vec<_>>();
        indexes.sort_by_key(|definition| definition.id);
        let manifest = SchemaManifest {
            format_version: 1,
            types,
            tables,
            indexes,
        };
        let encoded = serde_json::to_vec(&manifest)
            .expect("serializing the schema manifest to memory cannot fail");
        let hash = format!("sha256:{:x}", Sha256::digest(encoded));
        SchemaInfo {
            revision: self.schema_revision,
            hash,
        }
    }

    pub fn table_names(&self) -> Vec<String> {
        self.objects.keys().cloned().collect()
    }

    pub fn type_names(&self) -> Vec<String> {
        self.catalog.types.keys().cloned().collect()
    }

    pub fn field_names(&self) -> Vec<String> {
        fn collect(ty: &ScalarType, fields: &mut BTreeSet<String>) {
            match ty {
                ScalarType::Record(columns) => {
                    for column in columns {
                        fields.insert(column.name.clone());
                        collect(&column.ty, fields);
                    }
                }
                ScalarType::Enum(definition) => {
                    for variant in &definition.variants {
                        for argument in &variant.args {
                            collect(argument, fields);
                        }
                    }
                }
                ScalarType::Option(inner) | ScalarType::List(inner) => collect(inner, fields),
                ScalarType::Tuple(items) => {
                    for item in items {
                        collect(item, fields);
                    }
                }
                ScalarType::Int
                | ScalarType::Float
                | ScalarType::Bool
                | ScalarType::Text
                | ScalarType::Named(_)
                | ScalarType::Ref(_) => {}
            }
        }

        let mut fields = BTreeSet::new();
        for definition in self.catalog.types.values() {
            collect(&definition.ty, &mut fields);
        }
        for table in self.schema_tables() {
            for column in &table.schema {
                fields.insert(column.name.clone());
                collect(&column.ty, &mut fields);
            }
        }
        fields.into_iter().collect()
    }

    pub(crate) fn schema_tables(&self) -> Vec<&Table> {
        self.objects
            .values()
            .map(|object| match object {
                DbObject::Table(table) => table,
            })
            .collect()
    }

    pub(crate) fn schema_indexes(&self) -> Vec<(&str, &IndexDefinition)> {
        self.index_definitions
            .iter()
            .flat_map(|(table, definitions)| {
                definitions
                    .values()
                    .map(move |definition| (table.as_str(), definition))
            })
            .collect()
    }

    pub(crate) fn schema_type_impact(&self, name: &str) -> Vec<(String, usize, usize)> {
        let Some(definition) = self.catalog.types.get(name) else {
            return Vec::new();
        };
        let mut impact = self
            .schema_tables()
            .into_iter()
            .filter(|table| {
                table.row_type == Some(definition.id)
                    || table.schema.iter().any(|column| {
                        type_reaches(
                            &self.catalog,
                            &column.ty,
                            definition.id,
                            &mut BTreeSet::new(),
                        )
                    })
            })
            .map(|table| {
                (
                    table.name.clone(),
                    table.rows.len(),
                    self.index_definitions
                        .get(&table.name)
                        .map_or(0, BTreeMap::len),
                )
            })
            .collect::<Vec<_>>();
        impact.sort();
        impact
    }

    pub(crate) fn durable_meta(&self) -> DurableMeta {
        DurableMeta {
            sequence: self.sequence,
            schema_revision: self.schema_revision,
            next_catalog_id: self.catalog.next_id(),
            schema_hash: self.schema_info().hash,
        }
    }

    pub(crate) fn durable_catalog_entries(&self) -> Vec<DurableCatalogEntry> {
        let mut entries = self
            .catalog
            .types
            .values()
            .cloned()
            .map(DurableCatalogEntry::Type)
            .collect::<Vec<_>>();
        entries.extend(self.objects.values().map(|object| match object {
            DbObject::Table(table) => DurableCatalogEntry::Table(DurableTable {
                id: table.id,
                name: table.name.clone(),
                schema: table.schema.clone(),
                row_type: table.row_type,
                primary_key: table.primary_key.clone(),
                next_row_id: table.next_row_id,
            }),
        }));
        entries.extend(
            self.index_definitions
                .iter()
                .flat_map(|(table, definitions)| {
                    definitions
                        .values()
                        .cloned()
                        .map(|definition| DurableCatalogEntry::Index {
                            table: table.clone(),
                            definition,
                        })
                }),
        );
        entries.sort_by_key(DurableCatalogEntry::stable_id);
        entries
    }

    pub(crate) fn durable_rows(&self) -> Result<Vec<(u64, u64, Vec<u8>)>> {
        let mut encoded = Vec::new();
        for object in self.objects.values() {
            let DbObject::Table(table) = object;
            let ty = table
                .row_type
                .map(ScalarType::Ref)
                .unwrap_or_else(|| ScalarType::Record(table.schema.clone()));
            for row in &table.rows {
                let record = Value::Record(row.fields.clone());
                let value = match table.row_type {
                    Some(type_id) => Value::Named {
                        type_id,
                        value: Box::new(record),
                    },
                    None => record,
                };
                encoded.push((
                    table.id,
                    row.id,
                    crate::codec::encode_value(&self.catalog, &ty, &value)?,
                ));
            }
        }
        Ok(encoded)
    }

    pub(crate) fn durable_secondary_indexes(&self) -> Result<Vec<(u64, String, u64)>> {
        let mut entries = Vec::new();
        for (table, definitions) in &self.index_definitions {
            for (column, definition) in definitions {
                let posting = self
                    .indexes
                    .get(table)
                    .and_then(|columns| columns.get(column))
                    .ok_or_else(|| {
                        Error::new(
                            "E_STORAGE",
                            format!("missing in-memory index '{table}.{column}'"),
                        )
                    })?;
                for (value_key, row_ids) in posting {
                    for row_id in row_ids {
                        entries.push((definition.id, value_key.clone(), *row_id));
                    }
                }
            }
        }
        entries.sort();
        Ok(entries)
    }

    pub fn migration_history(&self) -> &[MigrationEntry] {
        &self.migration_history
    }

    pub(crate) fn append_migration(&mut self, entry: MigrationEntry) -> Result<()> {
        crate::migration::validate_next(&self.migration_history, &entry)?;
        self.migration_history.push(entry);
        Ok(())
    }

    pub(crate) fn durable_migrations(&self) -> &[MigrationEntry] {
        &self.migration_history
    }

    pub(crate) fn validate_logical_backup(mut self) -> Result<Self> {
        self.rebuild_indexes()?;
        Self::from_durable(
            self.durable_meta(),
            self.durable_catalog_entries(),
            self.durable_rows()?,
            self.migration_history.clone(),
        )
    }

    pub(crate) fn from_durable(
        meta: DurableMeta,
        entries: Vec<DurableCatalogEntry>,
        mut rows: Vec<(u64, u64, Vec<u8>)>,
        migration_history: Vec<MigrationEntry>,
    ) -> Result<Self> {
        let mut ids = BTreeSet::new();
        let mut catalog = Catalog::default();
        let mut objects = BTreeMap::new();
        let mut index_definitions: BTreeMap<String, BTreeMap<String, IndexDefinition>> =
            BTreeMap::new();
        let mut max_id = 0;
        for entry in entries {
            let id = entry.stable_id();
            if id == 0 || !ids.insert(id) {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("invalid or duplicate catalog ID {id}"),
                ));
            }
            max_id = max_id.max(id);
            match entry {
                DurableCatalogEntry::Type(definition) => {
                    if catalog
                        .types
                        .insert(definition.name.clone(), definition)
                        .is_some()
                    {
                        return Err(Error::new("E_STORAGE", "duplicate durable type name"));
                    }
                }
                DurableCatalogEntry::Table(table) => {
                    if objects.contains_key(&table.name) || catalog.types.contains_key(&table.name)
                    {
                        return Err(Error::new(
                            "E_STORAGE",
                            format!("duplicate durable schema name '{}'", table.name),
                        ));
                    }
                    objects.insert(
                        table.name.clone(),
                        DbObject::Table(Table {
                            id: table.id,
                            name: table.name,
                            schema: table.schema,
                            rows: Vec::new(),
                            next_row_id: table.next_row_id,
                            row_type: table.row_type,
                            primary_key: table.primary_key,
                        }),
                    );
                }
                DurableCatalogEntry::Index { table, definition } => {
                    let column = definition.column.clone();
                    if index_definitions
                        .entry(table)
                        .or_default()
                        .insert(column, definition)
                        .is_some()
                    {
                        return Err(Error::new(
                            "E_STORAGE",
                            "duplicate durable index definition",
                        ));
                    }
                }
            }
        }
        if meta.next_catalog_id <= max_id {
            return Err(Error::new(
                "E_STORAGE",
                "catalog next ID does not exceed every durable object ID",
            ));
        }
        catalog.restore_next_id(meta.next_catalog_id)?;
        catalog.validate_finite_types().map_err(|error| {
            Error::new(
                "E_STORAGE",
                format!(
                    "durable catalog contains an invalid type cycle: {}",
                    error.message
                ),
            )
        })?;
        for (table_name, definitions) in &index_definitions {
            let Some(DbObject::Table(table)) = objects.get(table_name) else {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("index references unknown table '{table_name}'"),
                ));
            };
            for definition in definitions.values() {
                if definition.table_id != table.id {
                    return Err(Error::new(
                        "E_STORAGE",
                        format!("index '{}' has the wrong table ID", definition.column),
                    ));
                }
            }
        }
        rows.sort_by_key(|(table_id, row_id, _)| (*table_id, *row_id));
        let table_names_by_id = objects
            .iter()
            .map(|(name, object)| match object {
                DbObject::Table(table) => (table.id, name.clone()),
            })
            .collect::<BTreeMap<_, _>>();
        let mut decoded_rows: BTreeMap<String, Vec<Row>> = BTreeMap::new();
        let mut previous_row_id: BTreeMap<u64, RowId> = BTreeMap::new();
        for (table_id, row_id, bytes) in rows {
            let table_name = table_names_by_id.get(&table_id).ok_or_else(|| {
                Error::new(
                    "E_STORAGE",
                    format!("row references unknown table ID {table_id}"),
                )
            })?;
            if previous_row_id
                .insert(table_id, row_id)
                .is_some_and(|previous| row_id <= previous)
            {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("table '{table_name}' has duplicate or unordered row IDs"),
                ));
            }
            let Some(DbObject::Table(table)) = objects.get(table_name) else {
                unreachable!()
            };
            let ty = table
                .row_type
                .map(ScalarType::Ref)
                .unwrap_or_else(|| ScalarType::Record(table.schema.clone()));
            let value = crate::codec::decode_value(&catalog, &ty, &bytes)?;
            let Value::Record(fields) = value.unwrapped() else {
                return Err(Error::new("E_STORAGE", "durable row is not a record"));
            };
            decoded_rows
                .entry(table_name.clone())
                .or_default()
                .push(Row {
                    id: row_id,
                    fields: fields.clone(),
                });
        }
        for (table_name, rows) in decoded_rows {
            let Some(DbObject::Table(table)) = objects.get_mut(&table_name) else {
                unreachable!()
            };
            table.rows = rows;
            let inferred_next = table.rows.last().map_or(0, |row| row.id.saturating_add(1));
            if table.next_row_id == 0 {
                // Catalogs written before stable RowIds did not persist an
                // allocation cursor. Their row keys were contiguous from zero.
                table.next_row_id = inferred_next;
            } else if table.next_row_id < inferred_next {
                return Err(Error::new(
                    "E_STORAGE",
                    format!("table '{table_name}' has a row ID beyond its allocation cursor"),
                ));
            }
        }
        let mut database = Self {
            objects,
            indexes: BTreeMap::new(),
            index_definitions,
            catalog,
            sequence: meta.sequence,
            schema_revision: meta.schema_revision,
            migration_history,
        };
        crate::migration::validate_history(&database.migration_history)?;
        if let Some(head) = database.migration_history.last()
            && (head.schema_revision != database.schema_revision
                || head.schema_hash != meta.schema_hash)
        {
            return Err(Error::new(
                "E_STORAGE",
                "migration ledger head does not match the durable schema",
            ));
        }
        database.rebuild_indexes()?;
        if database.schema_info().hash != meta.schema_hash {
            return Err(Error::new(
                "E_STORAGE",
                "durable schema hash does not match the catalog",
            ));
        }
        Ok(database)
    }

    pub fn schema_text(&self) -> String {
        let mut lines = Vec::new();
        // Definitions were registered in dependency order; names need not sort that way.
        let mut definitions = self.catalog.types.values().collect::<Vec<_>>();
        definitions.sort_by_key(|d| d.id);
        for d in definitions {
            match &d.ty {
                ScalarType::Record(fields) => {
                    lines.push(format!("type {} =", d.name));
                    for field in fields {
                        lines.push(format!("  {}", self.catalog.describe_column(field)));
                    }
                }
                ScalarType::Enum(def) => {
                    lines.push(format!("type {} =", d.name));
                    for (i, variant) in def.variants.iter().enumerate() {
                        lines.push(format!(
                            "  {}{}",
                            if i == 0 { "" } else { "| " },
                            self.catalog.describe_variant(variant)
                        ));
                    }
                }
                _ => lines.push(format!(
                    "type {} = {}",
                    d.name,
                    self.catalog.describe(&d.ty)
                )),
            }
        }
        let mut tables = self.schema_tables();
        tables.sort_by_key(|table| table.id);
        for t in tables {
            let name = &t.name;
            let row = t
                .row_type
                .and_then(|id| self.catalog.definition(id).ok())
                .map(|d| d.name.clone());
            if let Some(row) = row {
                lines.push(format!("table {name} {row}"));
            } else {
                let columns = t
                    .schema
                    .iter()
                    .map(|c| self.catalog.describe_column(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("create table {name} ({columns})"));
            }
            if let Some(key) = &t.primary_key {
                lines.push(format!("  key {key}"));
            }
        }
        let mut indexes = self.schema_indexes();
        indexes.sort_by_key(|(_, definition)| definition.id);
        for (table, definition) in indexes {
            if self
                .table(table)
                .ok()
                .and_then(|table| table.primary_key.as_deref())
                == Some(definition.column.as_str())
            {
                continue;
            }
            lines.push(format!("create index {table} ({})", definition.column));
        }
        lines.join("\n")
    }
}

fn type_reaches(catalog: &Catalog, ty: &ScalarType, target: u64, seen: &mut BTreeSet<u64>) -> bool {
    match ty {
        ScalarType::Ref(id) => {
            *id == target
                || seen.insert(*id)
                    && catalog
                        .definition(*id)
                        .is_ok_and(|definition| type_reaches(catalog, &definition.ty, target, seen))
        }
        ScalarType::Record(fields) => fields
            .iter()
            .any(|field| type_reaches(catalog, &field.ty, target, seen)),
        ScalarType::Enum(sum) => sum.variants.iter().any(|variant| {
            variant
                .args
                .iter()
                .any(|argument| type_reaches(catalog, argument, target, seen))
        }),
        ScalarType::Tuple(items) => items
            .iter()
            .any(|item| type_reaches(catalog, item, target, seen)),
        ScalarType::Option(inner) | ScalarType::List(inner) => {
            type_reaches(catalog, inner, target, seen)
        }
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Named(_) => false,
    }
}

fn apply_returning(
    response: &mut QueryResponse,
    returning: Option<BoundReturning>,
    rows: Vec<BTreeMap<String, Value>>,
) {
    if let Some(returning) = returning {
        response.columns = returning.columns;
        response.rows = rows;
    }
}

fn row_field<'a>(row: &'a BTreeMap<String, Value>, path: &str) -> Option<&'a Value> {
    if let Some(value) = row.get(path) {
        return Some(value);
    }
    let (head, tail) = path.split_once('.')?;
    row.get(head)?.field(tail)
}

fn build_posting(rows: &[Row], column: &str) -> BTreeMap<String, Vec<RowId>> {
    let mut posting: BTreeMap<String, Vec<RowId>> = BTreeMap::new();
    for row in rows {
        if let Some(value) = row_field(&row.fields, column) {
            posting.entry(value.index_key()).or_default().push(row.id);
        }
    }
    posting
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('.'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn set_row_field(row: &mut BTreeMap<String, Value>, path: &str, value: Value) -> Result<()> {
    let mut components = path.split('.');
    let head = components.next().unwrap_or_default();
    let tail = components.collect::<Vec<_>>();
    let field = row
        .get_mut(head)
        .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{path}'")))?;
    set_nested_field(field, &tail, value, path)
}

fn set_nested_field(
    current: &mut Value,
    path: &[&str],
    new_value: Value,
    full_path: &str,
) -> Result<()> {
    if path.is_empty() {
        *current = new_value;
        return Ok(());
    }
    match current {
        Value::Named { value, .. } => set_nested_field(value, path, new_value, full_path),
        Value::Record(fields) => {
            let next = fields
                .get_mut(path[0])
                .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{full_path}'")))?;
            set_nested_field(next, &path[1..], new_value, full_path)
        }
        _ => Err(Error::new(
            "E_FIELD",
            format!("field path '{full_path}' does not pass through a record"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execute(database: &mut Database, source: &str) {
        for statement in crate::syntax::parse(source).unwrap() {
            database.execute(statement.statement).unwrap();
        }
    }

    fn table<'a>(database: &'a Database, name: &str) -> &'a Table {
        let DbObject::Table(table) = database.objects.get(name).unwrap();
        table
    }

    #[test]
    fn bulk_insert_validates_before_rows_indexes_or_cursor_change() {
        let mut database = Database::default();
        execute(
            &mut database,
            "type Entry =\n  id int\ntable entries Entry\n  key id\ninsert entries {id = 1}",
        );
        let before_rows = table(&database, "entries").rows.clone();
        let before_cursor = table(&database, "entries").next_row_id;
        let before_indexes = database.indexes.get("entries").cloned().unwrap();

        let statement = crate::syntax::parse("insert many entries [{id = 2}, {id = 1}]")
            .unwrap()
            .remove(0)
            .statement;
        let error = database.execute(statement).unwrap_err();
        assert_eq!(error.code, "E_CONSTRAINT");
        assert_eq!(
            serde_json::to_value(&table(&database, "entries").rows).unwrap(),
            serde_json::to_value(&before_rows).unwrap()
        );
        assert_eq!(table(&database, "entries").next_row_id, before_cursor);
        assert_eq!(database.indexes["entries"], before_indexes);

        execute(&mut database, "insert many entries [{id = 2}, {id = 3}]");
        assert_eq!(
            table(&database, "entries")
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(table(&database, "entries").next_row_id, 3);
        assert_eq!(database.indexes["entries"]["id"].values().count(), 3);
    }

    #[test]
    fn row_ids_survive_gaps_indexes_and_durable_round_trips() {
        let mut database = Database::default();
        execute(
            &mut database,
            "create table entries (id int)\ncreate index entries (id)\ninsert entries {id = 1}\ninsert entries {id = 2}",
        );
        let Some(DbObject::Table(entries)) = database.objects.get_mut("entries") else {
            unreachable!()
        };
        entries.rows.remove(0);
        database.rebuild_indexes().unwrap();
        execute(&mut database, "insert entries {id = 3}");

        assert_eq!(
            table(&database, "entries")
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(table(&database, "entries").next_row_id, 3);
        let indexed = database
            .execute(crate::query::parse_statement("from entries | filter id == 3").unwrap())
            .unwrap();
        assert_eq!(indexed.rows.len(), 1);

        let restored = Database::from_durable(
            database.durable_meta(),
            database.durable_catalog_entries(),
            database.durable_rows().unwrap(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            table(&restored, "entries")
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(table(&restored, "entries").next_row_id, 3);
    }

    #[test]
    fn transitional_snapshots_receive_ordered_row_ids() {
        let mut database = Database::default();
        execute(
            &mut database,
            "create table entries (id int)\ninsert entries {id = 1}\ninsert entries {id = 2}",
        );
        let mut json = serde_json::to_value(database).unwrap();
        let table_json = json["objects"]["entries"].as_object_mut().unwrap();
        table_json.remove("next_row_id");
        for row in table_json["rows"].as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("id");
        }
        let mut restored: Database = serde_json::from_value(json).unwrap();
        restored.rebuild_indexes().unwrap();
        execute(&mut restored, "insert entries {id = 3}");

        assert_eq!(
            table(&restored, "entries")
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(table(&restored, "entries").next_row_id, 3);
    }

    #[test]
    fn aggregate_group_limits_reject_each_bounded_resource() {
        assert_eq!(
            check_group_limits(MAX_GROUPS + 1, 1, 0).unwrap_err().code,
            "E_LIMIT"
        );
        assert_eq!(
            check_group_limits(4_000, MAX_AGGREGATE_OUTPUTS, 0)
                .unwrap_err()
                .code,
            "E_LIMIT"
        );
        assert_eq!(
            check_group_limits(1, 1, MAX_GROUP_WORKING_BYTES + 1)
                .unwrap_err()
                .code,
            "E_LIMIT"
        );
        check_group_limits(1, MAX_AGGREGATE_OUTPUTS, MAX_GROUP_WORKING_BYTES).unwrap();
    }
}
