use crate::error::{Error, Result};
use crate::model::{Catalog, Column, ScalarType, Value};
use crate::query::{BoolExpression, CmpOp, ScalarExpression};

pub(crate) fn bind(
    catalog: &Catalog,
    scope: &[Column],
    expression: &mut BoolExpression,
) -> Result<()> {
    bind_in_scope(catalog, scope, expression, "field", "filter condition")
}

pub(crate) fn bind_match(
    catalog: &Catalog,
    scope: &[Column],
    expression: &mut BoolExpression,
) -> Result<()> {
    bind_in_scope(
        catalog,
        scope,
        expression,
        "match binding",
        "match condition",
    )
}

fn bind_in_scope(
    catalog: &Catalog,
    scope: &[Column],
    expression: &mut BoolExpression,
    reference_kind: &str,
    condition_kind: &str,
) -> Result<()> {
    match expression {
        BoolExpression::Value(value) => {
            let ty = bind_scalar(catalog, scope, value, None, reference_kind)?;
            require_bool(catalog, &ty, condition_kind)
        }
        BoolExpression::Compare { left, op, right } => {
            let inferred_left = infer_scalar(catalog, scope, left, reference_kind)?;
            let inferred_right = infer_scalar(catalog, scope, right, reference_kind)?;
            let expected = match (&*left, &*right) {
                (ScalarExpression::Literal(_), _) => inferred_right.or(inferred_left),
                (_, ScalarExpression::Literal(_)) => inferred_left.or(inferred_right),
                _ => inferred_left.or(inferred_right),
            }
            .ok_or_else(|| {
                Error::new(
                    "E_TYPE",
                    "cannot infer comparison operand type; compare a field or typed value",
                )
            })?;
            let left_ty = bind_scalar(catalog, scope, left, Some(&expected), reference_kind)?;
            let right_ty = bind_scalar(catalog, scope, right, Some(&expected), reference_kind)?;
            if !same_type(&left_ty, &right_ty) {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "comparison operands have different types: {} and {}",
                        catalog.describe(&left_ty),
                        catalog.describe(&right_ty)
                    ),
                ));
            }
            if !matches!(op, CmpOp::Eq | CmpOp::Ne) && !orderable(catalog, &left_ty)? {
                return Err(Error::new(
                    "E_TYPE",
                    format!("type '{}' has no ordering", catalog.describe(&left_ty)),
                ));
            }
            Ok(())
        }
        BoolExpression::Contains { collection, item } => {
            let inferred_collection = infer_scalar(catalog, scope, collection, reference_kind)?;
            let (collection_ty, item_ty) = if let Some(collection_ty) = inferred_collection {
                let ScalarType::List(item_ty) = catalog.underlying(&collection_ty)? else {
                    return Err(Error::new(
                        "E_TYPE",
                        format!(
                            "contains expects a list, got {}",
                            catalog.describe(&collection_ty)
                        ),
                    ));
                };
                let item_ty = item_ty.as_ref().clone();
                (collection_ty, item_ty)
            } else {
                let item_ty =
                    infer_scalar(catalog, scope, item, reference_kind)?.ok_or_else(|| {
                        Error::new(
                            "E_TYPE",
                            "cannot infer contains element type from two untyped values",
                        )
                    })?;
                (ScalarType::List(Box::new(item_ty.clone())), item_ty)
            };
            bind_scalar(
                catalog,
                scope,
                collection,
                Some(&collection_ty),
                reference_kind,
            )?;
            bind_scalar(catalog, scope, item, Some(&item_ty), reference_kind)?;
            Ok(())
        }
        BoolExpression::Not(value) => {
            bind_in_scope(catalog, scope, value, reference_kind, condition_kind)
        }
        BoolExpression::And(left, right) | BoolExpression::Or(left, right) => {
            bind_in_scope(catalog, scope, left, reference_kind, condition_kind)?;
            bind_in_scope(catalog, scope, right, reference_kind, condition_kind)
        }
    }
}

fn bind_scalar(
    catalog: &Catalog,
    scope: &[Column],
    expression: &mut ScalarExpression,
    expected: Option<&ScalarType>,
    reference_kind: &str,
) -> Result<ScalarType> {
    let ty = match expression {
        ScalarExpression::Reference(path) => {
            reference_type(catalog, scope, path, reference_kind)?.clone()
        }
        ScalarExpression::Literal(value) => {
            if let Some(expected) = expected {
                *value = catalog.coerce(value, expected, "expression literal")?;
                expected.clone()
            } else {
                infer_literal(value)?.ok_or_else(|| {
                    Error::new(
                        "E_TYPE",
                        "cannot infer the type of this literal without a typed field",
                    )
                })?
            }
        }
        ScalarExpression::Length(value) => {
            let ty = bind_scalar(catalog, scope, value, None, reference_kind)?;
            if !matches!(
                catalog.underlying(&ty)?,
                ScalarType::List(_) | ScalarType::Text
            ) {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "length expects a list or text, got {}",
                        catalog.describe(&ty)
                    ),
                ));
            }
            ScalarType::Int
        }
    };
    if let Some(expected) = expected
        && !same_type(&ty, expected)
    {
        return Err(Error::new(
            "E_TYPE",
            format!(
                "expression has type {}, expected {}",
                catalog.describe(&ty),
                catalog.describe(expected)
            ),
        ));
    }
    Ok(ty)
}

fn infer_scalar(
    catalog: &Catalog,
    scope: &[Column],
    expression: &ScalarExpression,
    reference_kind: &str,
) -> Result<Option<ScalarType>> {
    match expression {
        ScalarExpression::Reference(path) => Ok(Some(
            reference_type(catalog, scope, path, reference_kind)?.clone(),
        )),
        ScalarExpression::Literal(value) => infer_literal(value),
        ScalarExpression::Length(value) => {
            if let Some(ty) = infer_scalar(catalog, scope, value, reference_kind)?
                && !matches!(
                    catalog.underlying(&ty)?,
                    ScalarType::List(_) | ScalarType::Text
                )
            {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "length expects a list or text, got {}",
                        catalog.describe(&ty)
                    ),
                ));
            }
            Ok(Some(ScalarType::Int))
        }
    }
}

fn reference_type<'a>(
    catalog: &'a Catalog,
    scope: &'a [Column],
    path: &str,
    reference_kind: &str,
) -> Result<&'a ScalarType> {
    catalog.field_type(scope, path).map_err(|error| {
        if reference_kind == "match binding" && error.code == "E_FIELD" {
            Error::new("E_MATCH", format!("unknown match binding '{path}'"))
        } else {
            error
        }
    })
}

fn infer_literal(value: &Value) -> Result<Option<ScalarType>> {
    Ok(match value {
        Value::Int(_) => Some(ScalarType::Int),
        Value::Float(_) => Some(ScalarType::Float),
        Value::Bool(_) => Some(ScalarType::Bool),
        Value::Text(_) => Some(ScalarType::Text),
        Value::Named { type_id, .. } => Some(ScalarType::Ref(*type_id)),
        Value::Tuple(values) => values
            .iter()
            .map(infer_literal)
            .collect::<Result<Option<Vec<_>>>>()?
            .map(ScalarType::Tuple),
        Value::List(values) if !values.is_empty() => {
            let Some(first) = infer_literal(&values[0])? else {
                return Ok(None);
            };
            let rest = values
                .iter()
                .skip(1)
                .map(infer_literal)
                .collect::<Result<Vec<_>>>()?;
            if rest
                .iter()
                .all(|ty| ty.as_ref().is_some_and(|ty| same_type(&first, ty)))
            {
                Some(ScalarType::List(Box::new(first)))
            } else {
                None
            }
        }
        Value::Record(_) | Value::Enum(_) | Value::List(_) | Value::Option(_) | Value::Null => None,
    })
}

fn require_bool(catalog: &Catalog, ty: &ScalarType, context: &str) -> Result<()> {
    if matches!(catalog.underlying(ty)?, ScalarType::Bool) {
        Ok(())
    } else {
        Err(Error::new(
            "E_TYPE",
            format!("{context} must be bool, got {}", catalog.describe(ty)),
        ))
    }
}

fn orderable(catalog: &Catalog, ty: &ScalarType) -> Result<bool> {
    Ok(matches!(
        catalog.underlying(ty)?,
        ScalarType::Int | ScalarType::Float | ScalarType::Text
    ))
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
                    .all(|(left, right)| {
                        left.id == right.id
                            && left.args.len() == right.args.len()
                            && left
                                .args
                                .iter()
                                .zip(&right.args)
                                .all(|(left, right)| same_type(left, right))
                    })
        }
        _ => false,
    }
}

pub(crate) fn evaluate<'a>(
    expression: &'a BoolExpression,
    values: impl Fn(&str) -> Option<&'a Value> + Copy,
) -> bool {
    match expression {
        BoolExpression::Value(value) => {
            let Some(value) = evaluate_scalar(value, values) else {
                return false;
            };
            matches!(value.as_value().unwrapped(), Value::Bool(true))
        }
        BoolExpression::Compare { left, op, right } => {
            let Some(left) = evaluate_scalar(left, values) else {
                return false;
            };
            let Some(right) = evaluate_scalar(right, values) else {
                return false;
            };
            compare(left.as_value(), *op, right.as_value())
        }
        BoolExpression::Contains { collection, item } => {
            let Some(collection) = evaluate_scalar(collection, values) else {
                return false;
            };
            let Some(item) = evaluate_scalar(item, values) else {
                return false;
            };
            matches!(collection.as_value().unwrapped(), Value::List(values) if values.iter().any(|value| value.cmp_eq(item.as_value())))
        }
        BoolExpression::Not(value) => !evaluate(value, values),
        BoolExpression::And(left, right) => evaluate(left, values) && evaluate(right, values),
        BoolExpression::Or(left, right) => evaluate(left, values) || evaluate(right, values),
    }
}

fn evaluate_scalar<'a>(
    expression: &'a ScalarExpression,
    values: impl Fn(&str) -> Option<&'a Value> + Copy,
) -> Option<Evaluated<'a>> {
    match expression {
        ScalarExpression::Reference(path) => values(path).map(Evaluated::Borrowed),
        ScalarExpression::Literal(value) => Some(Evaluated::Borrowed(value)),
        ScalarExpression::Length(value) => {
            let value = evaluate_scalar(value, values)?;
            let length = match value.as_value().unwrapped() {
                Value::List(values) => values.len(),
                Value::Text(value) => value.chars().count(),
                _ => return None,
            };
            i64::try_from(length)
                .ok()
                .map(|value| Evaluated::Owned(Value::Int(value)))
        }
    }
}

enum Evaluated<'a> {
    Borrowed(&'a Value),
    Owned(Value),
}

impl Evaluated<'_> {
    fn as_value(&self) -> &Value {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}

fn compare(left: &Value, op: CmpOp, right: &Value) -> bool {
    match op {
        CmpOp::Eq => left.cmp_eq(right),
        CmpOp::Ne => !left.cmp_eq(right),
        CmpOp::Gt => left.cmp_ord(right) == Some(std::cmp::Ordering::Greater),
        CmpOp::Lt => left.cmp_ord(right) == Some(std::cmp::Ordering::Less),
        CmpOp::Gte => matches!(
            left.cmp_ord(right),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        CmpOp::Lte => matches!(
            left.cmp_ord(right),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}

pub(crate) fn simple_index_equality(expression: &BoolExpression) -> Option<(&str, &Value)> {
    let BoolExpression::Compare {
        left,
        op: CmpOp::Eq,
        right,
    } = expression
    else {
        return None;
    };
    match (left, right) {
        (ScalarExpression::Reference(path), ScalarExpression::Literal(value))
        | (ScalarExpression::Literal(value), ScalarExpression::Reference(path)) => {
            Some((path, value))
        }
        _ => None,
    }
}
