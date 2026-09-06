use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::{Catalog, Column, EnumType, EnumVariantDef, ScalarType, Value};
use crate::query::{CmpOp, MatchCondition, MatchPattern, MatchPredicate};

/// Bind a parsed match predicate to catalog identities and payload types.
/// This runs before rows are scanned, including for an empty table.
pub(crate) fn bind(catalog: &Catalog, schema: &[Column], pred: &mut MatchPredicate) -> Result<()> {
    let source_ty = catalog.field_type(schema, &pred.column)?;
    let ScalarType::Enum(enum_ty) = catalog.underlying(source_ty)? else {
        return Err(Error::new(
            "E_TYPE",
            format!("match source '{}' must be a sum type", pred.column),
        ));
    };
    let enum_ty = enum_ty.clone();
    let mut covered = BTreeSet::new();
    let mut wildcard = false;
    let arm_count = pred.arms.len();
    for (index, arm) in pred.arms.iter_mut().enumerate() {
        let bindings = match &mut arm.pattern {
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
            MatchPattern::Variant {
                name,
                fields,
                record,
                rest,
                variant_id,
            } => {
                let (qualifier, variant_name) = name
                    .rsplit_once('.')
                    .map(|(qualifier, name)| (Some(qualifier), name))
                    .unwrap_or((None, name.as_str()));
                if let Some(qualifier) = qualifier
                    && !qualifier_matches_enum(catalog, qualifier, &enum_ty)?
                {
                    return Err(Error::new(
                        "E_MATCH",
                        format!(
                            "pattern '{name}' belongs to a different type than '{}'",
                            pred.column
                        ),
                    ));
                }
                let variant = enum_ty
                    .variants
                    .iter()
                    .find(|variant| variant.name == variant_name)
                    .ok_or_else(|| {
                        Error::new(
                            "E_MATCH",
                            format!("unknown variant '{name}' for '{}'", pred.column),
                        )
                    })?;
                if !covered.insert(variant.id) {
                    return Err(Error::new(
                        "E_MATCH",
                        format!("variant '{}' is matched more than once", variant.name),
                    ));
                }
                *variant_id = Some(variant.id);
                bind_record_pattern(catalog, variant, fields, *record, *rest)?
            }
        };
        bind_condition(catalog, &mut arm.condition, &bindings)?;
    }
    if !wildcard {
        let missing = enum_ty
            .variants
            .iter()
            .filter(|variant| !covered.contains(&variant.id))
            .map(|variant| variant.name.clone())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(Error::new(
                "E_MATCH",
                format!("non-exhaustive match; missing {}", missing.join(", ")),
            ));
        }
    }
    Ok(())
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

fn bind_record_pattern(
    catalog: &Catalog,
    variant: &EnumVariantDef,
    fields: &[String],
    record: bool,
    rest: bool,
) -> Result<Vec<Column>> {
    if variant.args.is_empty() {
        if record {
            return Err(Error::new(
                "E_MATCH",
                format!("unit variant '{}' has no record payload", variant.name),
            ));
        }
        return Ok(Vec::new());
    }
    let [payload] = variant.args.as_slice() else {
        return Err(Error::new(
            "E_MATCH",
            format!(
                "variant '{}' has positional payload; positional match patterns are not implemented yet",
                variant.name
            ),
        ));
    };
    let ScalarType::Record(payload_fields) = catalog.underlying(payload)? else {
        return Err(Error::new(
            "E_MATCH",
            format!(
                "variant '{}' has a positional payload; positional match patterns are not implemented yet",
                variant.name
            ),
        ));
    };
    if !record {
        return Err(Error::new(
            "E_MATCH",
            format!("variant '{}' requires a record pattern", variant.name),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut bindings = Vec::new();
    for field in fields {
        if !seen.insert(field) {
            return Err(Error::new(
                "E_MATCH",
                format!("field '{field}' is bound more than once"),
            ));
        }
        let definition = payload_fields
            .iter()
            .find(|definition| definition.name == *field)
            .ok_or_else(|| {
                Error::new(
                    "E_MATCH",
                    format!("variant '{}' has no payload field '{field}'", variant.name),
                )
            })?;
        bindings.push(definition.clone());
    }
    if !rest {
        let missing = payload_fields
            .iter()
            .filter(|field| !seen.contains(&field.name))
            .map(|field| field.name.clone())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(Error::new(
                "E_MATCH",
                format!(
                    "record pattern for '{}' omits {}; add '..' to ignore them",
                    variant.name,
                    missing.join(", ")
                ),
            ));
        }
    }
    Ok(bindings)
}

fn bind_condition(
    catalog: &Catalog,
    condition: &mut MatchCondition,
    bindings: &[Column],
) -> Result<()> {
    let path_type = |path: &str| {
        catalog.field_type(bindings, path).map_err(|_| {
            Error::new(
                "E_MATCH",
                format!("unknown match binding '{path}' in this branch"),
            )
        })
    };
    match condition {
        MatchCondition::Bool(_) => Ok(()),
        MatchCondition::Binding(binding) => {
            let ty = path_type(binding)?;
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
            let ty = path_type(binding)?;
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

fn orderable(catalog: &Catalog, ty: &ScalarType) -> Result<bool> {
    Ok(matches!(
        catalog.underlying(ty)?,
        ScalarType::Int | ScalarType::Float | ScalarType::Text
    ))
}

pub(crate) fn evaluate(row: &BTreeMap<String, Value>, pred: &MatchPredicate) -> bool {
    let Some(Value::Enum(value)) = row_field(row, &pred.column).map(Value::unwrapped) else {
        return false;
    };
    for arm in &pred.arms {
        let mut bindings = BTreeMap::new();
        let matched = match &arm.pattern {
            MatchPattern::Wildcard => true,
            MatchPattern::Variant {
                fields, variant_id, ..
            } => {
                if *variant_id != Some(value.id) {
                    false
                } else if fields.is_empty() {
                    true
                } else {
                    let Some(Value::Record(payload)) = value.args.first().map(Value::unwrapped)
                    else {
                        return false;
                    };
                    for field in fields {
                        let Some(value) = payload.get(field) else {
                            return false;
                        };
                        bindings.insert(field.as_str(), value);
                    }
                    true
                }
            }
        };
        if matched {
            return evaluate_condition(&bindings, &arm.condition);
        }
    }
    false
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
