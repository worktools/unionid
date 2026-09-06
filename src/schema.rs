use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::db::{Database, SchemaInfo};
use crate::error::{Error, Result};
use crate::model::{Catalog, Column, ScalarType, Table, TypeDefinition};
use crate::query::Statement;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaCheck {
    pub schema: SchemaInfo,
    pub normalized: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaDiffOperation {
    pub description: String,
    pub destructive: bool,
    pub requires_input: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaDiff {
    pub current_schema: SchemaInfo,
    pub target_declaration: SchemaInfo,
    pub normalized_schema: String,
    pub operations: Vec<SchemaDiffOperation>,
    pub impacts: Vec<SchemaDiffImpact>,
    pub warnings: Vec<String>,
    pub runnable: bool,
    pub migration_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaDiffImpact {
    pub type_name: String,
    pub tables: Vec<SchemaDiffTableImpact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaDiffTableImpact {
    pub table: String,
    pub rows: usize,
    pub indexes: usize,
}

pub fn check(source: &str) -> Result<SchemaCheck> {
    let database = parse(source)?;
    Ok(SchemaCheck {
        schema: database.schema_info(),
        normalized: database.schema_text(),
    })
}

pub(crate) fn parse(source: &str) -> Result<Database> {
    let statements = crate::syntax::parse(source)?;
    if statements.is_empty() {
        return Err(Error::new("E_SCHEMA", "schema file must not be empty"));
    }
    let mut database = Database::default();
    let mut phase = 0_u8;
    for located in statements {
        if !matches!(
            located.statement,
            Statement::DefineType { .. }
                | Statement::CreateTable { .. }
                | Statement::TypedTable { .. }
                | Statement::CreateIndex { .. }
        ) {
            return Err(Error::new(
                "E_SCHEMA",
                "schema files may contain only type, table, and index declarations",
            )
            .at(located.span));
        }
        let statement_phase = match &located.statement {
            Statement::DefineType { .. } => 0,
            Statement::CreateTable { .. } | Statement::TypedTable { .. } => 1,
            Statement::CreateIndex { .. } => 2,
            _ => unreachable!(),
        };
        if statement_phase < phase {
            return Err(Error::new(
                "E_SCHEMA",
                "schema declarations must place types before tables and indexes after tables",
            )
            .at(located.span));
        }
        phase = statement_phase;
        database
            .execute(located.statement)
            .map_err(|error| error.at(located.span))?;
    }
    database.advance_schema_revision()?;
    Ok(database)
}

pub(crate) fn diff(
    current: &Database,
    target_source: &str,
    id: &str,
    parent: Option<&str>,
) -> Result<SchemaDiff> {
    let target = parse(target_source)?;
    let mut generated = Vec::new();
    let mut todos = Vec::new();
    let mut warnings = Vec::new();

    diff_types(current, &target, &mut generated, &mut todos, &mut warnings);
    diff_tables(current, &target, &mut generated, &mut todos);
    generated.sort_by_key(|operation| operation_priority(&operation.description));
    let impacts = current
        .catalog
        .types
        .iter()
        .filter(|(name, definition)| {
            target
                .catalog
                .types
                .get(*name)
                .is_none_or(|target_definition| {
                    type_signature(&current.catalog, &definition.ty)
                        != type_signature(&target.catalog, &target_definition.ty)
                })
        })
        .filter_map(|(name, _)| {
            let tables = current
                .schema_type_impact(name)
                .into_iter()
                .map(|(table, rows, indexes)| SchemaDiffTableImpact {
                    table,
                    rows,
                    indexes,
                })
                .collect::<Vec<_>>();
            (!tables.is_empty()).then(|| SchemaDiffImpact {
                type_name: name.clone(),
                tables,
            })
        })
        .collect();

    let mut source = format!("migration {id}\n");
    if let Some(parent) = parent {
        source.push_str(&format!("  parent {parent}\n"));
    }
    for operation in generated.iter().chain(&todos) {
        source.push_str("  ");
        source.push_str(&operation.description);
        source.push('\n');
    }
    let operations = generated.into_iter().chain(todos).collect::<Vec<_>>();
    let runnable = operations.iter().all(|operation| !operation.requires_input);
    if runnable && !operations.is_empty() {
        let migration = crate::migration::MigrationFile::parse(source.clone())?;
        let mut candidate = current.clone();
        candidate.execute(Statement::Migration {
            name: migration.id,
            parent: migration.parent,
            steps: migration.steps,
        })?;
        candidate.advance_schema_revision()?;
        if candidate.schema_text() != target.schema_text() {
            return Err(Error::new(
                "E_SCHEMA_DIFF",
                "generated migration does not reproduce the normalized target schema; explicit edits are required",
            ));
        }
    }
    Ok(SchemaDiff {
        current_schema: current.schema_info(),
        target_declaration: target.schema_info(),
        normalized_schema: target.schema_text(),
        runnable,
        operations,
        impacts,
        warnings,
        migration_source: source,
    })
}

fn diff_types(
    current: &Database,
    target: &Database,
    generated: &mut Vec<SchemaDiffOperation>,
    todos: &mut Vec<SchemaDiffOperation>,
    warnings: &mut Vec<String>,
) {
    let current_names = current
        .catalog
        .types
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let target_names = target
        .catalog
        .types
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut removed = current_names
        .difference(&target_names)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut added = target_names
        .difference(&current_names)
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut rename_from = BTreeSet::new();
    let mut rename_to = BTreeSet::new();
    for from in &removed {
        let candidates = added
            .iter()
            .filter_map(|to| {
                let left = &current.catalog.types[from];
                let right = &target.catalog.types[to];
                (type_signature(&current.catalog, &left.ty)
                    == type_signature(&target.catalog, &right.ty))
                .then(|| to.clone())
            })
            .collect::<Vec<_>>();
        if !candidates.is_empty() {
            rename_from.insert(from.clone());
            rename_to.extend(candidates.iter().cloned());
            let target = if candidates.len() == 1 {
                candidates[0].clone()
            } else {
                format!("one of {}", candidates.join(", "))
            };
            todos.push(todo(format!(
                "todo confirm rename type {from} to {target} or replace it explicitly"
            )));
        }
    }
    removed.retain(|name| !rename_from.contains(name));
    added.retain(|name| !rename_to.contains(name));

    let mut added_definitions = added
        .iter()
        .map(|name| &target.catalog.types[name])
        .collect::<Vec<_>>();
    added_definitions.sort_by_key(|definition| definition.id);
    for definition in added_definitions {
        generated.push(operation(
            format!(
                "add type {} = {}",
                definition.name,
                target.catalog.describe(&definition.ty)
            ),
            false,
        ));
    }

    for name in current_names.intersection(&target_names) {
        diff_type(
            name,
            &current.catalog.types[name],
            &current.catalog,
            &target.catalog.types[name],
            &target.catalog,
            generated,
            todos,
            warnings,
        );
    }
    let mut removed_definitions = removed
        .iter()
        .map(|name| &current.catalog.types[name])
        .collect::<Vec<_>>();
    removed_definitions.sort_by_key(|definition| std::cmp::Reverse(definition.id));
    for definition in removed_definitions {
        generated.push(operation(format!("drop type {}", definition.name), true));
    }
}

#[allow(clippy::too_many_arguments)]
fn diff_type(
    name: &str,
    current: &TypeDefinition,
    current_catalog: &Catalog,
    target: &TypeDefinition,
    target_catalog: &Catalog,
    generated: &mut Vec<SchemaDiffOperation>,
    todos: &mut Vec<SchemaDiffOperation>,
    warnings: &mut Vec<String>,
) {
    if type_signature(current_catalog, &current.ty) == type_signature(target_catalog, &target.ty) {
        return;
    }
    match (&current.ty, &target.ty) {
        (ScalarType::Record(current_fields), ScalarType::Record(target_fields)) => diff_fields(
            name,
            current_fields,
            current_catalog,
            target_fields,
            target_catalog,
            generated,
            todos,
        ),
        (ScalarType::Enum(current_sum), ScalarType::Enum(target_sum)) => {
            let current_variants = current_sum
                .variants
                .iter()
                .map(|variant| (variant.name.as_str(), variant))
                .collect::<BTreeMap<_, _>>();
            let target_variants = target_sum
                .variants
                .iter()
                .map(|variant| (variant.name.as_str(), variant))
                .collect::<BTreeMap<_, _>>();
            let mut renamed_from = BTreeSet::new();
            let mut renamed_to = BTreeSet::new();
            for (from, old) in &current_variants {
                if target_variants.contains_key(from) {
                    continue;
                }
                let candidates = target_variants
                    .iter()
                    .filter(|(to, new)| {
                        !current_variants.contains_key(*to)
                            && old
                                .args
                                .iter()
                                .map(|ty| current_catalog.describe(ty))
                                .collect::<Vec<_>>()
                                == new
                                    .args
                                    .iter()
                                    .map(|ty| target_catalog.describe(ty))
                                    .collect::<Vec<_>>()
                    })
                    .map(|(to, _)| *to)
                    .collect::<Vec<_>>();
                if !candidates.is_empty() {
                    renamed_from.insert(*from);
                    renamed_to.extend(candidates.iter().copied());
                    let target = if candidates.len() == 1 {
                        candidates[0].to_string()
                    } else {
                        format!("one of {}", candidates.join(", "))
                    };
                    todos.push(todo(format!(
                        "todo confirm rename variant {name}.{from} to {target}"
                    )));
                }
            }
            for (variant, definition) in &target_variants {
                if renamed_to.contains(variant) {
                    continue;
                }
                match current_variants.get(variant) {
                    None => {
                        generated.push(operation(
                            format!(
                                "add variant {name}.{}",
                                target_catalog.describe_variant(definition)
                            ),
                            false,
                        ));
                        warnings.push(format!(
                            "adding variant '{name}.{variant}' can invalidate exhaustive client matches"
                        ));
                    }
                    Some(old)
                        if variant_signature(current_catalog, old)
                            != variant_signature(target_catalog, definition) =>
                    {
                        todos.push(todo(format!(
                            "todo change variant {name}.{variant} to {} using old -> value",
                            variant_payload(target_catalog, definition)
                        )));
                    }
                    _ => {}
                }
            }
            for variant in current_variants.keys() {
                if !target_variants.contains_key(variant) && !renamed_from.contains(variant) {
                    generated.push(operation(format!("drop variant {name}.{variant}"), true));
                }
            }
            let current_order = current_sum
                .variants
                .iter()
                .filter(|variant| target_variants.contains_key(variant.name.as_str()))
                .map(|variant| variant.name.as_str())
                .collect::<Vec<_>>();
            let target_order = target_sum
                .variants
                .iter()
                .filter(|variant| current_variants.contains_key(variant.name.as_str()))
                .map(|variant| variant.name.as_str())
                .collect::<Vec<_>>();
            if current_order != target_order {
                todos.push(todo(format!(
                    "todo preserve or explicitly migrate variant order for type {name}"
                )));
            }
        }
        _ => todos.push(todo(format!(
            "todo replace type {name} with an explicit compatible migration"
        ))),
    }
}

fn diff_fields(
    owner: &str,
    current: &[Column],
    current_catalog: &Catalog,
    target: &[Column],
    target_catalog: &Catalog,
    generated: &mut Vec<SchemaDiffOperation>,
    todos: &mut Vec<SchemaDiffOperation>,
) {
    let current_by_name = current
        .iter()
        .map(|field| (field.name.as_str(), field))
        .collect::<BTreeMap<_, _>>();
    let target_by_name = target
        .iter()
        .map(|field| (field.name.as_str(), field))
        .collect::<BTreeMap<_, _>>();
    let mut renamed_from = BTreeSet::new();
    let mut renamed_to = BTreeSet::new();
    for (from, old) in &current_by_name {
        if target_by_name.contains_key(from) {
            continue;
        }
        let candidates = target_by_name
            .iter()
            .filter(|(to, new)| {
                !current_by_name.contains_key(*to)
                    && type_signature(current_catalog, &old.ty)
                        == type_signature(target_catalog, &new.ty)
                    && value_signature(old.default.as_ref())
                        == value_signature(new.default.as_ref())
            })
            .map(|(to, _)| *to)
            .collect::<Vec<_>>();
        if !candidates.is_empty() {
            renamed_from.insert(*from);
            renamed_to.extend(candidates.iter().copied());
            let target = if candidates.len() == 1 {
                candidates[0].to_string()
            } else {
                format!("one of {}", candidates.join(", "))
            };
            todos.push(todo(format!(
                "todo confirm rename field {owner}.{from} to {target}"
            )));
        }
    }
    for (name, field) in &target_by_name {
        if renamed_to.contains(name) {
            continue;
        }
        match current_by_name.get(name) {
            None if field.default.is_some() => generated.push(operation(
                format!(
                    "add field {owner}.{}",
                    target_catalog.describe_column(field)
                ),
                false,
            )),
            None => todos.push(todo(format!(
                "todo add field {owner}.{name} {} = required_backfill_value",
                target_catalog.describe(&field.ty)
            ))),
            Some(old)
                if type_signature(current_catalog, &old.ty)
                    != type_signature(target_catalog, &field.ty) =>
            {
                todos.push(todo(format!(
                    "todo change field {owner}.{name} to {} using old -> value",
                    target_catalog.describe(&field.ty)
                )));
            }
            Some(old)
                if value_signature(old.default.as_ref())
                    != value_signature(field.default.as_ref()) =>
            {
                match &field.default {
                    Some(value) => generated.push(operation(
                        format!("change default {owner}.{name} to {}", value.source_text()),
                        false,
                    )),
                    None => {
                        generated.push(operation(format!("drop default {owner}.{name}"), false))
                    }
                }
            }
            _ => {}
        }
    }
    for name in current_by_name.keys() {
        if !target_by_name.contains_key(name) && !renamed_from.contains(name) {
            generated.push(operation(format!("drop field {owner}.{name}"), true));
        }
    }
    let current_order = current
        .iter()
        .filter(|field| target_by_name.contains_key(field.name.as_str()))
        .map(|field| field.name.as_str())
        .collect::<Vec<_>>();
    let target_order = target
        .iter()
        .filter(|field| current_by_name.contains_key(field.name.as_str()))
        .map(|field| field.name.as_str())
        .collect::<Vec<_>>();
    if current_order != target_order {
        todos.push(todo(format!(
            "todo preserve or explicitly migrate field order for type {owner}"
        )));
    }
}

fn diff_tables(
    current: &Database,
    target: &Database,
    generated: &mut Vec<SchemaDiffOperation>,
    todos: &mut Vec<SchemaDiffOperation>,
) {
    let current_tables = current
        .schema_tables()
        .into_iter()
        .map(|table| (table.name.as_str(), table))
        .collect::<BTreeMap<_, _>>();
    let target_tables = target
        .schema_tables()
        .into_iter()
        .map(|table| (table.name.as_str(), table))
        .collect::<BTreeMap<_, _>>();
    let mut renamed_from = BTreeSet::new();
    let mut renamed_to = BTreeSet::new();
    for (from, old) in &current_tables {
        if target_tables.contains_key(from) {
            continue;
        }
        let candidates = target_tables
            .iter()
            .filter(|(to, new)| {
                !current_tables.contains_key(*to)
                    && table_type_name(current, old) == table_type_name(target, new)
                    && old.primary_key == new.primary_key
            })
            .map(|(to, _)| *to)
            .collect::<Vec<_>>();
        if !candidates.is_empty() {
            renamed_from.insert(*from);
            renamed_to.extend(candidates.iter().copied());
            let target = if candidates.len() == 1 {
                candidates[0].to_string()
            } else {
                format!("one of {}", candidates.join(", "))
            };
            todos.push(todo(format!(
                "todo confirm rename table {from} to {target}"
            )));
        }
    }

    for (name, table) in &target_tables {
        if renamed_to.contains(name) {
            continue;
        }
        match current_tables.get(name) {
            None => match table_type_name(target, table) {
                Some(row_type) => generated.push(operation(
                    format!(
                        "add table {name} {row_type}{}",
                        table
                            .primary_key
                            .as_ref()
                            .map(|key| format!(" key {key}"))
                            .unwrap_or_default()
                    ),
                    false,
                )),
                None => todos.push(todo(format!(
                    "todo add legacy structural table {name} as a named record table"
                ))),
            },
            Some(old) => {
                if table_type_name(current, old) != table_type_name(target, table) {
                    todos.push(todo(format!(
                        "todo change row type for table {name} with an explicit migration"
                    )));
                }
                if old.primary_key != table.primary_key {
                    if old.primary_key.is_some() {
                        generated.push(operation(format!("drop key {name}"), true));
                    }
                    if let Some(key) = &table.primary_key {
                        generated.push(operation(format!("set key {name}.{key}"), false));
                    }
                }
            }
        }
    }
    for name in current_tables.keys() {
        if !target_tables.contains_key(name) && !renamed_from.contains(name) {
            generated.push(operation(format!("drop table {name}"), true));
        }
    }

    let current_indexes = current
        .schema_indexes()
        .into_iter()
        .map(|(table, definition)| {
            (
                format!("{table}.{}", definition.column),
                (table, definition.column.as_str()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let target_indexes = target
        .schema_indexes()
        .into_iter()
        .map(|(table, definition)| {
            (
                format!("{table}.{}", definition.column),
                (table, definition.column.as_str()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for (identity, (table, column)) in &target_indexes {
        if !renamed_to.contains(table)
            && !current_indexes.contains_key(identity)
            && target_tables[*table]
                .primary_key
                .as_deref()
                .is_none_or(|key| key != *column)
        {
            generated.push(operation(format!("add index {table}.{column}"), false));
        }
    }
    for (identity, (table, column)) in &current_indexes {
        if !renamed_from.contains(table)
            && !target_indexes.contains_key(identity)
            && target_tables.contains_key(table)
        {
            let remains_primary = target_tables[*table]
                .primary_key
                .as_deref()
                .is_some_and(|key| key == *column);
            if !remains_primary {
                generated.push(operation(format!("drop index {table}.{column}"), true));
            }
        }
    }
}

fn table_type_name<'a>(database: &'a Database, table: &Table) -> Option<&'a str> {
    table
        .row_type
        .and_then(|id| database.catalog.definition(id).ok())
        .map(|definition| definition.name.as_str())
}

fn type_signature(catalog: &Catalog, ty: &ScalarType) -> String {
    catalog.describe(ty)
}

fn variant_signature(catalog: &Catalog, variant: &crate::model::EnumVariantDef) -> String {
    catalog.describe_variant(variant)
}

fn variant_payload(catalog: &Catalog, variant: &crate::model::EnumVariantDef) -> String {
    match variant.args.as_slice() {
        [] => "()".into(),
        [one] => catalog.describe(one),
        many => format!(
            "({})",
            many.iter()
                .map(|ty| catalog.describe(ty))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn value_signature(value: Option<&crate::model::Value>) -> Option<String> {
    value.map(crate::model::Value::source_text)
}

fn operation(description: String, destructive: bool) -> SchemaDiffOperation {
    SchemaDiffOperation {
        description,
        destructive,
        requires_input: false,
    }
}

fn todo(description: String) -> SchemaDiffOperation {
    SchemaDiffOperation {
        description,
        destructive: false,
        requires_input: true,
    }
}

fn operation_priority(description: &str) -> u8 {
    if description.starts_with("drop key ") {
        0
    } else if description.starts_with("drop index ") {
        1
    } else if description.starts_with("add type ") {
        2
    } else if description.starts_with("drop table ") {
        4
    } else if description.starts_with("drop type ") {
        5
    } else if description.starts_with("add table ") {
        6
    } else if description.starts_with("set key ") {
        7
    } else if description.starts_with("add index ") {
        8
    } else {
        3
    }
}
