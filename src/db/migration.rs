use super::*;
use crate::model::{EnumValue, MAX_DEPTH};
use crate::query::{MigrationTransform, SchemaMigration};
use std::sync::Arc;

impl Database {
    pub(super) fn migrate(
        &mut self,
        name: &str,
        steps: Vec<SchemaMigration>,
    ) -> Result<QueryResponse> {
        for step in steps {
            self.apply_schema_migration(step)?;
        }
        Ok(QueryResponse::ok_message(format!(
            "migration '{name}' applied"
        )))
    }

    fn apply_schema_migration(&mut self, step: SchemaMigration) -> Result<()> {
        match step {
            SchemaMigration::AddType { name, ty } => {
                if self.objects.contains_key(&name) {
                    return Err(Error::new(
                        "E_SCHEMA",
                        format!("name '{name}' is already used by a table"),
                    ));
                }
                self.catalog.define(name, ty)
            }
            SchemaMigration::DropType { name } => self.drop_type(&name),
            SchemaMigration::AddTable {
                table,
                row_type,
                key,
            } => {
                let definition = self.catalog.types.get(&row_type).ok_or_else(|| {
                    Error::new("E_SCHEMA", format!("unknown row type '{row_type}'"))
                })?;
                let ScalarType::Record(columns) = self.catalog.underlying(&definition.ty)? else {
                    return Err(Error::new("E_TYPE", "a table's row type must be a record"));
                };
                self.create_table(table, columns.clone(), Some(definition.id), key)
                    .map(|_| ())
            }
            SchemaMigration::DropTable { table } => self.drop_table(&table),
            SchemaMigration::RenameTable { from, to } => self.rename_table(&from, to),
            SchemaMigration::RenameType { from, to } => self.rename_type(&from, to),
            SchemaMigration::AddField { owner, column } => self.add_field(&owner, column),
            SchemaMigration::DropField { owner, field } => self.drop_field(&owner, &field),
            SchemaMigration::ChangeDefault {
                owner,
                field,
                value,
            } => self.change_default(&owner, &field, value),
            SchemaMigration::DropDefault { owner, field } => self.drop_default(&owner, &field),
            SchemaMigration::RenameField { owner, from, to } => {
                self.rename_field(&owner, &from, to)
            }
            SchemaMigration::ChangeField {
                owner,
                field,
                ty,
                transform,
            } => self.change_field(&owner, &field, ty, transform),
            SchemaMigration::AddVariant { owner, name, args } => {
                self.add_variant(&owner, name, args)
            }
            SchemaMigration::DropVariant {
                owner,
                variant,
                transform,
            } => self.drop_variant(&owner, &variant, transform),
            SchemaMigration::RenameVariant { owner, from, to } => {
                self.rename_variant(&owner, &from, to)
            }
            SchemaMigration::ChangeVariant {
                owner,
                variant,
                args,
                transform,
            } => self.change_variant(&owner, &variant, args, transform),
            SchemaMigration::AddIndex {
                table,
                column,
                unique,
            } => self.create_index(&table, &column, unique).map(|_| ()),
            SchemaMigration::DropIndex { table, column } => self.drop_index(&table, &column),
            SchemaMigration::SetKey { table, column } => self.set_key(&table, &column),
            SchemaMigration::DropKey { table } => self.drop_key(&table),
        }
    }

    fn drop_type(&mut self, name: &str) -> Result<()> {
        let id = self
            .catalog
            .types
            .get(name)
            .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type '{name}'")))?
            .id;
        if let Some(reference) = self.type_reference(id) {
            return Err(Error::new(
                "E_MIGRATION",
                format!("cannot drop type '{name}'; it is referenced by {reference}"),
            ));
        }
        self.catalog.types.remove(name);
        Ok(())
    }

    fn drop_table(&mut self, name: &str) -> Result<()> {
        if self.objects.remove(name).is_none() {
            return Err(Error::new("E_TABLE", format!("table '{name}' not found")));
        }
        self.indexes.remove(name);
        self.index_definitions.remove(name);
        Ok(())
    }

    fn rename_table(&mut self, from: &str, to: String) -> Result<()> {
        if self.objects.contains_key(&to) || self.catalog.types.contains_key(&to) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("schema name '{to}' already exists"),
            ));
        }
        let Some(DbObject::Table(mut table)) = self.objects.remove(from) else {
            return Err(Error::new("E_TABLE", format!("table '{from}' not found")));
        };
        table.name = to.clone();
        self.objects.insert(to.clone(), DbObject::Table(table));
        if let Some(indexes) = self.indexes.remove(from) {
            self.indexes.insert(to.clone(), indexes);
        }
        if let Some(definitions) = self.index_definitions.remove(from) {
            self.index_definitions.insert(to, definitions);
        }
        Ok(())
    }

    fn rename_type(&mut self, from: &str, to: String) -> Result<()> {
        require_uppercase_name(&to, "type")?;
        if self.catalog.types.contains_key(&to) || self.objects.contains_key(&to) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("schema name '{to}' already exists"),
            ));
        }
        let mut definition = self
            .catalog
            .types
            .remove(from)
            .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type '{from}'")))?;
        definition.name = to.clone();
        self.catalog.types.insert(to, definition);
        Ok(())
    }

    fn add_field(&mut self, owner: &str, column: Column) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let ty = self.catalog.resolve(column.ty, 0)?;
        let owner_id = self.named_type_id(owner)?;
        let default = self.catalog.coerce(
            column.default.as_ref().ok_or_else(|| {
                Error::new(
                    "E_MIGRATION",
                    "add field requires an explicit default for backfill",
                )
            })?,
            &ty,
            &format!("default for field '{owner}.{}'", column.name),
        )?;
        let id = self.catalog.allocate()?;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            return Err(Error::new(
                "E_TYPE",
                format!("type '{owner}' is not a record"),
            ));
        };
        if fields.iter().any(|field| field.name == column.name) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("field '{owner}.{}' already exists", column.name),
            ));
        }
        fields.push(Column {
            name: column.name,
            ty,
            default: Some(default),
            id,
        });
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn drop_field(&mut self, owner: &str, field: &str) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let field_id = direct_record_field(&old_catalog, owner, field)?.id;
        if let Some((table, column)) =
            self.index_definitions
                .iter()
                .find_map(|(table, definitions)| {
                    definitions
                        .values()
                        .find(|definition| definition.field_path.contains(&field_id))
                        .map(|definition| (table, &definition.column))
                })
        {
            return Err(Error::new(
                "E_MIGRATION",
                format!(
                    "cannot drop field '{owner}.{field}' while index '{table}.{column}' references it; drop the key/index first"
                ),
            ));
        }
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            return Err(Error::new(
                "E_TYPE",
                format!("type '{owner}' is not a record"),
            ));
        };
        let index = fields
            .iter()
            .position(|column| column.name == field)
            .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{owner}.{field}'")))?;
        if fields.len() == 1 {
            return Err(Error::new(
                "E_SCHEMA",
                "record types must keep at least one field",
            ));
        }
        fields.remove(index);
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn change_default(&mut self, owner: &str, field: &str, value: Value) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let column = direct_record_field(&self.catalog, owner, field)?.clone();
        let default = self.catalog.coerce(
            &value,
            &column.ty,
            &format!("default for field '{owner}.{field}'"),
        )?;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            unreachable!()
        };
        fields
            .iter_mut()
            .find(|candidate| candidate.id == column.id)
            .unwrap()
            .default = Some(default);
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn drop_default(&mut self, owner: &str, field: &str) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let field_id = direct_record_field(&old_catalog, owner, field)?.id;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            unreachable!()
        };
        let column = fields
            .iter_mut()
            .find(|column| column.id == field_id)
            .unwrap();
        if column.default.take().is_none() {
            return Err(Error::new(
                "E_MIGRATION",
                format!("field '{owner}.{field}' has no default"),
            ));
        }
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn rename_field(&mut self, owner: &str, from: &str, to: String) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            return Err(Error::new(
                "E_TYPE",
                format!("type '{owner}' is not a record"),
            ));
        };
        if fields.iter().any(|field| field.name == to) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("field '{owner}.{to}' already exists"),
            ));
        }
        let column = fields
            .iter_mut()
            .find(|column| column.name == from)
            .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{owner}.{from}'")))?;
        column.name = to;
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn change_field(
        &mut self,
        owner: &str,
        field: &str,
        ty: ScalarType,
        mut transform: MigrationTransform,
    ) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let old_column = direct_record_field(&old_catalog, owner, field)?.clone();
        let mut new_ty = self.catalog.resolve(ty, 0)?;
        preserve_structural_ids(&old_column.ty, &mut new_ty);
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            unreachable!()
        };
        let column = fields
            .iter_mut()
            .find(|column| column.id == old_column.id)
            .unwrap();
        column.ty = new_ty.clone();
        self.catalog.validate_finite_types()?;
        let input = expose_binding_root(&old_catalog, &old_column.ty)?;
        crate::matching::bind_migration_result(
            &self.catalog,
            &new_ty,
            &mut transform.value,
            &transform.binding,
            input.clone(),
        )?;
        let new_default = old_column
            .default
            .as_ref()
            .map(|value| {
                crate::matching::evaluate_migration_result(
                    &self.catalog,
                    &new_ty,
                    &transform.value,
                    &transform.binding,
                    value,
                )
            })
            .transpose()?;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Record(fields) = &mut definition.ty else {
            unreachable!()
        };
        fields
            .iter_mut()
            .find(|column| column.id == old_column.id)
            .unwrap()
            .default = new_default;
        let rewrite = ValueRewrite::Field {
            owner_id,
            field_id: old_column.id,
            output: new_ty,
            transform,
        };
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), Some(&rewrite))
    }

    fn add_variant(&mut self, owner: &str, name: String, args: Vec<ScalarType>) -> Result<()> {
        require_uppercase_name(&name, "variant")?;
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let args = args
            .into_iter()
            .map(|ty| self.catalog.resolve(ty, 0))
            .collect::<Result<Vec<_>>>()?;
        let id = self.catalog.allocate()?;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Enum(enum_type) = &mut definition.ty else {
            return Err(Error::new(
                "E_TYPE",
                format!("type '{owner}' is not a sum type"),
            ));
        };
        if enum_type
            .variants
            .iter()
            .any(|variant| variant.name == name)
        {
            return Err(Error::new(
                "E_SCHEMA",
                format!("variant '{owner}.{name}' already exists"),
            ));
        }
        enum_type
            .variants
            .push(crate::model::EnumVariantDef { name, args, id });
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn drop_variant(
        &mut self,
        owner: &str,
        variant: &str,
        mut transform: Option<MigrationTransform>,
    ) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let old_variant = direct_variant(&old_catalog, owner, variant)?.clone();
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Enum(enum_type) = &mut definition.ty else {
            unreachable!()
        };
        if enum_type.variants.len() == 1 {
            return Err(Error::new(
                "E_SCHEMA",
                "sum types must keep at least one variant",
            ));
        }
        enum_type
            .variants
            .retain(|candidate| candidate.id != old_variant.id);
        let rewrite = if let Some(transform) = transform.as_mut() {
            let input = expanded_payload_type(&old_catalog, &old_variant.args)?;
            let output = ScalarType::Ref(owner_id);
            crate::matching::bind_migration_result(
                &self.catalog,
                &output,
                &mut transform.value,
                &transform.binding,
                input.clone(),
            )?;
            Some(ValueRewrite::RemovedVariant {
                owner_id,
                variant_id: old_variant.id,
                output,
                transform: transform.clone(),
            })
        } else {
            None
        };
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), rewrite.as_ref())
    }

    fn rename_variant(&mut self, owner: &str, from: &str, to: String) -> Result<()> {
        require_uppercase_name(&to, "variant")?;
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Enum(enum_type) = &mut definition.ty else {
            return Err(Error::new(
                "E_TYPE",
                format!("type '{owner}' is not a sum type"),
            ));
        };
        if enum_type.variants.iter().any(|variant| variant.name == to) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("variant '{owner}.{to}' already exists"),
            ));
        }
        let variant = enum_type
            .variants
            .iter_mut()
            .find(|variant| variant.name == from)
            .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown variant '{owner}.{from}'")))?;
        variant.name = to;
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), None)
    }

    fn change_variant(
        &mut self,
        owner: &str,
        variant: &str,
        args: Vec<ScalarType>,
        mut transform: MigrationTransform,
    ) -> Result<()> {
        let old_catalog = self.catalog.clone();
        let owner_id = self.named_type_id(owner)?;
        let old_variant = direct_variant(&old_catalog, owner, variant)?.clone();
        let mut args = args
            .into_iter()
            .map(|ty| self.catalog.resolve(ty, 0))
            .collect::<Result<Vec<_>>>()?;
        for (old, new) in old_variant.args.iter().zip(&mut args) {
            preserve_structural_ids(old, new);
        }
        let definition = self.catalog.types.get_mut(owner).unwrap();
        let ScalarType::Enum(enum_type) = &mut definition.ty else {
            unreachable!()
        };
        let changed = enum_type
            .variants
            .iter_mut()
            .find(|candidate| candidate.id == old_variant.id)
            .unwrap();
        changed.args = args.clone();
        self.catalog.validate_finite_types()?;
        let input = expanded_payload_type(&old_catalog, &old_variant.args)?;
        let output = payload_type(&args);
        crate::matching::bind_migration_result(
            &self.catalog,
            &output,
            &mut transform.value,
            &transform.binding,
            input.clone(),
        )?;
        let rewrite = ValueRewrite::Variant {
            owner_id,
            variant_id: old_variant.id,
            output,
            transform,
        };
        self.rewrite_after_catalog_change(&old_catalog, Some(owner_id), Some(&rewrite))
    }

    fn drop_index(&mut self, table: &str, column: &str) -> Result<()> {
        let source = self.table(table)?;
        if source.primary_key.as_deref() == Some(column) {
            return Err(Error::new(
                "E_MIGRATION",
                format!("index '{table}.{column}' enforces the primary key; drop the key first"),
            ));
        }
        let definitions = self.index_definitions.get_mut(table).ok_or_else(|| {
            Error::new(
                "E_INDEX",
                format!("index '{table}.{column}' does not exist"),
            )
        })?;
        if definitions.remove(column).is_none() {
            return Err(Error::new(
                "E_INDEX",
                format!("index '{table}.{column}' does not exist"),
            ));
        }
        if let Some(columns) = self.indexes.get_mut(table) {
            columns.remove(column);
        }
        Ok(())
    }

    fn set_key(&mut self, table: &str, column: &str) -> Result<()> {
        let source = self.table(table)?;
        let ty = self.catalog.field_type(&source.schema, column)?;
        if !matches!(
            self.catalog.underlying(ty)?,
            ScalarType::Int | ScalarType::Text | ScalarType::Uuid
        ) {
            return Err(Error::new(
                "E_TYPE",
                "primary keys require int, text, or uuid",
            ));
        }
        let mut seen = BTreeSet::new();
        for row in &source.rows {
            let value = row_field(&row.fields, column).ok_or_else(|| {
                Error::new("E_FIELD", format!("missing primary key '{table}.{column}'"))
            })?;
            if !seen.insert(value.index_key()) {
                return Err(Error::new(
                    "E_CONSTRAINT",
                    format!("duplicate primary key '{table}.{column}'"),
                ));
            }
        }
        if !self
            .index_definitions
            .get(table)
            .is_some_and(|definitions| definitions.contains_key(column))
        {
            self.create_index(table, column, false)?;
        }
        let Some(DbObject::Table(source)) = self.objects.get_mut(table) else {
            unreachable!()
        };
        source.primary_key = Some(column.into());
        Ok(())
    }

    fn drop_key(&mut self, table: &str) -> Result<()> {
        let Some(DbObject::Table(source)) = self.objects.get_mut(table) else {
            return Err(Error::new("E_TABLE", format!("table '{table}' not found")));
        };
        if source.primary_key.take().is_none() {
            return Err(Error::new(
                "E_MIGRATION",
                format!("table '{table}' has no primary key"),
            ));
        }
        Ok(())
    }

    fn named_type_id(&self, name: &str) -> Result<u64> {
        self.catalog
            .types
            .get(name)
            .map(|definition| definition.id)
            .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type '{name}'")))
    }

    fn type_reference(&self, target: u64) -> Option<String> {
        for definition in self.catalog.types.values() {
            if definition.id != target && type_contains_ref(&definition.ty, target) {
                return Some(format!("type '{}'", definition.name));
            }
        }
        for (name, object) in &self.objects {
            let DbObject::Table(table) = object;
            if table.row_type == Some(target)
                || table
                    .schema
                    .iter()
                    .any(|field| type_contains_ref(&field.ty, target))
            {
                return Some(format!("table '{name}'"));
            }
        }
        None
    }

    fn rewrite_after_catalog_change(
        &mut self,
        old_catalog: &Catalog,
        changed_type: Option<u64>,
        rewrite: Option<&ValueRewrite>,
    ) -> Result<()> {
        self.catalog.validate_finite_types()?;
        let mut old_tables = BTreeMap::new();
        for (name, object) in &self.objects {
            let DbObject::Table(table) = object;
            let old_ty = table
                .row_type
                .map(ScalarType::Ref)
                .unwrap_or_else(|| ScalarType::Record(table.schema.clone()));
            let affected = changed_type.is_none_or(|target| {
                type_reaches(old_catalog, &old_ty, target, &mut BTreeSet::new(), 0)
            });
            if affected {
                old_tables.insert(name.clone(), table.clone());
            }
        }
        self.refresh_table_schemas()?;
        for (name, old_table) in old_tables {
            let new_table = self.table(&name)?.clone();
            let old_ty = old_table
                .row_type
                .map(ScalarType::Ref)
                .unwrap_or_else(|| ScalarType::Record(old_table.schema.clone()));
            let new_ty = new_table
                .row_type
                .map(ScalarType::Ref)
                .unwrap_or_else(|| ScalarType::Record(new_table.schema.clone()));
            let mut rows = Vec::with_capacity(old_table.rows.len());
            for row in old_table.rows {
                let row_context = old_table
                    .primary_key
                    .as_ref()
                    .and_then(|key| row_field(&row.fields, key).map(|value| (key, value)))
                    .map_or_else(
                        || format!("table '{name}' RowId {}", row.id),
                        |(key, value)| {
                            format!(
                                "table '{name}' RowId {} ({} = {})",
                                row.id,
                                key,
                                value.source_text()
                            )
                        },
                    );
                let value = Value::Record(row.fields.clone());
                let migrated = migrate_value(
                    old_catalog,
                    &self.catalog,
                    &old_ty,
                    &new_ty,
                    &value,
                    &row_context,
                    None,
                    rewrite,
                    0,
                )?;
                let Value::Record(fields) = migrated.unwrapped() else {
                    return Err(Error::new("E_MIGRATION", "migrated row is not a record"));
                };
                rows.push(Row {
                    id: row.id,
                    fields: fields.clone(),
                });
            }
            let Some(DbObject::Table(table)) = self.objects.get_mut(&name) else {
                unreachable!()
            };
            table.rows = rows.into_iter().map(Arc::new).collect();
        }
        self.refresh_index_paths()?;
        self.indexes.clear();
        self.rebuild_indexes()?;
        for name in self.table_names() {
            self.validate_primary_keys(&name, &self.table(&name)?.rows)?;
            self.validate_unique_indexes(&name, &self.table(&name)?.rows)?;
        }
        Ok(())
    }

    fn refresh_table_schemas(&mut self) -> Result<()> {
        let updates = self
            .objects
            .iter()
            .filter_map(|(name, object)| match object {
                DbObject::Table(table) => table.row_type.map(|id| (name.clone(), id)),
            })
            .map(|(name, id)| {
                let row_type = ScalarType::Ref(id);
                let ScalarType::Record(fields) = self.catalog.underlying(&row_type)? else {
                    return Err(Error::new(
                        "E_MIGRATION",
                        format!("table '{name}' row type is no longer a record"),
                    ));
                };
                Ok((name, fields.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        for (name, fields) in updates {
            let Some(DbObject::Table(table)) = self.objects.get_mut(&name) else {
                unreachable!()
            };
            table.schema = fields;
        }
        Ok(())
    }

    fn refresh_index_paths(&mut self) -> Result<()> {
        let table_names = self.table_names();
        for table_name in table_names {
            let table = self.table(&table_name)?.clone();
            let primary_id = table.primary_key.as_ref().and_then(|key| {
                self.index_definitions
                    .get(&table_name)
                    .and_then(|definitions| definitions.get(key))
                    .map(|definition| definition.id)
            });
            let definitions = self
                .index_definitions
                .remove(&table_name)
                .unwrap_or_default();
            let mut refreshed = BTreeMap::new();
            for (_, mut definition) in definitions {
                let Some(column) =
                    field_path_name(&self.catalog, &table.schema, &definition.field_path)
                else {
                    continue;
                };
                definition.column = column.clone();
                refreshed.insert(column, definition);
            }
            let primary_key = primary_id.and_then(|id| {
                refreshed
                    .values()
                    .find(|definition| definition.id == id)
                    .map(|definition| definition.column.clone())
            });
            if let Some(key) = &primary_key {
                let ty = self.catalog.field_type(&table.schema, key)?;
                if !matches!(
                    self.catalog.underlying(ty)?,
                    ScalarType::Int | ScalarType::Text | ScalarType::Uuid
                ) {
                    return Err(Error::new(
                        "E_TYPE",
                        format!("primary key '{table_name}.{key}' must remain int, text, or uuid"),
                    ));
                }
            }
            self.index_definitions.insert(table_name.clone(), refreshed);
            let Some(DbObject::Table(table)) = self.objects.get_mut(&table_name) else {
                unreachable!()
            };
            table.primary_key = primary_key;
        }
        Ok(())
    }
}

#[derive(Clone)]
enum ValueRewrite {
    Field {
        owner_id: u64,
        field_id: u64,
        output: ScalarType,
        transform: MigrationTransform,
    },
    Variant {
        owner_id: u64,
        variant_id: u64,
        output: ScalarType,
        transform: MigrationTransform,
    },
    RemovedVariant {
        owner_id: u64,
        variant_id: u64,
        output: ScalarType,
        transform: MigrationTransform,
    },
}

fn require_uppercase_name(name: &str, kind: &str) -> Result<()> {
    if name.starts_with(|ch: char| ch.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(Error::new(
            "E_SCHEMA",
            format!("{kind} names must start with an uppercase letter"),
        ))
    }
}

fn direct_record_field<'a>(catalog: &'a Catalog, owner: &str, field: &str) -> Result<&'a Column> {
    let definition = catalog
        .types
        .get(owner)
        .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type '{owner}'")))?;
    let ScalarType::Record(fields) = &definition.ty else {
        return Err(Error::new(
            "E_TYPE",
            format!("type '{owner}' is not a record"),
        ));
    };
    fields
        .iter()
        .find(|column| column.name == field)
        .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{owner}.{field}'")))
}

fn direct_variant<'a>(
    catalog: &'a Catalog,
    owner: &str,
    variant: &str,
) -> Result<&'a crate::model::EnumVariantDef> {
    let definition = catalog
        .types
        .get(owner)
        .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type '{owner}'")))?;
    let ScalarType::Enum(enum_type) = &definition.ty else {
        return Err(Error::new(
            "E_TYPE",
            format!("type '{owner}' is not a sum type"),
        ));
    };
    enum_type
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown variant '{owner}.{variant}'")))
}

fn type_contains_ref(ty: &ScalarType, target: u64) -> bool {
    match ty {
        ScalarType::Ref(id) => *id == target,
        ScalarType::Record(fields) => fields
            .iter()
            .any(|column| type_contains_ref(&column.ty, target)),
        ScalarType::Enum(enum_type) => enum_type
            .variants
            .iter()
            .flat_map(|variant| &variant.args)
            .any(|ty| type_contains_ref(ty, target)),
        ScalarType::Tuple(items) => items.iter().any(|ty| type_contains_ref(ty, target)),
        ScalarType::Option(item) | ScalarType::List(item) => type_contains_ref(item, target),
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Uuid
        | ScalarType::Date
        | ScalarType::Timestamp
        | ScalarType::Duration
        | ScalarType::Decimal { .. }
        | ScalarType::Bytes
        | ScalarType::Named(_) => false,
    }
}

fn type_reaches(
    catalog: &Catalog,
    ty: &ScalarType,
    target: u64,
    seen: &mut BTreeSet<u64>,
    depth: usize,
) -> bool {
    if depth >= MAX_DEPTH {
        return false;
    }
    match ty {
        ScalarType::Ref(id) => {
            if *id == target {
                return true;
            }
            if !seen.insert(*id) {
                return false;
            }
            catalog.definition(*id).is_ok_and(|definition| {
                type_reaches(catalog, &definition.ty, target, seen, depth + 1)
            })
        }
        ScalarType::Record(fields) => fields
            .iter()
            .any(|field| type_reaches(catalog, &field.ty, target, seen, depth + 1)),
        ScalarType::Enum(enum_type) => enum_type
            .variants
            .iter()
            .flat_map(|variant| &variant.args)
            .any(|ty| type_reaches(catalog, ty, target, seen, depth + 1)),
        ScalarType::Tuple(items) => items
            .iter()
            .any(|ty| type_reaches(catalog, ty, target, seen, depth + 1)),
        ScalarType::Option(item) | ScalarType::List(item) => {
            type_reaches(catalog, item, target, seen, depth + 1)
        }
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Uuid
        | ScalarType::Date
        | ScalarType::Timestamp
        | ScalarType::Duration
        | ScalarType::Decimal { .. }
        | ScalarType::Bytes
        | ScalarType::Named(_) => false,
    }
}

// A transform must see the outer record so `old.field` can bind, while nested
// Ref nodes stay nominal so values such as `old.meta` retain their type identity.
fn expose_binding_root(catalog: &Catalog, ty: &ScalarType) -> Result<ScalarType> {
    let mut current = ty;
    for _ in 0..MAX_DEPTH {
        match current {
            ScalarType::Ref(id) => current = &catalog.definition(*id)?.ty,
            other => return Ok(other.clone()),
        }
    }
    Err(Error::new(
        "E_LIMIT",
        "migration binding type expansion exceeds the nesting limit",
    ))
}

fn preserve_structural_ids(old: &ScalarType, new: &mut ScalarType) {
    match (old, new) {
        (ScalarType::Record(old_fields), ScalarType::Record(new_fields)) => {
            for new_field in new_fields {
                if let Some(old_field) = old_fields
                    .iter()
                    .find(|old_field| old_field.name == new_field.name)
                {
                    new_field.id = old_field.id;
                    preserve_structural_ids(&old_field.ty, &mut new_field.ty);
                }
            }
        }
        (ScalarType::Enum(old_enum), ScalarType::Enum(new_enum)) => {
            for new_variant in &mut new_enum.variants {
                if let Some(old_variant) = old_enum
                    .variants
                    .iter()
                    .find(|old_variant| old_variant.name == new_variant.name)
                {
                    new_variant.id = old_variant.id;
                    for (old_arg, new_arg) in old_variant.args.iter().zip(&mut new_variant.args) {
                        preserve_structural_ids(old_arg, new_arg);
                    }
                }
            }
        }
        (ScalarType::Tuple(old_items), ScalarType::Tuple(new_items)) => {
            for (old_item, new_item) in old_items.iter().zip(new_items) {
                preserve_structural_ids(old_item, new_item);
            }
        }
        (ScalarType::Option(old_item), ScalarType::Option(new_item))
        | (ScalarType::List(old_item), ScalarType::List(new_item)) => {
            preserve_structural_ids(old_item, new_item)
        }
        _ => {}
    }
}

fn payload_type(args: &[ScalarType]) -> ScalarType {
    match args {
        [only] => only.clone(),
        many => ScalarType::Tuple(many.to_vec()),
    }
}

fn expanded_payload_type(catalog: &Catalog, args: &[ScalarType]) -> Result<ScalarType> {
    expose_binding_root(catalog, &payload_type(args))
}

fn payload_value(args: &[Value]) -> Value {
    match args {
        [only] => only.clone(),
        many => Value::Tuple(many.to_vec()),
    }
}

fn split_payload(value: Value, args: &[ScalarType]) -> Result<Vec<Value>> {
    match args {
        [_] => Ok(vec![value]),
        many => {
            let Value::Tuple(values) = value.unwrapped() else {
                return Err(Error::new(
                    "E_MIGRATION",
                    "variant conversion must return a tuple for positional payloads",
                ));
            };
            if values.len() != many.len() {
                return Err(Error::new(
                    "E_MIGRATION",
                    "variant conversion returned the wrong payload arity",
                ));
            }
            Ok(values.clone())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn migrate_value(
    old_catalog: &Catalog,
    new_catalog: &Catalog,
    old_ty: &ScalarType,
    new_ty: &ScalarType,
    value: &Value,
    path: &str,
    owner: Option<u64>,
    rewrite: Option<&ValueRewrite>,
    depth: usize,
) -> Result<Value> {
    if depth >= MAX_DEPTH {
        return Err(Error::new(
            "E_LIMIT",
            format!("{path}: migration value nesting exceeds the limit"),
        ));
    }
    if let (ScalarType::Ref(old_id), ScalarType::Ref(new_id)) = (old_ty, new_ty) {
        if old_id != new_id {
            return Err(Error::new(
                "E_MIGRATION",
                format!("{path}: named type identity changed during migration"),
            ));
        }
        let raw = match value {
            Value::Named { type_id, value } if type_id == old_id => value.as_ref(),
            Value::Named { .. } => {
                return Err(Error::new(
                    "E_MIGRATION",
                    format!("{path}: value has the wrong named type identity"),
                ));
            }
            value => value,
        };
        let migrated = migrate_value(
            old_catalog,
            new_catalog,
            &old_catalog.definition(*old_id)?.ty,
            &new_catalog.definition(*new_id)?.ty,
            raw,
            path,
            Some(*old_id),
            rewrite,
            depth + 1,
        )?;
        return Ok(Value::Named {
            type_id: *new_id,
            value: Box::new(migrated),
        });
    }

    match (old_ty, new_ty) {
        (ScalarType::Record(old_fields), ScalarType::Record(new_fields)) => {
            let Value::Record(old_values) = value.unwrapped() else {
                return Err(Error::new(
                    "E_MIGRATION",
                    format!("{path}: expected an old record value"),
                ));
            };
            let mut values = BTreeMap::new();
            for new_field in new_fields {
                let field_path = format!("{path}.{}", new_field.name);
                let migrated = if let Some(old_field) = old_fields
                    .iter()
                    .find(|old_field| old_field.id == new_field.id)
                {
                    let old_value = old_values.get(&old_field.name).ok_or_else(|| {
                        Error::new(
                            "E_MIGRATION",
                            format!("{path}: missing old field '{}'", old_field.name),
                        )
                    })?;
                    if let Some(ValueRewrite::Field {
                        owner_id,
                        field_id,
                        output,
                        transform,
                        ..
                    }) = rewrite
                        && owner == Some(*owner_id)
                        && old_field.id == *field_id
                    {
                        crate::matching::evaluate_migration_result(
                            new_catalog,
                            output,
                            &transform.value,
                            &transform.binding,
                            old_value,
                        )
                        .map_err(|error| {
                            Error::new(&error.code, format!("{field_path}: {}", error.message))
                        })?
                    } else {
                        migrate_value(
                            old_catalog,
                            new_catalog,
                            &old_field.ty,
                            &new_field.ty,
                            old_value,
                            &field_path,
                            owner,
                            rewrite,
                            depth + 1,
                        )?
                    }
                } else {
                    new_field.default.clone().ok_or_else(|| {
                        Error::new(
                            "E_MIGRATION",
                            format!("{field_path}: new field has no backfill default"),
                        )
                    })?
                };
                values.insert(new_field.name.clone(), migrated);
            }
            Ok(Value::Record(values))
        }
        (ScalarType::Enum(old_enum), ScalarType::Enum(new_enum)) => {
            let Value::Enum(old_value) = value.unwrapped() else {
                return Err(Error::new(
                    "E_MIGRATION",
                    format!("{path}: expected an old sum value"),
                ));
            };
            let old_variant = old_enum
                .variants
                .iter()
                .find(|variant| {
                    (old_value.id != 0 && variant.id == old_value.id)
                        || (old_value.id == 0 && variant.name == old_value.variant)
                })
                .ok_or_else(|| {
                    Error::new(
                        "E_MIGRATION",
                        format!("{path}: unknown old variant '{}'", old_value.variant),
                    )
                })?;
            let Some(new_variant) = new_enum
                .variants
                .iter()
                .find(|variant| variant.id == old_variant.id)
            else {
                if let Some(ValueRewrite::RemovedVariant {
                    owner_id,
                    variant_id,
                    output,
                    transform,
                    ..
                }) = rewrite
                    && owner == Some(*owner_id)
                    && old_variant.id == *variant_id
                {
                    let input = payload_value(&old_value.args);
                    let result = crate::matching::evaluate_migration_result(
                        new_catalog,
                        output,
                        &transform.value,
                        &transform.binding,
                        &input,
                    )
                    .map_err(|error| {
                        Error::new(&error.code, format!("{path}: {}", error.message))
                    })?;
                    return Ok(result.unwrapped().clone());
                }
                return Err(Error::new(
                    "E_MIGRATION",
                    format!(
                        "{path}: variant '{}' still has data; provide a complete using mapping",
                        old_variant.name
                    ),
                ));
            };
            let args = if let Some(ValueRewrite::Variant {
                owner_id,
                variant_id,
                output,
                transform,
                ..
            }) = rewrite
                && owner == Some(*owner_id)
                && old_variant.id == *variant_id
            {
                let input = payload_value(&old_value.args);
                let result = crate::matching::evaluate_migration_result(
                    new_catalog,
                    output,
                    &transform.value,
                    &transform.binding,
                    &input,
                )
                .map_err(|error| Error::new(&error.code, format!("{path}: {}", error.message)))?;
                split_payload(result, &new_variant.args)?
            } else {
                if old_variant.args.len() != new_variant.args.len()
                    || old_value.args.len() != old_variant.args.len()
                {
                    return Err(Error::new(
                        "E_MIGRATION",
                        format!("{path}: variant payload changed without a using mapping"),
                    ));
                }
                old_value
                    .args
                    .iter()
                    .zip(&old_variant.args)
                    .zip(&new_variant.args)
                    .enumerate()
                    .map(|(index, ((value, old_ty), new_ty))| {
                        migrate_value(
                            old_catalog,
                            new_catalog,
                            old_ty,
                            new_ty,
                            value,
                            &format!("{path}.{}[{index}]", new_variant.name),
                            owner,
                            rewrite,
                            depth + 1,
                        )
                    })
                    .collect::<Result<_>>()?
            };
            Ok(Value::Enum(EnumValue {
                variant: new_variant.name.clone(),
                args,
                id: new_variant.id,
            }))
        }
        (ScalarType::Tuple(old_items), ScalarType::Tuple(new_items)) => {
            let Value::Tuple(old_values) = value.unwrapped() else {
                return Err(Error::new(
                    "E_MIGRATION",
                    format!("{path}: expected an old tuple value"),
                ));
            };
            if old_items.len() != new_items.len() || old_values.len() != old_items.len() {
                return Err(Error::new(
                    "E_MIGRATION",
                    format!("{path}: tuple arity changed without a using mapping"),
                ));
            }
            Ok(Value::Tuple(
                old_values
                    .iter()
                    .zip(old_items)
                    .zip(new_items)
                    .enumerate()
                    .map(|(index, ((value, old_ty), new_ty))| {
                        migrate_value(
                            old_catalog,
                            new_catalog,
                            old_ty,
                            new_ty,
                            value,
                            &format!("{path}.{index}"),
                            owner,
                            rewrite,
                            depth + 1,
                        )
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        (ScalarType::List(old_item), ScalarType::List(new_item)) => {
            let Value::List(old_values) = value.unwrapped() else {
                return Err(Error::new(
                    "E_MIGRATION",
                    format!("{path}: expected an old list value"),
                ));
            };
            Ok(Value::List(
                old_values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        migrate_value(
                            old_catalog,
                            new_catalog,
                            old_item,
                            new_item,
                            value,
                            &format!("{path}[{index}]"),
                            owner,
                            rewrite,
                            depth + 1,
                        )
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        (ScalarType::Option(old_item), ScalarType::Option(new_item)) => match value.unwrapped() {
            Value::Option(None) => Ok(Value::Option(None)),
            Value::Option(Some(value)) => Ok(Value::Option(Some(Box::new(migrate_value(
                old_catalog,
                new_catalog,
                old_item,
                new_item,
                value,
                path,
                owner,
                rewrite,
                depth + 1,
            )?)))),
            _ => Err(Error::new(
                "E_MIGRATION",
                format!("{path}: expected an old option value"),
            )),
        },
        _ => new_catalog.coerce(value.unwrapped(), new_ty, path),
    }
}

fn field_path_name(catalog: &Catalog, fields: &[Column], ids: &[u64]) -> Option<String> {
    let mut columns = fields;
    let mut names = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let field = columns.iter().find(|field| field.id == *id)?;
        names.push(field.name.clone());
        if index + 1 < ids.len() {
            let ScalarType::Record(nested) = catalog.underlying(&field.ty).ok()? else {
                return None;
            };
            columns = nested;
        }
    }
    Some(names.join("."))
}
