use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{Catalog, Column, DbObject, Row, ScalarType, Table, Value};
use crate::query::{CmpOp, Pipeline, Predicate, Stage, Statement};

type Indexes = BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<usize>>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseColumn {
    pub name: String,
    pub ty: String,
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
    pub catalog: Catalog,
    #[serde(default)]
    pub sequence: u64,
}

impl Database {
    pub fn execute(&mut self, stmt: Statement) -> Result<QueryResponse> {
        match stmt {
            Statement::DefineType { name, ty } => {
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
            Statement::Insert { table, values } => self.insert(&table, values),
            Statement::Pipeline(pipeline) => self.query(pipeline),
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
        self.objects.insert(
            name.clone(),
            DbObject::Table(Table {
                name: name.clone(),
                schema: columns,
                rows: Vec::new(),
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
        let table = self.table(name)?;
        self.catalog.field_type(&table.schema, column)?;
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
        let mut posting: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (id, row) in table.rows.iter().enumerate() {
            if let Some(value) = row_field(&row.fields, column) {
                posting.entry(value.index_key()).or_default().push(id);
            }
        }
        self.indexes
            .entry(name.into())
            .or_default()
            .insert(column.into(), posting);
        Ok(QueryResponse::ok_message(format!(
            "index created on '{name}.{column}'"
        )))
    }

    fn insert(&mut self, name: &str, values: Value) -> Result<QueryResponse> {
        let table = self.table(name)?;
        let ty = table
            .row_type
            .map(ScalarType::Ref)
            .unwrap_or_else(|| ScalarType::Record(table.schema.clone()));
        let value = self.catalog.coerce(&values, &ty, name)?;
        let Value::Record(fields) = value.unwrapped() else {
            return Err(Error::new("E_TYPE", "insert requires a complete record"));
        };
        let fields = fields.clone();
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
        let id = table.rows.len();
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
        table.rows.push(Row { fields });
        Ok(QueryResponse::ok_message(format!("inserted into '{name}'")))
    }

    fn query(&self, mut pipeline: Pipeline) -> Result<QueryResponse> {
        let table = self.table(&pipeline.from)?;
        let mut schema = table.schema.clone();
        // Validate and bind every stage before touching any rows, including empty tables.
        for stage in &mut pipeline.stages {
            match stage {
                Stage::Filter(pred) => {
                    let ty = self.catalog.field_type(&schema, &pred.column)?;
                    pred.value = self.catalog.coerce(&pred.value, ty, &pred.column)?;
                    if !matches!(pred.op, CmpOp::Eq | CmpOp::Ne) && !self.orderable(ty)? {
                        return Err(Error::new(
                            "E_TYPE",
                            format!("field '{}' has no ordering", pred.column),
                        ));
                    }
                }
                Stage::Select(columns) => {
                    schema = columns
                        .iter()
                        .map(|name| {
                            Ok(Column {
                                name: name.clone(),
                                ty: self.catalog.field_type(&schema, name)?.clone(),
                                id: 0,
                            })
                        })
                        .collect::<Result<_>>()?;
                }
                Stage::Sort { column, .. } => {
                    let ty = self.catalog.field_type(&schema, column)?;
                    if !self.orderable(ty)? {
                        return Err(Error::new(
                            "E_TYPE",
                            format!("field '{column}' has no ordering"),
                        ));
                    }
                }
                Stage::Limit(_) => {}
            }
        }
        let candidates = if let Some(Stage::Filter(pred)) = pipeline.stages.first() {
            if matches!(pred.op, CmpOp::Eq) {
                self.indexes
                    .get(&pipeline.from)
                    .and_then(|cols| cols.get(&pred.column))
                    .map(|posting| {
                        posting
                            .get(&pred.value.index_key())
                            .cloned()
                            .unwrap_or_default()
                    })
            } else {
                None
            }
        } else {
            None
        };
        let mut rows = match candidates {
            Some(ids) => ids
                .into_iter()
                .filter_map(|id| table.rows.get(id).map(|r| r.fields.clone()))
                .collect::<Vec<_>>(),
            None => table.rows.iter().map(|r| r.fields.clone()).collect(),
        };
        for stage in pipeline.stages {
            match stage {
                Stage::Filter(pred) => rows.retain(|row| evaluate(row, &pred)),
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
                Stage::Sort { column, descending } => rows.sort_by(|a, b| {
                    let order = row_field(a, &column)
                        .zip(row_field(b, &column))
                        .and_then(|(a, b)| a.cmp_ord(b))
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if descending { order.reverse() } else { order }
                }),
                Stage::Limit(n) => rows.truncate(n),
            }
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
        for (name, DbObject::Table(table)) in &self.objects {
            if let Some(key) = &table.primary_key {
                keys.insert((name.clone(), key.clone()));
            }
        }
        self.indexes.clear();
        for (table, column) in keys {
            self.create_index(&table, &column)?;
        }
        Ok(())
    }

    pub fn table_names(&self) -> Vec<String> {
        self.objects.keys().cloned().collect()
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
                        lines.push(format!(
                            "  {} {}",
                            field.name,
                            self.catalog.describe(&field.ty)
                        ));
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
        for (name, DbObject::Table(t)) in &self.objects {
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
                    .map(|c| format!("{} {}", c.name, self.catalog.describe(&c.ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("create table {name} ({columns})"));
            }
            if let Some(key) = &t.primary_key {
                lines.push(format!("  key {key}"));
            }
        }
        lines.join("\n")
    }
}

fn row_field<'a>(row: &'a BTreeMap<String, Value>, path: &str) -> Option<&'a Value> {
    if let Some(value) = row.get(path) {
        return Some(value);
    }
    let (head, tail) = path.split_once('.')?;
    row.get(head)?.field(tail)
}

fn evaluate(row: &BTreeMap<String, Value>, pred: &Predicate) -> bool {
    let Some(lhs) = row_field(row, &pred.column) else {
        return false;
    };
    match pred.op {
        CmpOp::Eq => lhs.cmp_eq(&pred.value),
        CmpOp::Ne => !lhs.cmp_eq(&pred.value),
        CmpOp::Gt => lhs.cmp_ord(&pred.value) == Some(std::cmp::Ordering::Greater),
        CmpOp::Lt => lhs.cmp_ord(&pred.value) == Some(std::cmp::Ordering::Less),
        CmpOp::Gte => matches!(
            lhs.cmp_ord(&pred.value),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        CmpOp::Lte => matches!(
            lhs.cmp_ord(&pred.value),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}
