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
    fn key(
        &self,
        db: &Database,
        fields: &BTreeMap<String, Value>,
        source: bool,
    ) -> Result<Option<Vec<u8>>> {
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
            components.push(crate::ordered_key::Component {
                ty: &component.target_type,
                value,
                descending: false,
            });
        }
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
        let mut definitions = BTreeMap::new();
        let mut states = BTreeMap::new();
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
            let state = self.build_reference_state(&bound, &source.name, &target.name)?;
            definitions.insert(bound.id, bound);
            states.insert(original.id, state);
        }
        self.reference_definitions = definitions;
        self.reference_states = states;
        Ok(())
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
