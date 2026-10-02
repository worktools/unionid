//! Binding for typed references. Execution and durable publication are separate
//! from resolving a declaration; binding never allocates a catalog identity.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{Database, RowChange};
use crate::error::{Error, Result};
use crate::expression::same_type;
use crate::model::{RowId, ScalarType, Value};
use crate::query::{MAX_INDEX_COMPONENTS, ReferenceSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceMode {
    Exact,
    Optional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReferenceTarget {
    PrimaryKey,
    UniqueIndex { index_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceComponent {
    pub column: String,
    pub field_path: Vec<u64>,
    pub target_column: String,
    pub target_field_path: Vec<u64>,
    pub mode: ReferenceMode,
    pub target_type: ScalarType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceDefinition {
    pub id: u64,
    pub table_id: u64,
    pub target_table_id: u64,
    pub target: ReferenceTarget,
    pub components: Vec<ReferenceComponent>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ReferenceState {
    targets: imbl::OrdMap<Vec<u8>, imbl::OrdSet<RowId>>,
    sources: imbl::OrdMap<Vec<u8>, imbl::OrdSet<RowId>>,
}

impl ReferenceDefinition {
    fn values<'a>(
        &self,
        fields: &'a BTreeMap<String, Value>,
        source: bool,
    ) -> Result<Option<Vec<&'a Value>>> {
        let mut components = Vec::with_capacity(self.components.len());
        for component in &self.components {
            let column = if source {
                &component.column
            } else {
                &component.target_column
            };
            let value = super::row_field(fields, column)
                .ok_or_else(|| Error::new("E_FIELD", "reference field is missing"))?;
            let value = if source && component.mode == ReferenceMode::Optional {
                match value {
                    Value::Option(None) => return Ok(None),
                    Value::Option(Some(value)) => value.as_ref(),
                    _ => {
                        return Err(Error::new(
                            "E_TYPE",
                            "optional reference requires an Option value",
                        ));
                    }
                }
            } else {
                value
            };
            components.push(value);
        }
        Ok(Some(components))
    }

    pub(crate) fn source_value(&self, fields: &BTreeMap<String, Value>) -> Result<Option<Value>> {
        Ok(self.values(fields, true)?.map(|values| {
            if values.len() == 1 {
                values[0].clone()
            } else {
                Value::Tuple(values.into_iter().cloned().collect())
            }
        }))
    }

    fn key(
        &self,
        db: &Database,
        fields: &BTreeMap<String, Value>,
        source: bool,
    ) -> Result<Option<Vec<u8>>> {
        let Some(values) = self.values(fields, source)? else {
            return Ok(None);
        };
        let components = self
            .components
            .iter()
            .zip(values)
            .map(|(component, value)| crate::ordered_key::Component {
                ty: &component.target_type,
                value,
                descending: false,
            })
            .collect::<Vec<_>>();
        let key = crate::ordered_key::encode_tuple(&db.catalog, &components)?;
        if key.len().saturating_add(23) > crate::ordered_key::MAX_COMPLETE_INDEX_KEY_BYTES {
            return Err(Error::new(
                "E_INDEX_KEY_LIMIT",
                "reference key exceeds the complete index key limit",
            ));
        }
        Ok(Some(key))
    }
}

fn remove(postings: &mut imbl::OrdMap<Vec<u8>, imbl::OrdSet<RowId>>, key: &[u8], id: RowId) {
    if let Some(ids) = postings.get_mut(key) {
        ids.remove(&id);
        if ids.is_empty() {
            postings.remove(key);
        }
    }
}

impl Database {
    pub(crate) fn reference_target_index(
        &self,
        definition: &ReferenceDefinition,
    ) -> Result<(&crate::model::Table, &super::IndexDefinition)> {
        let target = self
            .schema_tables()
            .into_iter()
            .find(|table| table.id == definition.target_table_id)
            .ok_or_else(|| Error::new("E_STORAGE", "reference target table is missing"))?;
        let index = self
            .index_definitions
            .get(&target.name)
            .and_then(|indexes| {
                indexes.values().find(|index| match definition.target {
                    ReferenceTarget::PrimaryKey => {
                        index.is_primary_index(target.primary_key.as_deref())
                    }
                    ReferenceTarget::UniqueIndex { index_id } => index.id == index_id,
                })
            })
            .ok_or_else(|| Error::new("E_STORAGE", "reference target index is missing"))?;
        Ok((target, index))
    }

    pub(crate) fn reference_target_boundary(
        &self,
        definition: &ReferenceDefinition,
        fields: &BTreeMap<String, Value>,
    ) -> Result<Option<Vec<u8>>> {
        let Some(values) = definition.values(fields, true)? else {
            return Ok(None);
        };
        let (_, index) = self.reference_target_index(definition)?;
        let components = index.effective_components();
        let bound = definition
            .components
            .iter()
            .zip(&components)
            .zip(values)
            .map(
                |((component, index_component), value)| crate::ordered_key::Component {
                    ty: &component.target_type,
                    value,
                    descending: index_component.descending,
                },
            )
            .collect::<Vec<_>>();
        crate::ordered_key::encode_tuple(&self.catalog, &bound).map(Some)
    }

    pub(super) fn validate_source_reference_changes(
        &self,
        source: &dyn crate::row_source::TypedRowSource,
        table: &str,
        changes: &[RowChange],
        control: Option<&crate::control::ExecutionControl>,
    ) -> Result<()> {
        use std::ops::Bound;
        super::check_deadline(control)?;
        if changes.is_empty() || !self.has_references() {
            return Ok(());
        }
        let table_id = self.table(table)?.id;
        let changed_ids = changes
            .iter()
            .map(RowChange::row_id)
            .collect::<BTreeSet<_>>();
        let empty = BTreeSet::new();
        for definition in self.reference_definitions.values() {
            super::check_deadline(control)?;
            let source_changed = definition.table_id == table_id;
            let target_changed = definition.target_table_id == table_id;
            if !source_changed && !target_changed {
                continue;
            }
            let (target, index) = self.reference_target_index(definition)?;
            let mut source_keys = BTreeMap::new();
            let mut removed_targets = BTreeSet::new();
            let mut new_targets = BTreeSet::new();
            let mut bytes = 0_usize;
            let mut account = |key: &[u8]| -> Result<()> {
                bytes = bytes.saturating_add(key.len()).saturating_add(64);
                if bytes > crate::row_source::GENERAL_WORKING_MAX_BYTES {
                    return Err(Error::new(
                        "E_LIMIT",
                        "reference validation exceeds mutation working state limit; reduce the batch size",
                    ));
                }
                Ok(())
            };
            for (position, change) in changes.iter().enumerate() {
                super::check_deadline_periodically(control, position)?;
                if target_changed {
                    if let Some(row) = &change.before {
                        let key = definition
                            .key(self, &row.fields, false)?
                            .expect("exact target");
                        account(&key)?;
                        removed_targets.insert(key);
                    }
                    if let Some(row) = &change.after {
                        let key = definition
                            .key(self, &row.fields, false)?
                            .expect("exact target");
                        account(&key)?;
                        new_targets.insert(key);
                    }
                }
                if source_changed
                    && let Some(row) = &change.after
                    && let Some(key) = definition.key(self, &row.fields, true)?
                {
                    let boundary = self
                        .reference_target_boundary(definition, &row.fields)?
                        .expect("present source key");
                    account(&key)?;
                    account(&boundary)?;
                    source_keys.insert(key, boundary);
                }
            }
            for key in removed_targets {
                super::check_deadline(control)?;
                if new_targets.contains(&key) {
                    continue;
                }
                if source_keys.contains_key(&key)
                    || source.reference_source_exists(
                        definition.id,
                        &key,
                        if source_changed { &changed_ids } else { &empty },
                        control,
                    )?
                {
                    return Err(Error::new("E_CONSTRAINT", "target key is still referenced")
                        .constraint(crate::error::ConstraintKind::ReferenceRestricted));
                }
            }
            for (key, boundary) in source_keys {
                super::check_deadline(control)?;
                if new_targets.contains(&key) {
                    continue;
                }
                let bounds = (Bound::Included(boundary.clone()), Bound::Included(boundary));
                let mut cursor =
                    source.scan_index(&target.name, &index.shape_key(), &bounds, false, None)?;
                let mut found = false;
                while let Some(hits) = cursor.next_batch(control)? {
                    if hits
                        .iter()
                        .any(|hit| !target_changed || !changed_ids.contains(&hit.row_id))
                    {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Err(missing_target());
                }
            }
        }
        Ok(())
    }

    pub(super) fn memory_reference_source_exists(
        &self,
        reference_id: u64,
        key: &[u8],
        excluded: &BTreeSet<RowId>,
        control: Option<&crate::control::ExecutionControl>,
    ) -> Result<bool> {
        super::check_deadline(control)?;
        let state = self
            .reference_states
            .get(&reference_id)
            .ok_or_else(|| Error::new("E_STORAGE", "reference postings are missing"))?;
        if let Some(rows) = state.sources.get(key) {
            for (position, id) in rows.iter().enumerate() {
                super::check_deadline_periodically(control, position)?;
                if !excluded.contains(id) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(crate) fn reference_component_count(&self, reference_id: u64) -> Result<u8> {
        let definition = self
            .reference_definitions
            .get(&reference_id)
            .ok_or_else(|| Error::new("E_STORAGE", "reference ID is not in the catalog"))?;
        u8::try_from(definition.components.len())
            .map_err(|_| Error::new("E_STORAGE", "reference has too many components"))
    }

    pub(super) fn record_reference_changes(
        &mut self,
        table: &str,
        changes: &[RowChange],
    ) -> Result<()> {
        let table_id = self.table(table)?.id;
        let definitions = self
            .reference_definitions
            .values()
            .filter(|definition| definition.table_id == table_id)
            .cloned()
            .collect::<Vec<_>>();
        for definition in definitions {
            for change in changes {
                let before = change
                    .before
                    .as_ref()
                    .map(|row| definition.source_value(&row.fields))
                    .transpose()?
                    .flatten();
                let after = change
                    .after
                    .as_ref()
                    .map(|row| definition.source_value(&row.fields))
                    .transpose()?
                    .flatten();
                if before.as_ref().map(Value::index_key) == after.as_ref().map(Value::index_key) {
                    continue;
                }
                if let Some(value) = before {
                    self.record_index_entry(definition.id, &value, change.row_id(), true, false);
                }
                if let Some(value) = after {
                    self.record_index_entry(definition.id, &value, change.row_id(), false, true);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn schema_references(&self) -> Vec<(&ReferenceDefinition, ReferenceSpec)> {
        let tables = self.schema_tables();
        self.reference_definitions
            .values()
            .map(|reference| {
                let source = tables
                    .iter()
                    .find(|table| table.id == reference.table_id)
                    .expect("bound reference source exists");
                let target = tables
                    .iter()
                    .find(|table| table.id == reference.target_table_id)
                    .expect("bound reference target exists");
                (
                    reference,
                    ReferenceSpec {
                        table: source.name.clone(),
                        fields: reference
                            .components
                            .iter()
                            .map(|part| part.column.clone())
                            .collect(),
                        target_table: target.name.clone(),
                        target_fields: reference
                            .components
                            .iter()
                            .map(|part| part.target_column.clone())
                            .collect(),
                    },
                )
            })
            .collect()
    }

    pub(crate) fn has_references(&self) -> bool {
        !self.reference_definitions.is_empty()
    }

    pub(super) fn create_reference(
        &mut self,
        spec: &ReferenceSpec,
    ) -> Result<super::QueryResponse> {
        let mut definition = self.bind_reference(spec)?;
        if self
            .reference_definitions
            .values()
            .any(|existing| same_shape(existing, &definition))
        {
            return Err(Error::new("E_SCHEMA", "reference already exists"));
        }
        let state = self.build_reference_state(&definition, &spec.table, &spec.target_table)?;
        definition.id = self.catalog.allocate()?;
        self.reference_states.insert(definition.id, state);
        self.reference_definitions.insert(definition.id, definition);
        Ok(super::QueryResponse::ok_message("reference created"))
    }

    fn build_reference_state(
        &self,
        definition: &ReferenceDefinition,
        source: &str,
        target: &str,
    ) -> Result<ReferenceState> {
        let mut state = ReferenceState::default();
        for row in &self.table(target)?.rows {
            let key = definition
                .key(self, &row.fields, false)?
                .expect("target keys are exact");
            state.targets.entry(key).or_default().insert(row.id);
        }
        for row in &self.table(source)?.rows {
            if let Some(key) = definition.key(self, &row.fields, true)? {
                if !state.targets.contains_key(&key) {
                    return Err(missing_target());
                }
                state.sources.entry(key).or_default().insert(row.id);
            }
        }
        Ok(state)
    }

    pub(super) fn add_migration_reference(&mut self, spec: &ReferenceSpec) -> Result<()> {
        let mut definition = self.bind_reference(spec)?;
        if self
            .reference_definitions
            .values()
            .any(|existing| same_shape(existing, &definition))
        {
            return Err(Error::new("E_SCHEMA", "reference already exists"));
        }
        definition.id = self.catalog.allocate()?;
        self.reference_definitions.insert(definition.id, definition);
        // Rows are checked against the final complete migration candidate.
        Ok(())
    }

    pub(super) fn rebuild_references(&mut self) -> Result<()> {
        let definitions = self.bound_reference_definitions()?;
        let mut states = BTreeMap::new();
        let tables = self.schema_tables();
        for definition in definitions.values() {
            let source = tables
                .iter()
                .find(|table| table.id == definition.table_id)
                .expect("reference binding validated source table");
            let target = tables
                .iter()
                .find(|table| table.id == definition.target_table_id)
                .expect("reference binding validated target table");
            states.insert(
                definition.id,
                self.build_reference_state(definition, &source.name, &target.name)?,
            );
        }
        self.reference_definitions = definitions;
        self.reference_states = states;
        Ok(())
    }

    /// Only unpublished migration batches may defer row existence validation.
    /// The complete generation must pass durable integrity checks before Ready.
    pub(super) fn rebind_migration_batch_references(&mut self) -> Result<()> {
        self.reference_definitions = self.bound_reference_definitions()?;
        self.reference_states.clear();
        Ok(())
    }

    fn bound_reference_definitions(&self) -> Result<BTreeMap<u64, ReferenceDefinition>> {
        let mut definitions = BTreeMap::new();
        for original in self.reference_definitions.values() {
            let tables = self.schema_tables();
            let source = tables
                .iter()
                .find(|table| table.id == original.table_id)
                .ok_or_else(|| Error::new("E_MIGRATION", "reference source table was removed"))?;
            let target = tables
                .iter()
                .find(|table| table.id == original.target_table_id)
                .ok_or_else(|| Error::new("E_MIGRATION", "reference target table was removed"))?;
            let fields = original
                .components
                .iter()
                .map(|component| {
                    super::migration::field_path_name(
                        &self.catalog,
                        &source.schema,
                        &component.field_path,
                    )
                    .ok_or_else(|| Error::new("E_MIGRATION", "reference source path was removed"))
                })
                .collect::<Result<Vec<_>>>()?;
            let target_fields = original
                .components
                .iter()
                .map(|component| {
                    super::migration::field_path_name(
                        &self.catalog,
                        &target.schema,
                        &component.target_field_path,
                    )
                    .ok_or_else(|| Error::new("E_MIGRATION", "reference target path was removed"))
                })
                .collect::<Result<Vec<_>>>()?;
            let spec = ReferenceSpec {
                table: source.name.clone(),
                fields,
                target_table: target.name.clone(),
                target_fields,
            };
            let mut bound = self.bind_reference(&spec)?;
            let pinned_valid = match original.target {
                ReferenceTarget::PrimaryKey => bound.target == ReferenceTarget::PrimaryKey,
                ReferenceTarget::UniqueIndex { index_id } => self
                    .index_definitions
                    .get(&target.name)
                    .into_iter()
                    .flat_map(|indexes| indexes.values())
                    .any(|index| {
                        let fields = index.effective_components();
                        index.id == index_id
                            && index.kind.is_unique()
                            && index.predicate.is_none()
                            && fields.len() == bound.components.len()
                            && fields
                                .iter()
                                .zip(&bound.components)
                                .all(|(field, component)| {
                                    field.field_path == component.target_field_path
                                })
                    }),
            };
            if !pinned_valid
                || original
                    .components
                    .iter()
                    .zip(&bound.components)
                    .any(|(old, new)| {
                        old.mode != new.mode || !same_type(&old.target_type, &new.target_type)
                    })
            {
                return Err(Error::new(
                    "E_MIGRATION",
                    "reference binding changed; drop and add the reference explicitly",
                ));
            }
            bound.id = original.id;
            bound.target = original.target.clone();
            if definitions
                .values()
                .any(|existing| same_shape(existing, &bound))
            {
                return Err(Error::new("E_SCHEMA", "duplicate reference shape"));
            }
            definitions.insert(bound.id, bound);
        }
        Ok(definitions)
    }

    pub(super) fn ensure_field_not_referenced(&self, field_id: u64) -> Result<()> {
        if self.reference_definitions.values().any(|reference| {
            reference.components.iter().any(|component| {
                component.field_path.contains(&field_id)
                    || component.target_field_path.contains(&field_id)
            })
        }) {
            return Err(Error::new(
                "E_MIGRATION",
                "field is used by a reference; drop the reference first",
            ));
        }
        Ok(())
    }

    pub(super) fn drop_reference(&mut self, spec: &ReferenceSpec) -> Result<super::QueryResponse> {
        let bound = self.bind_reference(spec)?;
        let id = self
            .reference_definitions
            .iter()
            .find(|(_, existing)| same_shape(existing, &bound))
            .map(|(id, _)| *id)
            .ok_or_else(|| Error::new("E_SCHEMA", "reference does not exist"))?;
        self.reference_definitions.remove(&id);
        self.reference_states.remove(&id);
        Ok(super::QueryResponse::ok_message("reference dropped"))
    }

    pub(super) fn reference_candidate(
        &self,
        table: &str,
        changes: &[RowChange],
    ) -> Result<BTreeMap<u64, ReferenceState>> {
        let mut states = self.reference_states.clone();
        if changes.is_empty() || !self.has_references() {
            return Ok(states);
        }
        let table_id = self.table(table)?.id;
        for definition in self.reference_definitions.values() {
            let source_changed = definition.table_id == table_id;
            let target_changed = definition.target_table_id == table_id;
            if !source_changed && !target_changed {
                continue;
            }
            let state = states
                .get_mut(&definition.id)
                .ok_or_else(|| Error::new("E_STORAGE", "reference postings are missing"))?;
            let mut source_keys = BTreeSet::new();
            let mut removed_targets = BTreeSet::new();
            // Apply every before/after image before checking. This is essential
            // for batch self-references and simultaneous key changes.
            for change in changes {
                if let Some(row) = &change.before {
                    if source_changed && let Some(key) = definition.key(self, &row.fields, true)? {
                        remove(&mut state.sources, &key, row.id);
                    }
                    if target_changed {
                        let key = definition
                            .key(self, &row.fields, false)?
                            .expect("target key");
                        remove(&mut state.targets, &key, row.id);
                        removed_targets.insert(key);
                    }
                }
            }
            for change in changes {
                if let Some(row) = &change.after {
                    if source_changed && let Some(key) = definition.key(self, &row.fields, true)? {
                        state.sources.entry(key.clone()).or_default().insert(row.id);
                        source_keys.insert(key);
                    }
                    if target_changed {
                        let key = definition
                            .key(self, &row.fields, false)?
                            .expect("target key");
                        state.targets.entry(key).or_default().insert(row.id);
                    }
                }
            }
            for key in removed_targets {
                if !state.targets.contains_key(&key) && state.sources.contains_key(&key) {
                    return Err(Error::new("E_CONSTRAINT", "target key is still referenced")
                        .constraint(crate::error::ConstraintKind::ReferenceRestricted));
                }
            }
            for key in source_keys {
                if !state.targets.contains_key(&key) {
                    return Err(missing_target());
                }
            }
        }
        Ok(states)
    }

    pub(crate) fn bind_reference(&self, spec: &ReferenceSpec) -> Result<ReferenceDefinition> {
        // Also validate manually constructed Rust ASTs, not only parsed input.
        if spec.fields.is_empty()
            || spec.fields.len() > MAX_INDEX_COMPONENTS
            || spec.fields.len() != spec.target_fields.len()
        {
            return Err(Error::new(
                "E_SCHEMA",
                "a reference requires matching source and target lists of 1 to 16 fields",
            ));
        }
        for fields in [&spec.fields, &spec.target_fields] {
            let mut seen = BTreeSet::new();
            if fields.iter().any(|field| !seen.insert(field)) {
                return Err(Error::new("E_SCHEMA", "duplicate reference field"));
            }
        }
        let source = self.table(&spec.table)?;
        let target = self.table(&spec.target_table)?;
        let mut components = Vec::with_capacity(spec.fields.len());
        for (column, target_column) in spec.fields.iter().zip(&spec.target_fields) {
            let source_type = self.catalog.field_type(&source.schema, column)?;
            let target_type = self.catalog.field_type(&target.schema, target_column)?;
            let mode = if same_type(source_type, target_type) {
                ReferenceMode::Exact
            } else if matches!(source_type, ScalarType::Option(inner) if same_type(inner, target_type))
            {
                ReferenceMode::Optional
            } else {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "reference field '{}.{column}' must match '{}.{target_column}' exactly or wrap its type in one Option",
                        spec.table, spec.target_table,
                    ),
                ));
            };
            components.push(ReferenceComponent {
                column: column.clone(),
                field_path: self.catalog.field_path_ids(&source.schema, column)?,
                target_column: target_column.clone(),
                target_field_path: self.catalog.field_path_ids(&target.schema, target_column)?,
                mode,
                target_type: target_type.clone(),
            });
        }
        let key = if components.len() == 1
            && target.primary_key.as_ref().is_some_and(|key| {
                self.catalog
                    .field_path_ids(&target.schema, key)
                    .is_ok_and(|path| path == components[0].target_field_path)
            }) {
            ReferenceTarget::PrimaryKey
        } else {
            let index_id = self.index_definitions.get(&spec.target_table)
                .into_iter().flat_map(|indexes| indexes.values())
                .filter(|index| index.kind.is_unique() && index.predicate.is_none())
                .filter(|index| {
                    let target_components = index.effective_components();
                    target_components.len() == components.len()
                        && target_components.iter().zip(&components)
                            .all(|(target, source)| target.field_path == source.target_field_path)
                })
                .map(|index| index.id).min()
                .ok_or_else(|| Error::new(
                    "E_SCHEMA",
                    "reference target requires a primary key or unconditional unique index over the same ordered fields",
                ))?;
            ReferenceTarget::UniqueIndex { index_id }
        };
        Ok(ReferenceDefinition {
            id: 0,
            table_id: source.id,
            target_table_id: target.id,
            target: key,
            components,
        })
    }
}

fn same_shape(left: &ReferenceDefinition, right: &ReferenceDefinition) -> bool {
    left.table_id == right.table_id
        && left.target_table_id == right.target_table_id
        && left.components.len() == right.components.len()
        && left
            .components
            .iter()
            .zip(&right.components)
            .all(|(left, right)| {
                left.field_path == right.field_path
                    && left.target_field_path == right.target_field_path
            })
}

fn missing_target() -> Error {
    Error::new("E_CONSTRAINT", "reference target does not exist")
        .constraint(crate::error::ConstraintKind::ReferenceMissing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(source: &str) -> Database {
        let mut database = Database::default();
        for located in crate::syntax::parse(source).unwrap() {
            database.execute(located.statement).unwrap();
        }
        database
    }

    fn spec(source: &str) -> ReferenceSpec {
        let crate::query::Statement::CreateReference(spec) =
            crate::syntax::parse(source).unwrap().remove(0).statement
        else {
            panic!("expected reference")
        };
        spec
    }

    fn execute(db: &mut Database, source: &str) {
        for located in crate::syntax::parse(source).unwrap() {
            db.execute(located.statement).unwrap();
        }
    }

    fn bounded(source: &Database, query: &str) -> Result<super::super::QueryResponse> {
        let mut candidate = source.metadata_only()?;
        assert!(candidate.reference_states.is_empty());
        let statement = crate::syntax::parse(query).unwrap().remove(0).statement;
        let result = candidate.execute_bounded_mutation_from(source, statement, None);
        if result.is_err() {
            assert!(candidate.take_write_set().rows.is_empty());
        }
        result
    }

    #[test]
    fn bounded_mutations_enforce_references_without_resident_postings() {
        let db = database(
            "struct User {id: int}\nstruct Task {id: int, assignee: Option<int>}\ntable users: User {key id}\ntable tasks: Task {key id}\ncreate reference tasks (assignee) references users (id)\ninsert many users [{id: 7}, {id: 8}]\ninsert tasks {id: 1, assignee: Some(7)}",
        );
        for query in [
            "insert tasks {id: 2, assignee: Some(9)}",
            "upsert tasks {id: 1, assignee: Some(9)}",
            "update tasks | set assignee = Some(9)",
        ] {
            assert_eq!(
                bounded(&db, query).unwrap_err().constraint,
                Some(crate::error::ConstraintKind::ReferenceMissing),
                "{query}"
            );
        }
        for query in [
            "delete users | filter id == 7",
            "update users | filter id == 7 | set id = 9",
        ] {
            assert_eq!(
                bounded(&db, query).unwrap_err().constraint,
                Some(crate::error::ConstraintKind::ReferenceRestricted),
                "{query}"
            );
        }
        for query in [
            "insert tasks {id: 2, assignee: Some(8)}",
            "insert tasks {id: 2, assignee: None}",
            "upsert tasks {id: 1, assignee: Some(8)}",
            "update tasks | set assignee = None",
            "delete tasks",
            "delete users | filter id == 8",
        ] {
            bounded(&db, query).unwrap_or_else(|error| panic!("{query}: {error}"));
        }
    }

    #[test]
    fn bounded_self_reference_uses_the_complete_batch_candidate() {
        let mut db = database(
            "struct Node {id: int, parent: int}\ntable nodes: Node {key id}\ncreate reference nodes (parent) references nodes (id)",
        );
        let cycle = "insert many nodes [{id: 1, parent: 2}, {id: 2, parent: 1}]";
        bounded(&db, cycle).unwrap();
        execute(&mut db, cycle);
        bounded(&db, "delete nodes").unwrap();
        assert_eq!(
            bounded(&db, "delete nodes | filter id == 1")
                .unwrap_err()
                .constraint,
            Some(crate::error::ConstraintKind::ReferenceRestricted)
        );
        bounded(
            &db,
            "update nodes | set {id = id + 10, parent = parent + 10}",
        )
        .unwrap();
    }

    #[test]
    fn bounded_target_lookup_respects_descending_composite_unique_keys() {
        let db = database(
            "struct Parent {tenant: int, alias: Option<int>}\nstruct Child {tenant: int, alias: Option<int>}\ntable parents: Parent {}\ntable children: Child {}\ncreate unique index parents (-tenant, alias)\ncreate reference children (tenant, alias) references parents (tenant, alias)\ninsert parents {tenant: 1, alias: None}",
        );
        bounded(&db, "insert children {tenant: 1, alias: None}").unwrap();
        assert_eq!(
            bounded(&db, "insert children {tenant: 2, alias: None}")
                .unwrap_err()
                .constraint,
            Some(crate::error::ConstraintKind::ReferenceMissing)
        );
    }

    #[test]
    fn reverse_postings_project_optional_values_and_coalesce_changes() {
        let mut db = database(
            "struct User {id: int}\nstruct Task {id: int, assignee: Option<int>}\ntable users: User {key id}\ntable tasks: Task {key id}\ncreate reference tasks (assignee) references users (id)\ninsert many users [{id: 7}, {id: 8}]\ninsert many tasks [{id: 1, assignee: Some(7)}, {id: 2, assignee: None}]",
        );
        let reference = db.reference_definitions.values().next().unwrap().clone();
        let postings = db
            .durable_secondary_indexes()
            .unwrap()
            .into_iter()
            .filter(|(id, _, _)| *id == reference.id)
            .collect::<Vec<_>>();
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].1.index_key(), Value::Int(7).index_key());
        let encoded = db
            .encode_secondary_index_key_v4(reference.id, &postings[0].1, postings[0].2)
            .unwrap();
        crate::ordered_key::validate_complete_v4(&encoded).unwrap();
        assert!(
            db.encode_secondary_index_key_v3(reference.id, &postings[0].1, postings[0].2)
                .is_err()
        );

        db.take_write_set();
        execute(
            &mut db,
            "update tasks | filter id == 1 | set assignee = Some(8)\nupdate tasks | filter id == 1 | set assignee = Some(7)\ninsert tasks {id: 3, assignee: Some(8)}\ndelete tasks | filter id == 3",
        );
        assert!(
            db.take_write_set()
                .index_entries
                .values()
                .all(|entry| entry.index_id != reference.id)
        );
        execute(
            &mut db,
            "update tasks | filter id == 1 | set assignee = None",
        );
        let writes = db.take_write_set();
        let entries = writes
            .index_entries
            .values()
            .filter(|entry| entry.index_id == reference.id)
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].before && !entries[0].after);
        assert_eq!(entries[0].value.index_key(), Value::Int(7).index_key());
    }

    #[test]
    fn reverse_postings_keep_exact_none_and_composite_component_boundaries() {
        let db = database(
            "struct Parent {tenant: int, alias: Option<int>}\nstruct Child {tenant: int, alias: Option<int>}\ntable parents: Parent {}\ntable children: Child {}\ncreate unique index parents (-tenant, alias)\ncreate reference children (tenant, alias) references parents (tenant, alias)\ninsert parents {tenant: 1, alias: None}\ninsert children {tenant: 1, alias: None}",
        );
        let reference = db.reference_definitions.values().next().unwrap();
        let postings = db
            .durable_secondary_indexes()
            .unwrap()
            .into_iter()
            .filter(|(id, _, _)| *id == reference.id)
            .collect::<Vec<_>>();
        assert_eq!(postings.len(), 1);
        assert_eq!(
            postings[0].1.index_key(),
            Value::Tuple(vec![Value::Int(1), Value::Option(None)]).index_key()
        );
        let encoded = db
            .encode_secondary_index_key_v4(reference.id, &postings[0].1, postings[0].2)
            .unwrap();
        let row = &db.table("children").unwrap().rows[0];
        let projected = reference.key(&db, &row.fields, true).unwrap().unwrap();
        assert_eq!(&encoded[15..encoded.len() - 8], projected.as_slice());
        assert!(
            db.encode_secondary_index_key_v4(reference.id, &Value::Int(1), 0)
                .is_err()
        );
        assert!(
            db.encode_secondary_index_key_v4(reference.id, &Value::Tuple(vec![Value::Int(1)]), 0)
                .is_err()
        );
    }

    #[test]
    fn optional_binding_preserves_nominal_identity_and_nested_options() {
        let db = database(
            "enum UserId { Id(int) }\nenum OtherId { Id(int) }\nstruct User { id: UserId, alias: Option<UserId> }\nstruct Task { exact: UserId, assignee: Option<UserId>, nested: Option<Option<UserId>>, other: OtherId }\ntable users: User {}\ntable tasks: Task {}\ncreate unique index users (id)\ncreate unique index users (alias)",
        );
        for (field, target, mode) in [
            ("exact", "id", ReferenceMode::Exact),
            ("assignee", "id", ReferenceMode::Optional),
            ("assignee", "alias", ReferenceMode::Exact),
            ("nested", "alias", ReferenceMode::Optional),
        ] {
            let bound = db
                .bind_reference(&spec(&format!(
                    "create reference tasks ({field}) references users ({target})"
                )))
                .unwrap();
            assert_eq!(bound.components[0].mode, mode);
            assert!(!bound.components[0].field_path.is_empty());
        }
        for field in ["other", "nested"] {
            let error = db
                .bind_reference(&spec(&format!(
                    "create reference tasks ({field}) references users (id)"
                )))
                .unwrap_err();
            assert_eq!(error.code, "E_TYPE");
        }
    }

    #[test]
    fn composite_binding_is_ordered_and_selects_lowest_unique_id() {
        let db = database(
            "struct Key { tenant: int, sku: text }\nstruct Source { key: Key }\ntable inventory: Key {}\ntable sources: Source {}\ncreate unique index inventory (-tenant, sku)\ncreate unique index inventory (tenant, sku)",
        );
        let before = db.schema_info();
        let bound = db
            .bind_reference(&spec(
                "create reference sources (key.tenant, key.sku) references inventory (tenant, sku)",
            ))
            .unwrap();
        let minimum = db.index_definitions["inventory"]
            .values()
            .map(|index| index.id)
            .min()
            .unwrap();
        assert_eq!(
            bound.target,
            ReferenceTarget::UniqueIndex { index_id: minimum }
        );
        assert_eq!(bound.components[0].field_path.len(), 2);
        assert_eq!(bound.id, 0);
        assert_eq!(db.schema_info(), before);
        assert!(
            db.bind_reference(&spec(
                "create reference sources (key.sku, key.tenant) references inventory (sku, tenant)"
            ))
            .is_err()
        );
    }

    #[test]
    fn primary_key_precedes_unique_and_partial_is_not_a_target() {
        let db = database(
            "struct User { id: int, active: bool, external: text }\nstruct Task { assignee: Option<int>, external: text }\ntable users: User { key id }\ntable tasks: Task {}\ncreate unique index users (-id)\ncreate unique index users (external) if active == true",
        );
        let bound = db
            .bind_reference(&spec(
                "create reference tasks (assignee) references users (id)",
            ))
            .unwrap();
        assert_eq!(bound.target, ReferenceTarget::PrimaryKey);
        assert_eq!(
            db.bind_reference(&spec(
                "create reference tasks (external) references users (external)"
            ))
            .unwrap_err()
            .code,
            "E_SCHEMA"
        );
    }

    #[test]
    fn binding_rejects_decimal_coercion_and_ast_shape_bypasses() {
        let db = database(
            "struct A { amount: decimal 10 2 }\nstruct B { amount: decimal 12 2 }\ntable sources: A {}\ntable targets: B {}\ncreate unique index targets (amount)",
        );
        let mut declaration = spec("create reference sources (amount) references targets (amount)");
        assert_eq!(db.bind_reference(&declaration).unwrap_err().code, "E_TYPE");
        declaration.fields.clear();
        assert_eq!(
            db.bind_reference(&declaration).unwrap_err().code,
            "E_SCHEMA"
        );
        declaration.fields = vec!["amount".into(), "amount".into()];
        declaration.target_fields = declaration.fields.clone();
        assert_eq!(
            db.bind_reference(&declaration).unwrap_err().code,
            "E_SCHEMA"
        );
    }
}
