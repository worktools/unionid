use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::Value;
use crate::query::{
    BoolExpression, LocatedStatement, MatchValue, MatchValuePayload, Pipeline, ScalarExpression,
    SchemaMigration, SetValue, Stage, Statement,
};

pub(crate) fn names(statements: &[LocatedStatement]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for located in statements {
        visit_statement(&located.statement, &mut |expression| {
            if let ScalarExpression::Parameter { name, .. } = expression {
                names.insert(name.clone());
            }
        });
    }
    names
}

pub(crate) fn types(
    statements: &[LocatedStatement],
) -> Result<BTreeMap<String, crate::model::ScalarType>> {
    let mut types = BTreeMap::new();
    let mut conflict = None;
    for located in statements {
        visit_statement(&located.statement, &mut |expression| {
            let ScalarExpression::Parameter { name, ty: Some(ty) } = expression else {
                return;
            };
            if let Some(existing) = types.get(name) {
                if !crate::expression::same_type(existing, ty) {
                    conflict.get_or_insert_with(|| name.clone());
                }
            } else {
                types.insert(name.clone(), ty.clone());
            }
        });
    }
    if let Some(name) = conflict {
        Err(Error::new(
            "E_TYPE",
            format!("parameter '${name}' is used with incompatible types"),
        ))
    } else {
        Ok(types)
    }
}

pub(crate) fn bind(
    statements: &mut [LocatedStatement],
    parameters: &BTreeMap<String, Value>,
) -> Result<()> {
    let expected = names(statements);
    let missing = expected
        .difference(&parameters.keys().cloned().collect())
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(Error::new(
            "E_PARAM_MISSING",
            format!("missing parameter(s): {}", display_names(&missing)),
        ));
    }
    let extra = parameters
        .keys()
        .filter(|name| !expected.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !extra.is_empty() {
        return Err(Error::new(
            "E_PARAM_EXTRA",
            format!("unexpected parameter(s): {}", display_names(&extra)),
        ));
    }
    for located in statements {
        visit_statement_mut(&mut located.statement, &mut |expression| {
            if let ScalarExpression::Parameter { name, .. } = expression {
                *expression = ScalarExpression::Literal(
                    parameters
                        .get(name)
                        .expect("parameter set was validated")
                        .clone(),
                );
            }
        });
    }
    Ok(())
}

fn display_names(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("'${name}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn visit_statement(statement: &Statement, visitor: &mut impl FnMut(&ScalarExpression)) {
    match statement {
        Statement::Update {
            target,
            assignments,
            ..
        } => {
            visit_pipeline(target, visitor);
            for assignment in assignments {
                match &assignment.value {
                    SetValue::Expression(value) => visit_scalar(value, visitor),
                    SetValue::Match(value) => {
                        for arm in &value.arms {
                            visit_match_value(&arm.result, visitor);
                        }
                    }
                }
            }
        }
        Statement::Delete { target, .. }
        | Statement::Explain(target)
        | Statement::Pipeline(target) => visit_pipeline(target, visitor),
        Statement::Migration { steps, .. } => {
            for step in steps {
                visit_migration(step, visitor);
            }
        }
        Statement::InsertParameter {
            parameter,
            parameter_type,
            ..
        }
        | Statement::UpsertParameter {
            parameter,
            parameter_type,
            ..
        } => {
            visitor(&ScalarExpression::Parameter {
                name: parameter.clone(),
                ty: parameter_type.clone(),
            });
        }
        Statement::InsertManyParameter {
            parameter,
            parameter_type,
            ..
        } => {
            visitor(&ScalarExpression::Parameter {
                name: parameter.clone(),
                ty: parameter_type.clone(),
            });
        }
        Statement::DefineType { .. }
        | Statement::CreateTable { .. }
        | Statement::TypedTable { .. }
        | Statement::CreateIndex { .. }
        | Statement::Insert { .. }
        | Statement::InsertMany { .. }
        | Statement::Upsert { .. } => {}
    }
}

fn visit_statement_mut(statement: &mut Statement, visitor: &mut impl FnMut(&mut ScalarExpression)) {
    match statement {
        Statement::Update {
            target,
            assignments,
            ..
        } => {
            visit_pipeline_mut(target, visitor);
            for assignment in assignments {
                match &mut assignment.value {
                    SetValue::Expression(value) => visit_scalar_mut(value, visitor),
                    SetValue::Match(value) => {
                        for arm in &mut value.arms {
                            visit_match_value_mut(&mut arm.result, visitor);
                        }
                    }
                }
            }
        }
        Statement::Delete { target, .. }
        | Statement::Explain(target)
        | Statement::Pipeline(target) => visit_pipeline_mut(target, visitor),
        Statement::Migration { steps, .. } => {
            for step in steps {
                visit_migration_mut(step, visitor);
            }
        }
        Statement::InsertParameter {
            table,
            parameter,
            returning,
            ..
        } => {
            let table = std::mem::take(table);
            let parameter = std::mem::take(parameter);
            let returning = returning.take();
            *statement = Statement::Insert {
                table,
                values: parameters_value(visitor, &parameter),
                returning,
            };
        }
        Statement::UpsertParameter {
            table,
            parameter,
            returning,
            ..
        } => {
            let table = std::mem::take(table);
            let parameter = std::mem::take(parameter);
            let returning = returning.take();
            *statement = Statement::Upsert {
                table,
                values: parameters_value(visitor, &parameter),
                returning,
            };
        }
        Statement::InsertManyParameter {
            table,
            parameter,
            returning,
            ..
        } => {
            let table = std::mem::take(table);
            let parameter = std::mem::take(parameter);
            let returning = returning.take();
            *statement = Statement::InsertMany {
                table,
                values: parameters_value(visitor, &parameter),
                returning,
            };
        }
        Statement::DefineType { .. }
        | Statement::CreateTable { .. }
        | Statement::TypedTable { .. }
        | Statement::CreateIndex { .. }
        | Statement::Insert { .. }
        | Statement::InsertMany { .. }
        | Statement::Upsert { .. } => {}
    }
}

fn parameters_value(visitor: &mut impl FnMut(&mut ScalarExpression), parameter: &str) -> Value {
    let mut expression = ScalarExpression::Parameter {
        name: parameter.into(),
        ty: None,
    };
    visitor(&mut expression);
    let ScalarExpression::Literal(value) = expression else {
        unreachable!("validated parameter visitor must replace row parameters")
    };
    value
}

fn visit_pipeline(pipeline: &Pipeline, visitor: &mut impl FnMut(&ScalarExpression)) {
    for stage in &pipeline.stages {
        match stage {
            Stage::Let(binding) => visit_bool(&binding.expression, visitor),
            Stage::Filter(expression) => visit_bool(expression, visitor),
            Stage::FilterMatch(predicate) => {
                for arm in &predicate.arms {
                    visit_bool(&arm.condition, visitor);
                }
            }
            Stage::Derive(derive) => visit_bool(&derive.expression, visitor),
            Stage::DeriveMatch(derive) => {
                for arm in &derive.arms {
                    visit_match_value(&arm.result, visitor);
                }
            }
            Stage::Aggregate(aggregate) => {
                for assignment in &aggregate.assignments {
                    if let Some(input) = &assignment.input {
                        visit_scalar(input, visitor);
                    }
                }
            }
            Stage::Select(_) | Stage::Sort(_) | Stage::Take { .. } => {}
        }
    }
}

fn visit_pipeline_mut(pipeline: &mut Pipeline, visitor: &mut impl FnMut(&mut ScalarExpression)) {
    for stage in &mut pipeline.stages {
        match stage {
            Stage::Let(binding) => visit_bool_mut(&mut binding.expression, visitor),
            Stage::Filter(expression) => visit_bool_mut(expression, visitor),
            Stage::FilterMatch(predicate) => {
                for arm in &mut predicate.arms {
                    visit_bool_mut(&mut arm.condition, visitor);
                }
            }
            Stage::Derive(derive) => visit_bool_mut(&mut derive.expression, visitor),
            Stage::DeriveMatch(derive) => {
                for arm in &mut derive.arms {
                    visit_match_value_mut(&mut arm.result, visitor);
                }
            }
            Stage::Aggregate(aggregate) => {
                for assignment in &mut aggregate.assignments {
                    if let Some(input) = &mut assignment.input {
                        visit_scalar_mut(input, visitor);
                    }
                }
            }
            Stage::Select(_) | Stage::Sort(_) | Stage::Take { .. } => {}
        }
    }
}

fn visit_bool(expression: &BoolExpression, visitor: &mut impl FnMut(&ScalarExpression)) {
    match expression {
        BoolExpression::Value(value) => visit_scalar(value, visitor),
        BoolExpression::Compare { left, right, .. }
        | BoolExpression::Contains {
            collection: left,
            item: right,
        } => {
            visit_scalar(left, visitor);
            visit_scalar(right, visitor);
        }
        BoolExpression::Any {
            collection,
            predicate,
            ..
        }
        | BoolExpression::All {
            collection,
            predicate,
            ..
        } => {
            visit_scalar(collection, visitor);
            visit_bool(predicate, visitor);
        }
        BoolExpression::IsSome(value) | BoolExpression::IsNone(value) => {
            visit_scalar(value, visitor)
        }
        BoolExpression::Not(value) => visit_bool(value, visitor),
        BoolExpression::And(left, right) | BoolExpression::Or(left, right) => {
            visit_bool(left, visitor);
            visit_bool(right, visitor);
        }
    }
}

fn visit_bool_mut(
    expression: &mut BoolExpression,
    visitor: &mut impl FnMut(&mut ScalarExpression),
) {
    match expression {
        BoolExpression::Value(value) => visit_scalar_mut(value, visitor),
        BoolExpression::Compare { left, right, .. }
        | BoolExpression::Contains {
            collection: left,
            item: right,
        } => {
            visit_scalar_mut(left, visitor);
            visit_scalar_mut(right, visitor);
        }
        BoolExpression::Any {
            collection,
            predicate,
            ..
        }
        | BoolExpression::All {
            collection,
            predicate,
            ..
        } => {
            visit_scalar_mut(collection, visitor);
            visit_bool_mut(predicate, visitor);
        }
        BoolExpression::IsSome(value) | BoolExpression::IsNone(value) => {
            visit_scalar_mut(value, visitor)
        }
        BoolExpression::Not(value) => visit_bool_mut(value, visitor),
        BoolExpression::And(left, right) | BoolExpression::Or(left, right) => {
            visit_bool_mut(left, visitor);
            visit_bool_mut(right, visitor);
        }
    }
}

fn visit_scalar(expression: &ScalarExpression, visitor: &mut impl FnMut(&ScalarExpression)) {
    visitor(expression);
    match expression {
        ScalarExpression::Ascribed { value, .. }
        | ScalarExpression::Length(value)
        | ScalarExpression::Negate { value, .. } => visit_scalar(value, visitor),
        ScalarExpression::Arithmetic { left, right, .. } => {
            visit_scalar(left, visitor);
            visit_scalar(right, visitor);
        }
        ScalarExpression::Call { arguments, .. } => {
            for argument in arguments {
                visit_scalar(argument, visitor);
            }
        }
        ScalarExpression::Reference(_)
        | ScalarExpression::Parameter { .. }
        | ScalarExpression::Literal(_) => {}
    }
}

fn visit_scalar_mut(
    expression: &mut ScalarExpression,
    visitor: &mut impl FnMut(&mut ScalarExpression),
) {
    visitor(expression);
    match expression {
        ScalarExpression::Ascribed { value, .. }
        | ScalarExpression::Length(value)
        | ScalarExpression::Negate { value, .. } => visit_scalar_mut(value, visitor),
        ScalarExpression::Arithmetic { left, right, .. } => {
            visit_scalar_mut(left, visitor);
            visit_scalar_mut(right, visitor);
        }
        ScalarExpression::Call { arguments, .. } => {
            for argument in arguments {
                visit_scalar_mut(argument, visitor);
            }
        }
        ScalarExpression::Reference(_)
        | ScalarExpression::Parameter { .. }
        | ScalarExpression::Literal(_) => {}
    }
}

fn visit_match_value(value: &MatchValue, visitor: &mut impl FnMut(&ScalarExpression)) {
    match value {
        MatchValue::Expression(expression) => visit_scalar(expression, visitor),
        MatchValue::Constructor { payload, .. } => match payload {
            MatchValuePayload::Unit => {}
            MatchValuePayload::Record(fields) => {
                for field in fields {
                    visit_match_value(&field.value, visitor);
                }
            }
            MatchValuePayload::Positional(values) => {
                for value in values {
                    visit_match_value(value, visitor);
                }
            }
        },
        MatchValue::Record(fields) => {
            for field in fields {
                visit_match_value(&field.value, visitor);
            }
        }
        MatchValue::Tuple(values) | MatchValue::List(values) => {
            for value in values {
                visit_match_value(value, visitor);
            }
        }
        MatchValue::Binding(_) | MatchValue::Literal(_) => {}
    }
}

fn visit_match_value_mut(value: &mut MatchValue, visitor: &mut impl FnMut(&mut ScalarExpression)) {
    match value {
        MatchValue::Expression(expression) => visit_scalar_mut(expression, visitor),
        MatchValue::Constructor { payload, .. } => match payload {
            MatchValuePayload::Unit => {}
            MatchValuePayload::Record(fields) => {
                for field in fields {
                    visit_match_value_mut(&mut field.value, visitor);
                }
            }
            MatchValuePayload::Positional(values) => {
                for value in values {
                    visit_match_value_mut(value, visitor);
                }
            }
        },
        MatchValue::Record(fields) => {
            for field in fields {
                visit_match_value_mut(&mut field.value, visitor);
            }
        }
        MatchValue::Tuple(values) | MatchValue::List(values) => {
            for value in values {
                visit_match_value_mut(value, visitor);
            }
        }
        MatchValue::Binding(_) | MatchValue::Literal(_) => {}
    }
}

fn visit_migration(step: &SchemaMigration, visitor: &mut impl FnMut(&ScalarExpression)) {
    match step {
        SchemaMigration::ChangeField { transform, .. }
        | SchemaMigration::ChangeVariant { transform, .. } => {
            visit_match_value(&transform.value, visitor)
        }
        SchemaMigration::DropVariant {
            transform: Some(transform),
            ..
        } => visit_match_value(&transform.value, visitor),
        _ => {}
    }
}

fn visit_migration_mut(
    step: &mut SchemaMigration,
    visitor: &mut impl FnMut(&mut ScalarExpression),
) {
    match step {
        SchemaMigration::ChangeField { transform, .. }
        | SchemaMigration::ChangeVariant { transform, .. } => {
            visit_match_value_mut(&mut transform.value, visitor)
        }
        SchemaMigration::DropVariant {
            transform: Some(transform),
            ..
        } => visit_match_value_mut(&mut transform.value, visitor),
        _ => {}
    }
}
