use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::{Catalog, Column, EnumType, ScalarType, Value};
use crate::query::{
    CmpOp, DeriveMatch, MatchCondition, MatchPattern, MatchPayload, MatchPredicate, MatchTag,
    MatchValue,
};

/// Bind a parsed match predicate to catalog identities and payload types.
/// This runs before rows are scanned, including for an empty table.
pub(crate) fn bind(catalog: &Catalog, schema: &[Column], pred: &mut MatchPredicate) -> Result<()> {
    let source_ty = catalog.field_type(schema, &pred.column)?;
    let arm_count = pred.arms.len();
    let bindings = bind_patterns(
        catalog,
        &pred.column,
        source_ty,
        pred.arms.iter_mut().map(|arm| &mut arm.pattern),
        arm_count,
    )?;
    for (arm, bindings) in pred.arms.iter_mut().zip(&bindings) {
        bind_condition(catalog, &mut arm.condition, bindings)?;
    }
    Ok(())
}

/// Bind a match expression and return the column it appends to the pipeline.
pub(crate) fn bind_derive(
    catalog: &Catalog,
    schema: &[Column],
    derive: &mut DeriveMatch,
) -> Result<Column> {
    if schema.iter().any(|column| column.name == derive.name) {
        return Err(Error::new(
            "E_FIELD",
            format!(
                "derive field '{}' already exists; choose a new field name",
                derive.name
            ),
        ));
    }
    let source_ty = catalog.field_type(schema, &derive.source)?;
    let arm_count = derive.arms.len();
    let bindings = bind_patterns(
        catalog,
        &derive.source,
        source_ty,
        derive.arms.iter_mut().map(|arm| &mut arm.pattern),
        arm_count,
    )?;

    let output_type = derive
        .arms
        .iter()
        .zip(&bindings)
        .find_map(|(arm, bindings)| infer_result_type(catalog, &arm.result, bindings).transpose())
        .transpose()?
        .ok_or_else(|| {
            Error::new(
                "E_TYPE",
                format!(
                    "cannot infer type of derived field '{}'; return a typed binding or primitive literal in at least one branch",
                    derive.name
                ),
            )
        })?;

    for (arm, bindings) in derive.arms.iter_mut().zip(&bindings) {
        bind_result(
            catalog,
            &derive.name,
            &output_type,
            &mut arm.result,
            bindings,
        )?;
    }
    derive.output_type = Some(output_type.clone());
    Ok(Column {
        name: derive.name.clone(),
        ty: output_type,
        default: None,
        id: 0,
    })
}

enum MatchSource {
    Enum(EnumType),
    Option(ScalarType),
}

fn bind_patterns<'a>(
    catalog: &Catalog,
    source_name: &str,
    source_ty: &ScalarType,
    patterns: impl IntoIterator<Item = &'a mut MatchPattern>,
    arm_count: usize,
) -> Result<Vec<Vec<Column>>> {
    let source = match catalog.underlying(source_ty)? {
        ScalarType::Enum(enum_type) => MatchSource::Enum(enum_type.clone()),
        ScalarType::Option(inner) => MatchSource::Option(inner.as_ref().clone()),
        _ => {
            return Err(Error::new(
                "E_TYPE",
                format!("match source '{source_name}' must be a sum type or option"),
            ));
        }
    };
    let mut covered = BTreeSet::new();
    let mut wildcard = false;
    let mut all_bindings = Vec::with_capacity(arm_count);
    for (index, pattern) in patterns.into_iter().enumerate() {
        let bindings = match pattern {
            MatchPattern::Wildcard => {
                if index + 1 != arm_count {
                    return Err(Error::new(
                        "E_MATCH",
                        "wildcard match branch must be last; later branches are unreachable",
                    ));
                }
                wildcard = true;
                Vec::new()
            }
            MatchPattern::Constructor { name, payload, tag } => {
                let (resolved_tag, argument_types, display_name) =
                    resolve_constructor(catalog, &source, source_name, name)?;
                if !covered.insert(resolved_tag) {
                    return Err(Error::new(
                        "E_MATCH",
                        format!("constructor '{display_name}' is matched more than once"),
                    ));
                }
                *tag = Some(resolved_tag);
                bind_payload(catalog, &display_name, &argument_types, payload)?
            }
        };
        all_bindings.push(bindings);
    }
    if !wildcard {
        let missing = match &source {
            MatchSource::Enum(enum_type) => enum_type
                .variants
                .iter()
                .filter(|variant| !covered.contains(&MatchTag::Variant(variant.id)))
                .map(|variant| variant.name.clone())
                .collect::<Vec<_>>(),
            MatchSource::Option(_) => [(MatchTag::None, "None"), (MatchTag::Some, "Some")]
                .into_iter()
                .filter(|(tag, _)| !covered.contains(tag))
                .map(|(_, name)| name.into())
                .collect(),
        };
        if !missing.is_empty() {
            return Err(Error::new(
                "E_MATCH",
                format!("non-exhaustive match; missing {}", missing.join(", ")),
            ));
        }
    }
    Ok(all_bindings)
}

fn resolve_constructor(
    catalog: &Catalog,
    source: &MatchSource,
    source_name: &str,
    name: &str,
) -> Result<(MatchTag, Vec<ScalarType>, String)> {
    match source {
        MatchSource::Enum(enum_type) => {
            let (qualifier, variant_name) = name
                .rsplit_once('.')
                .map(|(qualifier, name)| (Some(qualifier), name))
                .unwrap_or((None, name));
            if let Some(qualifier) = qualifier
                && !qualifier_matches_enum(catalog, qualifier, enum_type)?
            {
                return Err(Error::new(
                    "E_MATCH",
                    format!("pattern '{name}' belongs to a different type than '{source_name}'"),
                ));
            }
            let variant = enum_type
                .variants
                .iter()
                .find(|variant| variant.name == variant_name)
                .ok_or_else(|| {
                    Error::new(
                        "E_MATCH",
                        format!("unknown variant '{name}' for '{source_name}'"),
                    )
                })?;
            Ok((
                MatchTag::Variant(variant.id),
                variant.args.clone(),
                variant.name.clone(),
            ))
        }
        MatchSource::Option(inner) => match name {
            "None" => Ok((MatchTag::None, Vec::new(), "None".into())),
            "Some" => Ok((MatchTag::Some, vec![inner.clone()], "Some".into())),
            _ => Err(Error::new(
                "E_MATCH",
                format!("unknown option constructor '{name}' for '{source_name}'"),
            )),
        },
    }
}

fn qualifier_matches_enum(catalog: &Catalog, qualifier: &str, expected: &EnumType) -> Result<bool> {
    let definition = catalog
        .types
        .get(qualifier)
        .ok_or_else(|| Error::new("E_MATCH", format!("unknown type qualifier '{qualifier}'")))?;
    let ScalarType::Enum(actual) = catalog.underlying(&definition.ty)? else {
        return Ok(false);
    };
    Ok(actual.variants.len() == expected.variants.len()
        && actual
            .variants
            .iter()
            .zip(&expected.variants)
            .all(|(actual, expected)| actual.id == expected.id))
}

fn bind_payload(
    catalog: &Catalog,
    constructor: &str,
    argument_types: &[ScalarType],
    payload: &MatchPayload,
) -> Result<Vec<Column>> {
    match payload {
        MatchPayload::Unit => {
            if argument_types.is_empty() {
                Ok(Vec::new())
            } else {
                Err(Error::new(
                    "E_MATCH",
                    format!(
                        "constructor '{constructor}' expects {} payload binding(s)",
                        argument_types.len()
                    ),
                ))
            }
        }
        MatchPayload::Record { fields, rest } => {
            let [payload] = argument_types else {
                return Err(Error::new(
                    "E_MATCH",
                    format!(
                        "constructor '{constructor}' has no record payload with exactly one argument"
                    ),
                ));
            };
            let ScalarType::Record(payload_fields) = catalog.underlying(payload)? else {
                return Err(Error::new(
                    "E_MATCH",
                    format!("constructor '{constructor}' has no record payload"),
                ));
            };
            let mut seen_fields = BTreeSet::new();
            let mut seen_bindings = BTreeSet::new();
            let mut bindings = Vec::new();
            for field in fields {
                if !seen_fields.insert(&field.field) {
                    return Err(Error::new(
                        "E_MATCH",
                        format!("field '{}' is bound more than once", field.field),
                    ));
                }
                if !seen_bindings.insert(&field.binding) {
                    return Err(Error::new(
                        "E_MATCH",
                        format!("binding '{}' is declared more than once", field.binding),
                    ));
                }
                let definition = payload_fields
                    .iter()
                    .find(|definition| definition.name == field.field)
                    .ok_or_else(|| {
                        Error::new(
                            "E_MATCH",
                            format!(
                                "constructor '{constructor}' has no payload field '{}'",
                                field.field
                            ),
                        )
                    })?;
                let mut definition = definition.clone();
                definition.name = field.binding.clone();
                bindings.push(definition);
            }
            if !rest {
                let missing = payload_fields
                    .iter()
                    .filter(|field| !seen_fields.contains(&field.name))
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    return Err(Error::new(
                        "E_MATCH",
                        format!(
                            "record pattern for '{constructor}' omits {}; add '..' to ignore them",
                            missing.join(", ")
                        ),
                    ));
                }
            }
            Ok(bindings)
        }
        MatchPayload::Positional(names) => {
            if names.len() != argument_types.len() {
                return Err(Error::new(
                    "E_MATCH",
                    format!(
                        "constructor '{constructor}' expects {} payload binding(s), got {}",
                        argument_types.len(),
                        names.len()
                    ),
                ));
            }
            let mut seen = BTreeSet::new();
            let mut bindings = Vec::new();
            for (name, ty) in names.iter().zip(argument_types) {
                if name == "_" {
                    continue;
                }
                if !seen.insert(name) {
                    return Err(Error::new(
                        "E_MATCH",
                        format!("binding '{name}' is declared more than once"),
                    ));
                }
                bindings.push(Column {
                    name: name.clone(),
                    ty: ty.clone(),
                    default: None,
                    id: 0,
                });
            }
            Ok(bindings)
        }
    }
}

fn bind_condition(
    catalog: &Catalog,
    condition: &mut MatchCondition,
    bindings: &[Column],
) -> Result<()> {
    match condition {
        MatchCondition::Bool(_) => Ok(()),
        MatchCondition::Binding(binding) => {
            let ty = binding_type(catalog, bindings, binding)?;
            if matches!(catalog.underlying(ty)?, ScalarType::Bool) {
                Ok(())
            } else {
                Err(Error::new(
                    "E_TYPE",
                    format!("match condition '{binding}' must be bool"),
                ))
            }
        }
        MatchCondition::Compare { binding, op, value } => {
            let ty = binding_type(catalog, bindings, binding)?;
            *value = catalog.coerce(value, ty, binding)?;
            if !matches!(op, CmpOp::Eq | CmpOp::Ne) && !orderable(catalog, ty)? {
                return Err(Error::new(
                    "E_TYPE",
                    format!("match binding '{binding}' has no ordering"),
                ));
            }
            Ok(())
        }
    }
}

fn infer_result_type(
    catalog: &Catalog,
    result: &MatchValue,
    bindings: &[Column],
) -> Result<Option<ScalarType>> {
    match result {
        MatchValue::Binding(path) => Ok(Some(binding_type(catalog, bindings, path)?.clone())),
        MatchValue::Literal(value) => literal_type(catalog, value),
    }
}

fn literal_type(catalog: &Catalog, value: &Value) -> Result<Option<ScalarType>> {
    Ok(match value {
        Value::Int(_) => Some(ScalarType::Int),
        Value::Float(_) => Some(ScalarType::Float),
        Value::Bool(_) => Some(ScalarType::Bool),
        Value::Text(_) => Some(ScalarType::Text),
        Value::Named { type_id, .. } => Some(ScalarType::Ref(*type_id)),
        Value::Tuple(values) => {
            let types = values
                .iter()
                .map(|value| literal_type(catalog, value))
                .collect::<Result<Option<Vec<_>>>>()?;
            types.map(ScalarType::Tuple)
        }
        Value::List(values) if !values.is_empty() => {
            let Some(first) = literal_type(catalog, &values[0])? else {
                return Ok(None);
            };
            if values
                .iter()
                .skip(1)
                .map(|value| literal_type(catalog, value))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|ty| ty.as_ref().is_some_and(|ty| same_type(&first, ty)))
            {
                Some(ScalarType::List(Box::new(first)))
            } else {
                None
            }
        }
        Value::Option(Some(value)) => {
            literal_type(catalog, value)?.map(|ty| ScalarType::Option(Box::new(ty)))
        }
        Value::Enum(value) => value
            .variant
            .rsplit_once('.')
            .and_then(|(qualifier, _)| catalog.types.get(qualifier))
            .map(|definition| ScalarType::Ref(definition.id)),
        Value::Null | Value::Record(_) | Value::List(_) | Value::Option(None) => None,
    })
}

fn bind_result(
    catalog: &Catalog,
    derive_name: &str,
    expected: &ScalarType,
    result: &mut MatchValue,
    bindings: &[Column],
) -> Result<()> {
    match result {
        MatchValue::Binding(path) => {
            let actual = binding_type(catalog, bindings, path)?;
            if same_type(expected, actual) {
                Ok(())
            } else {
                Err(Error::new(
                    "E_TYPE",
                    format!(
                        "derive '{derive_name}' branch returns {}, expected {}",
                        catalog.describe(actual),
                        catalog.describe(expected)
                    ),
                ))
            }
        }
        MatchValue::Literal(value) => {
            *value = catalog.coerce(value, expected, &format!("derive '{derive_name}' branch"))?;
            Ok(())
        }
    }
}

fn binding_type<'a>(
    catalog: &'a Catalog,
    bindings: &'a [Column],
    path: &str,
) -> Result<&'a ScalarType> {
    catalog.field_type(bindings, path).map_err(|_| {
        Error::new(
            "E_MATCH",
            format!("unknown match binding '{path}' in this branch"),
        )
    })
}

fn same_type(left: &ScalarType, right: &ScalarType) -> bool {
    match (left, right) {
        (ScalarType::Int, ScalarType::Int)
        | (ScalarType::Float, ScalarType::Float)
        | (ScalarType::Bool, ScalarType::Bool)
        | (ScalarType::Text, ScalarType::Text) => true,
        (ScalarType::Ref(left), ScalarType::Ref(right)) => left == right,
        (ScalarType::Option(left), ScalarType::Option(right))
        | (ScalarType::List(left), ScalarType::List(right)) => same_type(left, right),
        (ScalarType::Tuple(left), ScalarType::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_type(left, right))
        }
        (ScalarType::Record(left), ScalarType::Record(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| left.name == right.name && same_type(&left.ty, &right.ty))
        }
        (ScalarType::Enum(left), ScalarType::Enum(right)) => {
            left.variants.len() == right.variants.len()
                && left
                    .variants
                    .iter()
                    .zip(&right.variants)
                    .all(|(left, right)| left.id != 0 && left.id == right.id)
        }
        (ScalarType::Named(left), ScalarType::Named(right)) => left == right,
        _ => false,
    }
}

fn orderable(catalog: &Catalog, ty: &ScalarType) -> Result<bool> {
    Ok(matches!(
        catalog.underlying(ty)?,
        ScalarType::Int | ScalarType::Float | ScalarType::Text
    ))
}

pub(crate) fn evaluate(row: &BTreeMap<String, Value>, pred: &MatchPredicate) -> bool {
    let Some(value) = row_field(row, &pred.column) else {
        return false;
    };
    for arm in &pred.arms {
        if let Some(bindings) = match_bindings(value, &arm.pattern) {
            return evaluate_condition(&bindings, &arm.condition);
        }
    }
    false
}

pub(crate) fn evaluate_derive(
    row: &BTreeMap<String, Value>,
    derive: &DeriveMatch,
) -> Result<Value> {
    let value = row_field(row, &derive.source).ok_or_else(|| {
        Error::new(
            "E_FIELD",
            format!("missing match source '{}' during execution", derive.source),
        )
    })?;
    for arm in &derive.arms {
        if let Some(bindings) = match_bindings(value, &arm.pattern) {
            return match &arm.result {
                MatchValue::Binding(path) => binding_value(&bindings, path)
                    .cloned()
                    .ok_or_else(|| Error::new("E_MATCH", format!("missing binding '{path}'"))),
                MatchValue::Literal(value) => Ok(value.clone()),
            };
        }
    }
    Err(Error::new(
        "E_MATCH",
        "exhaustive match did not select a branch",
    ))
}

fn match_bindings<'p, 'v>(
    value: &'v Value,
    pattern: &'p MatchPattern,
) -> Option<BTreeMap<&'p str, &'v Value>> {
    let MatchPattern::Constructor { payload, tag, .. } = pattern else {
        return Some(BTreeMap::new());
    };
    let tag = tag.as_ref()?;
    let value = value.unwrapped();
    match (tag, value) {
        (MatchTag::Variant(expected), Value::Enum(value)) if expected == &value.id => {
            payload_bindings(payload, &value.args)
        }
        (MatchTag::None, Value::Option(None)) => payload_bindings(payload, &[]),
        (MatchTag::Some, Value::Option(Some(value))) => {
            payload_bindings(payload, std::slice::from_ref(value.as_ref()))
        }
        _ => None,
    }
}

fn payload_bindings<'p, 'v>(
    payload: &'p MatchPayload,
    values: &'v [Value],
) -> Option<BTreeMap<&'p str, &'v Value>> {
    let mut bindings = BTreeMap::new();
    match payload {
        MatchPayload::Unit => {}
        MatchPayload::Record { fields, .. } => {
            let Value::Record(record) = values.first()?.unwrapped() else {
                return None;
            };
            for field in fields {
                bindings.insert(field.binding.as_str(), record.get(&field.field)?);
            }
        }
        MatchPayload::Positional(names) => {
            if names.len() != values.len() {
                return None;
            }
            for (name, value) in names.iter().zip(values) {
                if name != "_" {
                    bindings.insert(name.as_str(), value);
                }
            }
        }
    }
    Some(bindings)
}

fn evaluate_condition(bindings: &BTreeMap<&str, &Value>, condition: &MatchCondition) -> bool {
    match condition {
        MatchCondition::Bool(value) => *value,
        MatchCondition::Binding(binding) => {
            matches!(
                binding_value(bindings, binding).map(Value::unwrapped),
                Some(Value::Bool(true))
            )
        }
        MatchCondition::Compare { binding, op, value } => {
            binding_value(bindings, binding).is_some_and(|binding| compare(binding, *op, value))
        }
    }
}

fn binding_value<'a>(bindings: &BTreeMap<&str, &'a Value>, path: &str) -> Option<&'a Value> {
    let (head, tail) = path.split_once('.').unwrap_or((path, ""));
    let value = *bindings.get(head)?;
    if tail.is_empty() {
        Some(value)
    } else {
        value.field(tail)
    }
}

fn row_field<'a>(row: &'a BTreeMap<String, Value>, path: &str) -> Option<&'a Value> {
    if let Some(value) = row.get(path) {
        return Some(value);
    }
    let (head, tail) = path.split_once('.')?;
    row.get(head)?.field(tail)
}

fn compare(lhs: &Value, op: CmpOp, rhs: &Value) -> bool {
    match op {
        CmpOp::Eq => lhs.cmp_eq(rhs),
        CmpOp::Ne => !lhs.cmp_eq(rhs),
        CmpOp::Gt => lhs.cmp_ord(rhs) == Some(std::cmp::Ordering::Greater),
        CmpOp::Lt => lhs.cmp_ord(rhs) == Some(std::cmp::Ordering::Less),
        CmpOp::Gte => matches!(
            lhs.cmp_ord(rhs),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        CmpOp::Lte => matches!(
            lhs.cmp_ord(rhs),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}
