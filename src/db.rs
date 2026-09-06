use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::model::{
    Catalog, Column, DbObject, Row, RowId, ScalarType, Table, TypeDefinition, Value,
};
use crate::query::{Pipeline, Stage, Statement};

type Indexes = BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<RowId>>>>;

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
}

impl Database {
    pub fn execute(&mut self, stmt: Statement) -> Result<QueryResponse> {
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
        let mut posting: BTreeMap<String, Vec<RowId>> = BTreeMap::new();
        for row in &table.rows {
            if let Some(value) = row_field(&row.fields, column) {
                posting.entry(value.index_key()).or_default().push(row.id);
            }
        }
        Ok(posting)
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
        Ok(QueryResponse::ok_message(format!("inserted into '{name}'")))
    }

    fn query(&self, mut pipeline: Pipeline) -> Result<QueryResponse> {
        let table = self.table(&pipeline.from)?;
        let mut schema = table.schema.clone();
        // Validate and bind every stage before touching any rows, including empty tables.
        for stage in &mut pipeline.stages {
            match stage {
                Stage::Filter(expression) => {
                    crate::expression::bind(&self.catalog, &schema, expression)?
                }
                Stage::FilterMatch(pred) => crate::matching::bind(&self.catalog, &schema, pred)?,
                Stage::DeriveMatch(derive) => {
                    schema.push(crate::matching::bind_derive(
                        &self.catalog,
                        &schema,
                        derive,
                    )?);
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
        let candidates = if let Some(Stage::Filter(expression)) = pipeline.stages.first() {
            crate::expression::simple_index_equality(expression).and_then(|(column, value)| {
                self.indexes
                    .get(&pipeline.from)
                    .and_then(|cols| cols.get(column))
                    .map(|posting| posting.get(&value.index_key()).cloned().unwrap_or_default())
            })
        } else {
            None
        };
        let mut rows = match candidates {
            Some(ids) => ids
                .into_iter()
                .filter_map(|id| {
                    table
                        .rows
                        .binary_search_by_key(&id, |row| row.id)
                        .ok()
                        .map(|position| table.rows[position].fields.clone())
                })
                .collect::<Vec<_>>(),
            None => table.rows.iter().map(|r| r.fields.clone()).collect(),
        };
        for stage in pipeline.stages {
            match stage {
                Stage::Filter(expression) => {
                    let mut filtered = Vec::with_capacity(rows.len());
                    for row in rows {
                        if crate::expression::evaluate(&self.catalog, &expression, |path| {
                            row_field(&row, path)
                        })? {
                            filtered.push(row);
                        }
                    }
                    rows = filtered;
                }
                Stage::FilterMatch(pred) => {
                    let mut filtered = Vec::with_capacity(rows.len());
                    for row in rows {
                        if crate::matching::evaluate(&self.catalog, &row, &pred)? {
                            filtered.push(row);
                        }
                    }
                    rows = filtered;
                }
                Stage::DeriveMatch(derive) => {
                    for row in &mut rows {
                        let value = crate::matching::evaluate_derive(&self.catalog, row, &derive)?;
                        row.insert(derive.name.clone(), value);
                    }
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
                Stage::Sort(keys) => rows.sort_by(|a, b| {
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
                }),
                Stage::Take { offset, limit } => {
                    rows = rows.into_iter().skip(offset).take(limit).collect()
                }
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
            schema: None,
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

    pub(crate) fn from_durable(
        meta: DurableMeta,
        entries: Vec<DurableCatalogEntry>,
        mut rows: Vec<(u64, u64, Vec<u8>)>,
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
        };
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
                    .map(|c| self.catalog.describe_column(c))
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
}
