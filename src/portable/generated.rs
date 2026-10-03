//! Generated defaults belong to table inputs, never reusable ADT field shapes.
use super::*;

fn invalid(message: impl Into<String>) -> Error {
    Error::new("E_CONTRACT_SCHEMA", message)
}

pub(super) fn validate(
    description: &SchemaDescription,
    types: &BTreeMap<u64, (&str, &TypeShape)>,
    ids: &mut BTreeSet<u64>,
) -> Result<()> {
    if description.version < 4
        && (!description.sequences.is_empty()
            || description
                .tables
                .iter()
                .any(|table| !table.generated_defaults.is_empty()))
    {
        return Err(invalid("generated defaults require description version 4"));
    }
    let mut sequences = BTreeSet::new();
    let mut names = description
        .types
        .iter()
        .map(|ty| ty.name.as_str())
        .chain(description.tables.iter().map(|table| table.name.as_str()))
        .collect::<BTreeSet<_>>();
    for sequence in &description.sequences {
        let id = insert_contract_id(ids, &sequence.id, "sequence ID")?;
        sequences.insert(id);
        if sequence.name.is_empty() || !names.insert(sequence.name.as_str()) {
            return Err(invalid("sequence name collides with another schema object"));
        }
        let start = sequence
            .start
            .parse::<i64>()
            .map_err(|_| invalid("sequence start must be an i64 decimal string"))?;
        if sequence.start != start.to_string() {
            return Err(invalid("sequence start must be canonical"));
        }
    }
    for table in &description.tables {
        let mut fields = BTreeSet::new();
        for policy in &table.generated_defaults {
            let id = parse_contract_id(&policy.field_id, "generated-default field ID", false)?;
            if !fields.insert(id) {
                return Err(invalid("duplicate table generated-default field"));
            }
            let (name, mut shape) = resolve_field_path(
                &table.row,
                std::slice::from_ref(&policy.field_id),
                types,
                "generated-default field",
            )?;
            if name != policy.field {
                return Err(invalid(
                    "generated-default field name disagrees with its ID",
                ));
            }
            for _ in 0..crate::model::MAX_DEPTH {
                let TypeShape::Ref { type_id, .. } = shape else {
                    break;
                };
                let id = parse_contract_id(type_id, "generated-default result type", false)?;
                shape = types
                    .get(&id)
                    .map(|(_, shape)| *shape)
                    .ok_or_else(|| invalid("unknown generated-default result type"))?;
            }
            let valid = match &policy.generator {
                GeneratorDescription::Next { sequence_id } => {
                    let id =
                        parse_contract_id(sequence_id, "generated-default sequence ID", false)?;
                    if !sequences.contains(&id) {
                        return Err(invalid("generated default references unknown sequence"));
                    }
                    matches!(shape, TypeShape::Int { .. })
                }
                GeneratorDescription::UuidV7 => matches!(shape, TypeShape::Uuid { .. }),
                GeneratorDescription::Now => matches!(shape, TypeShape::Timestamp { .. }),
            };
            if !valid {
                return Err(invalid("generated default has incompatible result type"));
            }
        }
    }
    Ok(())
}

fn policies(table: Option<&TableDescription>) -> BTreeMap<&str, &GeneratedDefaultDescription> {
    table
        .into_iter()
        .flat_map(|table| &table.generated_defaults)
        .map(|policy| (policy.field_id.as_str(), policy))
        .collect()
}

pub(super) fn validate_live(live: &SchemaDescription, declared: &SchemaDescription) -> Result<()> {
    let sequences = |description: &SchemaDescription| {
        description
            .sequences
            .iter()
            .map(|sequence| (sequence.id.clone(), sequence.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    if sequences(live) != sequences(declared) {
        return Err(invalid("sequence metadata does not match the live catalog"));
    }
    for table in live.tables.iter().chain(&declared.tables) {
        let left = live.tables.iter().find(|item| item.id == table.id);
        let right = declared.tables.iter().find(|item| item.id == table.id);
        if policies(left) != policies(right) {
            return Err(invalid(
                "generated-default metadata does not match the live catalog",
            ));
        }
    }
    Ok(())
}

fn has_constant_default(
    description: &SchemaDescription,
    table: &TableDescription,
    field_id: &str,
) -> bool {
    let mut shape = &table.row;
    for _ in 0..crate::model::MAX_DEPTH {
        let TypeShape::Ref { type_id, .. } = shape else {
            break;
        };
        let Some(ty) = description.types.iter().find(|ty| ty.id == *type_id) else {
            return false;
        };
        shape = &ty.shape;
    }
    let TypeShape::Record { fields } = shape else {
        return false;
    };
    fields
        .iter()
        .any(|field| field.id == field_id && field.input_omittable)
}

pub(super) fn compare(
    baseline: &SchemaDescription,
    candidate: &SchemaDescription,
    report: &mut EvolutionReport,
) {
    for old in &baseline.tables {
        let Some(new) = candidate.tables.iter().find(|table| table.id == old.id) else {
            continue;
        };
        let left = policies(Some(old));
        let right = policies(Some(new));
        for field in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
            let previous = left.get(field).map(|policy| &policy.generator);
            let next = right.get(field).map(|policy| &policy.generator);
            if previous == next {
                continue;
            };
            let path = format!("table {}.field#{field}", new.name);
            report.client_write.add(
                if next.is_none() && !has_constant_default(candidate, new, field) {
                    CompatibilityLevel::Incompatible
                } else {
                    CompatibilityLevel::Conditional
                },
                if next.is_none() {
                    "generated_default_removed"
                } else {
                    "generated_default_changed"
                },
                &path,
                "table input generation changed; review omitted-field writes",
            );
        }
    }
}
