use crate::error::{Error, Result};
use crate::model::{Catalog, Column, ScalarType, Value};
use crate::query::{ArithmeticOp, BoolExpression, CmpOp, ScalarExpression};

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
            let expected = if is_constant(left) {
                inferred_right.or(inferred_left)
            } else {
                inferred_left.or(inferred_right)
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

pub(crate) fn bind_scalar(
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
        ScalarExpression::Parameter { name, ty } => {
            match expected.cloned().or_else(|| ty.clone()) {
                Some(expected) => {
                    *ty = Some(expected.clone());
                    expected
                }
                None => {
                    return Err(Error::new(
                        "E_TYPE",
                        format!(
                            "cannot infer parameter '${name}' type; compare it with a typed field or value"
                        ),
                    ));
                }
            }
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
        ScalarExpression::Negate { value, ty } => {
            let inferred = infer_scalar(catalog, scope, value, reference_kind)?;
            let result = expected.cloned().or(inferred).ok_or_else(|| {
                Error::new(
                    "E_TYPE",
                    "cannot infer unary '-' operand type; use an int or float value",
                )
            })?;
            require_numeric(catalog, &result, "unary '-'")?;
            bind_scalar(catalog, scope, value, Some(&result), reference_kind)?;
            *ty = Some(result.clone());
            result
        }
        ScalarExpression::Arithmetic {
            left,
            op: _,
            right,
            ty,
        } => {
            let inferred = infer_arithmetic_type(catalog, scope, left, right, reference_kind)?;
            let result = expected.cloned().or(inferred).ok_or_else(|| {
                Error::new(
                    "E_TYPE",
                    "cannot infer arithmetic operand type; use int or float values",
                )
            })?;
            require_numeric(catalog, &result, "arithmetic")?;
            bind_scalar(catalog, scope, left, Some(&result), reference_kind)?;
            bind_scalar(catalog, scope, right, Some(&result), reference_kind)?;
            *ty = Some(result.clone());
            result
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

pub(crate) fn infer_scalar(
    catalog: &Catalog,
    scope: &[Column],
    expression: &ScalarExpression,
    reference_kind: &str,
) -> Result<Option<ScalarType>> {
    match expression {
        ScalarExpression::Reference(path) => Ok(Some(
            reference_type(catalog, scope, path, reference_kind)?.clone(),
        )),
        ScalarExpression::Parameter { ty, .. } => Ok(ty.clone()),
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
        ScalarExpression::Negate { value, .. } => {
            let ty = infer_scalar(catalog, scope, value, reference_kind)?;
            if let Some(ty) = &ty {
                require_numeric(catalog, ty, "unary '-'")?;
            }
            Ok(ty)
        }
        ScalarExpression::Arithmetic { left, right, .. } => {
            infer_arithmetic_type(catalog, scope, left, right, reference_kind)
        }
    }
}

fn infer_arithmetic_type(
    catalog: &Catalog,
    scope: &[Column],
    left: &ScalarExpression,
    right: &ScalarExpression,
    reference_kind: &str,
) -> Result<Option<ScalarType>> {
    let left_ty = infer_scalar(catalog, scope, left, reference_kind)?;
    let right_ty = infer_scalar(catalog, scope, right, reference_kind)?;
    let result = match (left_ty, right_ty) {
        (Some(left_ty), Some(right_ty)) if same_type(&left_ty, &right_ty) => Some(left_ty),
        (_, Some(right_ty)) if is_constant(left) => Some(right_ty),
        (Some(left_ty), _) if is_constant(right) => Some(left_ty),
        (Some(left_ty), Some(right_ty)) => {
            return Err(Error::new(
                "E_TYPE",
                format!(
                    "arithmetic operands have different types: {} and {}",
                    catalog.describe(&left_ty),
                    catalog.describe(&right_ty)
                ),
            ));
        }
        (Some(ty), None) | (None, Some(ty)) => Some(ty),
        (None, None) => None,
    };
    if let Some(ty) = &result {
        require_numeric(catalog, ty, "arithmetic")?;
    }
    Ok(result)
}

fn is_constant(expression: &ScalarExpression) -> bool {
    match expression {
        ScalarExpression::Literal(_) => true,
        ScalarExpression::Parameter { .. } => true,
        ScalarExpression::Reference(_) => false,
        ScalarExpression::Length(value) | ScalarExpression::Negate { value, .. } => {
            is_constant(value)
        }
        ScalarExpression::Arithmetic { left, right, .. } => is_constant(left) && is_constant(right),
    }
}

fn require_numeric(catalog: &Catalog, ty: &ScalarType, context: &str) -> Result<()> {
    if matches!(catalog.underlying(ty)?, ScalarType::Int | ScalarType::Float) {
        Ok(())
    } else {
        Err(Error::new(
            "E_TYPE",
            format!(
                "{context} expects int or float, got {}",
                catalog.describe(ty)
            ),
        ))
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

pub(crate) fn same_type(left: &ScalarType, right: &ScalarType) -> bool {
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
    catalog: &Catalog,
    expression: &'a BoolExpression,
    values: impl Fn(&str) -> Option<&'a Value> + Copy,
) -> Result<bool> {
    match expression {
        BoolExpression::Value(value) => {
            let Some(value) = evaluate_scalar(catalog, value, values)? else {
                return Ok(false);
            };
            Ok(matches!(value.as_value().unwrapped(), Value::Bool(true)))
        }
        BoolExpression::Compare { left, op, right } => {
            let Some(left) = evaluate_scalar(catalog, left, values)? else {
                return Ok(false);
            };
            let Some(right) = evaluate_scalar(catalog, right, values)? else {
                return Ok(false);
            };
            Ok(compare(left.as_value(), *op, right.as_value()))
        }
        BoolExpression::Contains { collection, item } => {
            let Some(collection) = evaluate_scalar(catalog, collection, values)? else {
                return Ok(false);
            };
            let Some(item) = evaluate_scalar(catalog, item, values)? else {
                return Ok(false);
            };
            Ok(
                matches!(collection.as_value().unwrapped(), Value::List(values) if values.iter().any(|value| value.cmp_eq(item.as_value()))),
            )
        }
        BoolExpression::Not(value) => Ok(!evaluate(catalog, value, values)?),
        BoolExpression::And(left, right) => {
            if !evaluate(catalog, left, values)? {
                return Ok(false);
            }
            evaluate(catalog, right, values)
        }
        BoolExpression::Or(left, right) => {
            if evaluate(catalog, left, values)? {
                return Ok(true);
            }
            evaluate(catalog, right, values)
        }
    }
}

fn evaluate_scalar<'a>(
    catalog: &Catalog,
    expression: &'a ScalarExpression,
    values: impl Fn(&str) -> Option<&'a Value> + Copy,
) -> Result<Option<Evaluated<'a>>> {
    match expression {
        ScalarExpression::Reference(path) => Ok(values(path).map(Evaluated::Borrowed)),
        ScalarExpression::Parameter { name, .. } => Err(Error::new(
            "E_PARAM_MISSING",
            format!("parameter '${name}' was not bound"),
        )),
        ScalarExpression::Literal(value) => Ok(Some(Evaluated::Borrowed(value))),
        ScalarExpression::Length(value) => {
            let Some(value) = evaluate_scalar(catalog, value, values)? else {
                return Ok(None);
            };
            let length = match value.as_value().unwrapped() {
                Value::List(values) => values.len(),
                Value::Text(value) => value.chars().count(),
                _ => return Ok(None),
            };
            let length = i64::try_from(length)
                .map_err(|_| Error::new("E_LIMIT", "length exceeds i64 range"))?;
            Ok(Some(Evaluated::Owned(Value::Int(length))))
        }
        ScalarExpression::Negate { value, ty } => {
            let Some(value) = evaluate_scalar(catalog, value, values)? else {
                return Ok(None);
            };
            let raw = match value.as_value().unwrapped() {
                Value::Int(value) => Value::Int(
                    value
                        .checked_neg()
                        .ok_or_else(|| Error::new("E_ARITH", "integer overflow in unary '-'"))?,
                ),
                Value::Float(value) => finite_float(-value, "unary '-'")?,
                _ => return Err(Error::new("E_TYPE", "non-numeric unary '-' operand")),
            };
            Ok(Some(Evaluated::Owned(coerce_arithmetic_result(
                catalog, ty, raw,
            )?)))
        }
        ScalarExpression::Arithmetic {
            left,
            op,
            right,
            ty,
        } => {
            let Some(left) = evaluate_scalar(catalog, left, values)? else {
                return Ok(None);
            };
            let Some(right) = evaluate_scalar(catalog, right, values)? else {
                return Ok(None);
            };
            let raw = evaluate_arithmetic(left.as_value(), *op, right.as_value())?;
            Ok(Some(Evaluated::Owned(coerce_arithmetic_result(
                catalog, ty, raw,
            )?)))
        }
    }
}

pub(crate) fn evaluate_value<'a>(
    catalog: &Catalog,
    expression: &'a ScalarExpression,
    values: impl Fn(&str) -> Option<&'a Value> + Copy,
) -> Result<Value> {
    evaluate_scalar(catalog, expression, values)?
        .map(|value| value.as_value().clone())
        .ok_or_else(|| Error::new("E_QUERY", "bound scalar expression has no runtime value"))
}

fn coerce_arithmetic_result(
    catalog: &Catalog,
    ty: &Option<ScalarType>,
    value: Value,
) -> Result<Value> {
    let ty = ty
        .as_ref()
        .ok_or_else(|| Error::new("E_TYPE", "arithmetic expression was not bound"))?;
    catalog.coerce(&value, ty, "arithmetic result")
}

fn evaluate_arithmetic(left: &Value, op: ArithmeticOp, right: &Value) -> Result<Value> {
    match (left.unwrapped(), right.unwrapped()) {
        (Value::Int(left), Value::Int(right)) => {
            let value = match op {
                ArithmeticOp::Add => left.checked_add(*right),
                ArithmeticOp::Subtract => left.checked_sub(*right),
                ArithmeticOp::Multiply => left.checked_mul(*right),
                ArithmeticOp::Divide if *right == 0 => {
                    return Err(Error::new("E_ARITH", "integer division by zero"));
                }
                ArithmeticOp::Divide => left.checked_div(*right),
            }
            .ok_or_else(|| Error::new("E_ARITH", "integer arithmetic overflow"))?;
            Ok(Value::Int(value))
        }
        (Value::Float(left), Value::Float(right)) => {
            if matches!(op, ArithmeticOp::Divide) && *right == 0.0 {
                return Err(Error::new("E_ARITH", "float division by zero"));
            }
            let value = match op {
                ArithmeticOp::Add => left + right,
                ArithmeticOp::Subtract => left - right,
                ArithmeticOp::Multiply => left * right,
                ArithmeticOp::Divide => left / right,
            };
            finite_float(value, "float arithmetic")
        }
        _ => Err(Error::new(
            "E_TYPE",
            "arithmetic operands do not have the same numeric type",
        )),
    }
}

fn finite_float(value: f64, context: &str) -> Result<Value> {
    if !value.is_finite() {
        return Err(Error::new(
            "E_ARITH",
            format!("{context} produced a non-finite float"),
        ));
    }
    Ok(Value::Float(if value == 0.0 { 0.0 } else { value }))
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
