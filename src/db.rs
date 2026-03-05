use std::collections::BTreeMap;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::model::{DbObject, Row, Table, Value};
use crate::query::{
    CmpOp, CreateIndexStmt, CreateTableStmt, InsertStmt, Pipeline, Predicate, Stage, Statement,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct QueryResponse {
    pub ok: bool,
    pub message: String,
    pub rows: Vec<BTreeMap<String, Value>>,
}

impl QueryResponse {
    pub fn ok_message(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            rows: Vec::new(),
        }
    }

    pub fn ok_rows(rows: Vec<BTreeMap<String, Value>>) -> Self {
        Self {
            ok: true,
            message: format!("{} row(s)", rows.len()),
            rows,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            rows: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Database {
    objects: BTreeMap<String, DbObject>,
    #[serde(default)]
    indexes: BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<usize>>>>,
}

impl Database {
    pub fn execute(&mut self, stmt: Statement) -> QueryResponse {
        match stmt {
            Statement::CreateTable(s) => self.create_table(s),
            Statement::CreateIndex(s) => self.create_index(s),
            Statement::Insert(s) => self.insert(s),
            Statement::Pipeline(p) => self.query(p),
        }
    }

    fn create_table(&mut self, stmt: CreateTableStmt) -> QueryResponse {
        if self.objects.contains_key(&stmt.table) {
            return QueryResponse::err(format!("table '{}' already exists", stmt.table));
        }

        let table = Table {
            name: stmt.table.clone(),
            schema: stmt.columns,
            rows: Vec::new(),
        };

        self.objects
            .insert(stmt.table.clone(), DbObject::Table(table));
        QueryResponse::ok_message(format!("table '{}' created", stmt.table))
    }

    fn create_index(&mut self, stmt: CreateIndexStmt) -> QueryResponse {
        let Some(DbObject::Table(table)) = self.objects.get(&stmt.table) else {
            return QueryResponse::err(format!("table '{}' not found", stmt.table));
        };

        if !table.schema.iter().any(|c| c.name == stmt.column) {
            return QueryResponse::err(format!(
                "column '{}' not found in table '{}'",
                stmt.column, stmt.table
            ));
        }

        if self
            .indexes
            .get(&stmt.table)
            .and_then(|cols| cols.get(&stmt.column))
            .is_some()
        {
            return QueryResponse::err(format!(
                "index on '{}.{}' already exists",
                stmt.table, stmt.column
            ));
        }

        let mut posting = BTreeMap::<String, Vec<usize>>::new();
        for (row_id, row) in table.rows.iter().enumerate() {
            let value = row.fields.get(&stmt.column).unwrap_or(&Value::Null);
            let key = index_key(value);
            posting.entry(key).or_default().push(row_id);
        }

        self.indexes
            .entry(stmt.table.clone())
            .or_default()
            .insert(stmt.column.clone(), posting);

        QueryResponse::ok_message(format!("index created on '{}.{}'", stmt.table, stmt.column))
    }

    fn insert(&mut self, stmt: InsertStmt) -> QueryResponse {
        let table_name = stmt.table.clone();
        let Some(DbObject::Table(table)) = self.objects.get_mut(&stmt.table) else {
            return QueryResponse::err(format!("table '{}' not found", stmt.table));
        };

        let mut incoming = BTreeMap::new();
        for (k, v) in stmt.values {
            incoming.insert(k, v);
        }

        let mut row_fields = BTreeMap::new();
        for col in &table.schema {
            let raw = incoming.remove(&col.name).unwrap_or(Value::Null);
            match raw.coerce_to(&col.ty) {
                Ok(v) => {
                    row_fields.insert(col.name.clone(), v);
                }
                Err(e) => {
                    return QueryResponse::err(format!(
                        "insert failed for column '{}': {}",
                        col.name, e
                    ));
                }
            }
        }

        if !incoming.is_empty() {
            let extra = incoming.keys().cloned().collect::<Vec<_>>().join(", ");
            return QueryResponse::err(format!("unknown column(s): {extra}"));
        }

        let inserted_fields = row_fields.clone();
        table.rows.push(Row { fields: row_fields });
        let row_id = table.rows.len() - 1;

        if let Some(table_indexes) = self.indexes.get_mut(&table_name) {
            for (column, posting) in table_indexes.iter_mut() {
                let value = inserted_fields.get(column).unwrap_or(&Value::Null);
                let key = index_key(value);
                posting.entry(key).or_default().push(row_id);
            }
        }

        QueryResponse::ok_message(format!("inserted into '{}'", stmt.table))
    }

    fn query(&self, pipeline: Pipeline) -> QueryResponse {
        let Some(DbObject::Table(table)) = self.objects.get(&pipeline.from) else {
            return QueryResponse::err(format!("table '{}' not found", pipeline.from));
        };

        let mut rows = table
            .rows
            .iter()
            .enumerate()
            .map(|(row_id, row)| (row_id, row.fields.clone()))
            .collect::<Vec<_>>();

        let table_indexes = self.indexes.get(&pipeline.from);

        for stage in pipeline.stages {
            match stage {
                Stage::Filter(pred) => {
                    if let Some(allowed_ids) = indexed_row_ids(table_indexes, &pred) {
                        rows.retain(|(row_id, _)| allowed_ids.contains(row_id));
                    }
                    rows.retain(|row| evaluate_predicate(row, &pred));
                }
                Stage::Select(columns) => {
                    rows = rows
                        .into_iter()
                        .map(|(row_id, row)| {
                            let mut out = BTreeMap::new();
                            for col in &columns {
                                out.insert(
                                    col.clone(),
                                    row.get(col).cloned().unwrap_or(Value::Null),
                                );
                            }
                            (row_id, out)
                        })
                        .collect();
                }
                Stage::Limit(n) => {
                    rows.truncate(n);
                }
            }
        }

        QueryResponse::ok_rows(rows.into_iter().map(|(_, row)| row).collect())
    }
}

fn indexed_row_ids(
    table_indexes: Option<&BTreeMap<String, BTreeMap<String, Vec<usize>>>>,
    pred: &Predicate,
) -> Option<HashSet<usize>> {
    if !matches!(pred.op, CmpOp::Eq) {
        return None;
    }

    let posting = table_indexes?
        .get(&pred.column)?
        .get(&index_key(&pred.value))?
        .iter()
        .copied()
        .collect::<HashSet<_>>();

    Some(posting)
}

fn index_key(value: &Value) -> String {
    match value {
        Value::Int(v) => format!("int:{v}"),
        Value::Float(v) => format!("float:{}", v.to_bits()),
        Value::Bool(v) => format!("bool:{v}"),
        Value::Text(v) => format!("text:{v}"),
        Value::Enum(v) => format!(
            "enum:{}",
            serde_json::to_string(v).unwrap_or_else(|_| "<invalid>".to_string())
        ),
        Value::Null => "null".to_string(),
    }
}

fn evaluate_predicate(row: &(usize, BTreeMap<String, Value>), pred: &Predicate) -> bool {
    let lhs = row.1.get(&pred.column).unwrap_or(&Value::Null);
    let rhs = &pred.value;

    match pred.op {
        CmpOp::Eq => lhs.cmp_eq(rhs),
        CmpOp::Ne => !lhs.cmp_eq(rhs),
        CmpOp::Gt => lhs
            .cmp_ord(rhs)
            .map(|o| o == std::cmp::Ordering::Greater)
            .unwrap_or(false),
        CmpOp::Gte => lhs
            .cmp_ord(rhs)
            .map(|o| o == std::cmp::Ordering::Greater || o == std::cmp::Ordering::Equal)
            .unwrap_or(false),
        CmpOp::Lt => lhs
            .cmp_ord(rhs)
            .map(|o| o == std::cmp::Ordering::Less)
            .unwrap_or(false),
        CmpOp::Lte => lhs
            .cmp_ord(rhs)
            .map(|o| o == std::cmp::Ordering::Less || o == std::cmp::Ordering::Equal)
            .unwrap_or(false),
    }
}
