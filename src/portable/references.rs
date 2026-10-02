//! Portable reference metadata and validation share the catalog's nominal rules.
use super::*;

pub(super) fn describe(database: &Database) -> BTreeMap<u64, Vec<ReferenceDescription>> {
    let mut result = BTreeMap::<_, Vec<_>>::new();
    for (definition, spec) in database.schema_references() {
        result
            .entry(definition.table_id)
            .or_default()
            .push(ReferenceDescription {
                id: definition.id.to_string(),
                target_table_id: definition.target_table_id.to_string(),
                target_table: spec.target_table,
                target: match definition.target {
                    crate::db::ReferenceTarget::PrimaryKey => ReferenceKeyDescription::PrimaryKey,
                    crate::db::ReferenceTarget::UniqueIndex { index_id } => {
                        ReferenceKeyDescription::UniqueIndex {
                            index_id: index_id.to_string(),
                        }
                    }
                },
                components: definition
                    .components
                    .iter()
                    .map(|part| ReferenceComponentDescription {
                        source: KeyDescription {
                            field: part.column.clone(),
                            field_path: part.field_path.iter().map(u64::to_string).collect(),
                        },
                        target: KeyDescription {
                            field: part.target_column.clone(),
                            field_path: part.target_field_path.iter().map(u64::to_string).collect(),
                        },
                        mode: match part.mode {
                            crate::db::ReferenceMode::Exact => ReferenceMatchMode::Exact,
                            crate::db::ReferenceMode::Optional => ReferenceMatchMode::Optional,
                        },
                    })
                    .collect(),
                on_delete: ReferenceAction::Restrict,
                on_update: ReferenceAction::Restrict,
            });
    }
    result
}

pub(super) fn validate(
    description: &SchemaDescription,
    types: &BTreeMap<u64, (&str, &TypeShape)>,
    ids: &mut BTreeSet<u64>,
) -> Result<()> {
    let tables = description
        .tables
        .iter()
        .map(|table| (table.id.as_str(), table))
        .collect::<BTreeMap<_, _>>();
    for source in &description.tables {
        let mut shapes = BTreeSet::new();
        for reference in &source.references {
            let owner = format!("table {} reference {}", source.name, reference.id);
            let invalid =
                |reason: &str| Error::new("E_CONTRACT_SCHEMA", format!("{owner}: {reason}"));
            if description.version < 3 {
                return Err(invalid("references require description version 3"));
            }
            insert_contract_id(ids, &reference.id, &owner)?;
            parse_contract_id(&reference.target_table_id, &owner, false)?;
            let target = tables
                .get(reference.target_table_id.as_str())
                .ok_or_else(|| invalid("unknown target table ID"))?;
            if target.name != reference.target_table {
                return Err(invalid("target table name does not match its ID"));
            }
            if reference.components.is_empty()
                || reference.components.len() > crate::query::MAX_INDEX_COMPONENTS
            {
                return Err(invalid("reference requires 1..16 components"));
            }
            let mut source_paths = BTreeSet::new();
            let mut target_paths = BTreeSet::new();
            for component in &reference.components {
                if !source_paths.insert(&component.source.field_path)
                    || !target_paths.insert(&component.target.field_path)
                {
                    return Err(invalid("duplicate reference field path"));
                }
                let (source_name, source_type) =
                    resolve_field_path(&source.row, &component.source.field_path, types, &owner)?;
                let (target_name, target_type) =
                    resolve_field_path(&target.row, &component.target.field_path, types, &owner)?;
                if source_name != component.source.field || target_name != component.target.field {
                    return Err(invalid("field name does not match its ID path"));
                }
                // Exact matching takes precedence, including Option<T> -> Option<T>.
                let mode = if same_type(source_type, target_type) {
                    ReferenceMatchMode::Exact
                } else if matches!(source_type, TypeShape::Option { item } if same_type(item, target_type))
                {
                    ReferenceMatchMode::Optional
                } else {
                    return Err(invalid("source and target types do not match nominally"));
                };
                if component.mode != mode {
                    return Err(invalid(
                        "reference mode does not match the source/target types",
                    ));
                }
            }
            let paths = reference
                .components
                .iter()
                .map(|component| &component.target.field_path)
                .collect::<Vec<_>>();
            let matches_key = match &reference.target {
                ReferenceKeyDescription::PrimaryKey => target
                    .primary_key
                    .as_ref()
                    .is_some_and(|key| paths == [&key.field_path]),
                ReferenceKeyDescription::UniqueIndex { index_id } => {
                    parse_contract_id(index_id, &owner, false)?;
                    target.indexes.iter().any(|index| {
                        index.id == *index_id
                            && index.unique
                            && index.predicate.is_none()
                            && index
                                .components
                                .iter()
                                .map(|part| &part.field_path)
                                .collect::<Vec<_>>()
                                == paths
                    })
                }
            };
            if !matches_key {
                return Err(invalid(
                    "target must be the pinned primary key or unconditional unique index in field order",
                ));
            }
            let shape = (
                reference.target_table_id.as_str(),
                reference
                    .components
                    .iter()
                    .map(|part| (&part.source.field_path, &part.target.field_path))
                    .collect::<Vec<_>>(),
            );
            if !shapes.insert(shape) {
                return Err(invalid("duplicate reference declaration"));
            }
        }
    }
    Ok(())
}

// Structural records and sums compare names/shapes, not nested catalog field
// IDs or defaults. Named types remain nominal even when their bodies agree.
fn same_type(left: &TypeShape, right: &TypeShape) -> bool {
    match (left, right) {
        (TypeShape::Ref { type_id: left, .. }, TypeShape::Ref { type_id: right, .. }) => {
            left == right
        }
        (TypeShape::Record { fields: left }, TypeShape::Record { fields: right }) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    left.name == right.name && same_type(&left.shape, &right.shape)
                })
        }
        (TypeShape::Sum { variants: left }, TypeShape::Sum { variants: right }) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    left.name == right.name && same_items(&left.payload, &right.payload)
                })
        }
        (TypeShape::Tuple { items: left }, TypeShape::Tuple { items: right }) => {
            same_items(left, right)
        }
        (TypeShape::Option { item: left }, TypeShape::Option { item: right })
        | (TypeShape::List { item: left, .. }, TypeShape::List { item: right, .. }) => {
            same_type(left, right)
        }
        (
            TypeShape::Map {
                key: lk, value: lv, ..
            },
            TypeShape::Map {
                key: rk, value: rv, ..
            },
        ) => same_type(lk, rk) && same_type(lv, rv),
        _ => left == right,
    }
}

fn same_items(left: &[TypeShape], right: &[TypeShape]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| same_type(left, right))
}

pub(super) fn compare(
    baseline: &[ReferenceDescription],
    candidate: &[ReferenceDescription],
    path: &str,
    report: &mut EvolutionReport,
) {
    for reference in candidate {
        let previous = baseline.iter().find(|previous| previous.id == reference.id);
        // Names can change without changing the relationship; compare stable paths.
        let unchanged = previous.is_some_and(|previous| {
            previous.target_table_id == reference.target_table_id
                && previous.target == reference.target
                && previous.components.len() == reference.components.len()
                && previous
                    .components
                    .iter()
                    .zip(&reference.components)
                    .all(|(left, right)| {
                        left.source.field_path == right.source.field_path
                            && left.target.field_path == right.target.field_path
                            && left.mode == right.mode
                    })
        });
        if !unchanged {
            report.client_write.add(CompatibilityLevel::Incompatible,
                if previous.is_some() { "reference_changed" } else { "reference_added" },
                format!("{path}.reference#{}", reference.id),
                "a reference can reject source writes and restrict target deletion or key changes previously accepted");
        }
    }
    for previous in baseline {
        if !candidate
            .iter()
            .any(|reference| reference.id == previous.id)
        {
            report.query.add(CompatibilityLevel::Conditional, "reference_removed", format!("{path}.reference#{}", previous.id),
                "the existence guarantee was removed; queries relying on related rows require review");
        }
    }
}
