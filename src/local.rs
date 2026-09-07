use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::{Catalog, Column, MAX_DEPTH, ScalarType};
use crate::query::{BoolExpression, LocalBinding, MatchValue, MatchValuePayload, ScalarExpression};

pub(crate) const MAX_LOCAL_BINDINGS: usize = 256;
pub(crate) const MAX_LOCAL_EXPANSION_STEPS: usize = 100_000;
pub(crate) const MAX_LOCAL_CALL_DEPTH: usize = 32;

#[derive(Clone)]
struct Definition {
    name: String,
    result_type: Option<ScalarType>,
    parameters: Vec<(String, Option<ScalarType>)>,
    expression: BoolExpression,
    captured: BTreeMap<String, usize>,
}

pub(crate) struct LocalScope {
    definitions: Vec<Definition>,
    visible: BTreeMap<String, usize>,
    remaining_steps: usize,
}

impl LocalScope {
    pub(crate) fn new() -> Self {
        Self {
            definitions: Vec::new(),
            visible: BTreeMap::new(),
            remaining_steps: MAX_LOCAL_EXPANSION_STEPS,
        }
    }

    pub(crate) fn define(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        binding: &mut LocalBinding,
    ) -> Result<()> {
        if self.definitions.len() >= MAX_LOCAL_BINDINGS {
            return Err(Error::new(
                "E_LIMIT",
                format!("query has more than {MAX_LOCAL_BINDINGS} local bindings"),
            ));
        }
        let mut parameter_names = BTreeSet::new();
        if let Some(annotation) = &mut binding.annotation {
            *annotation = resolve_annotation(catalog, annotation.clone(), 0)?;
        }
        for parameter in &mut binding.parameters {
            if !parameter_names.insert(parameter.name.clone()) {
                return Err(Error::new(
                    "E_QUERY",
                    format!("duplicate local parameter '{}'", parameter.name),
                ));
            }
            if let Some(annotation) = &mut parameter.annotation {
                *annotation = resolve_annotation(catalog, annotation.clone(), 0)?;
            }
        }
        validate_definition(
            catalog,
            schema,
            &binding.name,
            &binding.expression,
            &parameter_names,
            &self.visible,
            &self.definitions,
            &BTreeSet::new(),
        )
        .map_err(|error| error.at(binding.span))?;

        let captured = self.visible.clone();
        if binding.parameters.is_empty() {
            let mut expression = binding.expression.clone();
            self.expand_bool_in(
                catalog,
                schema,
                &mut expression,
                &captured,
                &BTreeMap::new(),
                &BTreeSet::new(),
                0,
            )?;
            let inferred = if let Some(expected) = &binding.annotation {
                match &mut expression {
                    BoolExpression::Value(value) => crate::expression::bind_scalar(
                        catalog,
                        schema,
                        value,
                        Some(expected),
                        "local value",
                    )?,
                    _ => {
                        let inferred =
                            crate::expression::bind_derive(catalog, schema, &mut expression)?;
                        if !crate::expression::same_type(&inferred, expected) {
                            return Err(Error::new(
                                "E_TYPE",
                                format!(
                                    "local value '{}' has type {}, expected {}",
                                    binding.name,
                                    catalog.describe(&inferred),
                                    catalog.describe(expected)
                                ),
                            ));
                        }
                        inferred
                    }
                }
            } else {
                crate::expression::bind_derive(catalog, schema, &mut expression)?
            };
            let _ = inferred;
            binding.expression = expression;
        }

        let index = self.definitions.len();
        self.definitions.push(Definition {
            name: binding.name.clone(),
            result_type: binding.annotation.clone(),
            parameters: binding
                .parameters
                .iter()
                .map(|parameter| (parameter.name.clone(), parameter.annotation.clone()))
                .collect(),
            expression: binding.expression.clone(),
            captured,
        });
        self.visible.insert(binding.name.clone(), index);
        Ok(())
    }

    pub(crate) fn expand_bool(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        expression: &mut BoolExpression,
    ) -> Result<()> {
        let visible = self.visible.clone();
        self.expand_bool_in(
            catalog,
            schema,
            expression,
            &visible,
            &BTreeMap::new(),
            &BTreeSet::new(),
            0,
        )
    }

    pub(crate) fn expand_scalar(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        expression: &mut ScalarExpression,
    ) -> Result<()> {
        let visible = self.visible.clone();
        *expression = self.expand_scalar_in(
            catalog,
            schema,
            expression.clone(),
            &visible,
            &BTreeMap::new(),
            &BTreeSet::new(),
            0,
        )?;
        Ok(())
    }

    pub(crate) fn expand_match_value(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        value: &mut MatchValue,
    ) -> Result<()> {
        let visible = self.visible.clone();
        self.expand_match_value_in(
            catalog,
            schema,
            value,
            &visible,
            &BTreeMap::new(),
            &BTreeSet::new(),
            0,
        )
    }

    fn consume_step(&mut self) -> Result<()> {
        if self.remaining_steps == 0 {
            return Err(Error::new(
                "E_LIMIT",
                format!("local expression expansion exceeds {MAX_LOCAL_EXPANSION_STEPS} steps"),
            ));
        }
        self.remaining_steps -= 1;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_bool_in(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        expression: &mut BoolExpression,
        visible: &BTreeMap<String, usize>,
        substitutions: &BTreeMap<String, ScalarExpression>,
        shadowed: &BTreeSet<String>,
        call_depth: usize,
    ) -> Result<()> {
        self.consume_step()?;
        match expression {
            BoolExpression::Value(value) => {
                if let Some(expanded) = self.expand_local_value(
                    catalog,
                    schema,
                    value,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )? {
                    *expression = expanded;
                    self.expand_bool_in(
                        catalog,
                        schema,
                        expression,
                        visible,
                        substitutions,
                        shadowed,
                        call_depth,
                    )?;
                } else {
                    *value = self.expand_scalar_in(
                        catalog,
                        schema,
                        value.clone(),
                        visible,
                        substitutions,
                        shadowed,
                        call_depth,
                    )?;
                }
            }
            BoolExpression::Compare { left, right, .. }
            | BoolExpression::Contains {
                collection: left,
                item: right,
            } => {
                *left = self.expand_scalar_in(
                    catalog,
                    schema,
                    left.clone(),
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
                *right = self.expand_scalar_in(
                    catalog,
                    schema,
                    right.clone(),
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
            }
            BoolExpression::Any {
                collection,
                binding,
                predicate,
            }
            | BoolExpression::All {
                collection,
                binding,
                predicate,
            } => {
                *collection = self.expand_scalar_in(
                    catalog,
                    schema,
                    collection.clone(),
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
                let mut nested_shadowed = shadowed.clone();
                nested_shadowed.insert(binding.clone());
                self.expand_bool_in(
                    catalog,
                    schema,
                    predicate,
                    visible,
                    substitutions,
                    &nested_shadowed,
                    call_depth,
                )?;
            }
            BoolExpression::IsSome(value) | BoolExpression::IsNone(value) => {
                *value = self.expand_scalar_in(
                    catalog,
                    schema,
                    value.clone(),
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
            }
            BoolExpression::Not(value) => self.expand_bool_in(
                catalog,
                schema,
                value,
                visible,
                substitutions,
                shadowed,
                call_depth,
            )?,
            BoolExpression::And(left, right) | BoolExpression::Or(left, right) => {
                self.expand_bool_in(
                    catalog,
                    schema,
                    left,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
                self.expand_bool_in(
                    catalog,
                    schema,
                    right,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_local_value(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        expression: &ScalarExpression,
        visible: &BTreeMap<String, usize>,
        substitutions: &BTreeMap<String, ScalarExpression>,
        shadowed: &BTreeSet<String>,
        call_depth: usize,
    ) -> Result<Option<BoolExpression>> {
        match expression {
            ScalarExpression::Reference(path)
                if !path.contains('.') && !shadowed.contains(path) =>
            {
                if substitutions.contains_key(path) {
                    return Ok(None);
                }
                let Some(index) = visible.get(path).copied() else {
                    return Ok(None);
                };
                let definition = &self.definitions[index];
                if !definition.parameters.is_empty() {
                    return Ok(None);
                }
                self.expand_call(catalog, schema, index, Vec::new(), call_depth)
                    .map(Some)
            }
            ScalarExpression::Call {
                name,
                arguments,
                span,
            } if !shadowed.contains(name) => {
                let Some(index) = visible.get(name).copied() else {
                    return Ok(None);
                };
                let mut expanded_arguments = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    expanded_arguments.push(self.expand_scalar_in(
                        catalog,
                        schema,
                        argument.clone(),
                        visible,
                        substitutions,
                        shadowed,
                        call_depth,
                    )?);
                }
                self.expand_call(catalog, schema, index, expanded_arguments, call_depth)
                    .map(Some)
                    .map_err(|error| error.at(*span))
            }
            _ => Ok(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_scalar_in(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        expression: ScalarExpression,
        visible: &BTreeMap<String, usize>,
        substitutions: &BTreeMap<String, ScalarExpression>,
        shadowed: &BTreeSet<String>,
        call_depth: usize,
    ) -> Result<ScalarExpression> {
        self.consume_step()?;
        match expression {
            ScalarExpression::Reference(path) => {
                if let Some(value) = substitute_reference(&path, substitutions, shadowed)? {
                    return Ok(value);
                }
                let Some((name, suffix)) = split_reference(&path) else {
                    return Ok(ScalarExpression::Reference(path));
                };
                if shadowed.contains(name) {
                    return Ok(ScalarExpression::Reference(path));
                }
                let Some(index) = visible.get(name).copied() else {
                    return Ok(ScalarExpression::Reference(path));
                };
                let definition = &self.definitions[index];
                if !definition.parameters.is_empty() {
                    return Err(Error::new(
                        "E_QUERY",
                        format!("local function '{name}' requires arguments"),
                    ));
                }
                let expanded = self.expand_call(catalog, schema, index, Vec::new(), call_depth)?;
                bool_to_scalar(expanded, suffix)
            }
            ScalarExpression::Call {
                name,
                arguments,
                span,
            } => {
                if shadowed.contains(&name) {
                    return Err(Error::new(
                        "E_QUERY",
                        format!("local value '{name}' cannot be called as a function"),
                    ));
                }
                let index = visible.get(&name).copied().ok_or_else(|| {
                    Error::new("E_QUERY", format!("unknown local function '{name}'")).at(span)
                })?;
                let mut expanded_arguments = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    expanded_arguments.push(self.expand_scalar_in(
                        catalog,
                        schema,
                        argument,
                        visible,
                        substitutions,
                        shadowed,
                        call_depth,
                    )?);
                }
                let expanded = self
                    .expand_call(catalog, schema, index, expanded_arguments, call_depth)
                    .map_err(|error| error.at(span))?;
                bool_to_scalar(expanded, None)
            }
            ScalarExpression::Length(value) => {
                Ok(ScalarExpression::Length(Box::new(self.expand_scalar_in(
                    catalog,
                    schema,
                    *value,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?)))
            }
            ScalarExpression::Ascribed { value, ty } => Ok(ScalarExpression::Ascribed {
                value: Box::new(self.expand_scalar_in(
                    catalog,
                    schema,
                    *value,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?),
                ty,
            }),
            ScalarExpression::Negate { value, ty } => Ok(ScalarExpression::Negate {
                value: Box::new(self.expand_scalar_in(
                    catalog,
                    schema,
                    *value,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?),
                ty,
            }),
            ScalarExpression::Arithmetic {
                left,
                op,
                right,
                ty,
            } => Ok(ScalarExpression::Arithmetic {
                left: Box::new(self.expand_scalar_in(
                    catalog,
                    schema,
                    *left,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?),
                op,
                right: Box::new(self.expand_scalar_in(
                    catalog,
                    schema,
                    *right,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?),
                ty,
            }),
            ScalarExpression::Parameter { .. } | ScalarExpression::Literal(_) => Ok(expression),
        }
    }

    fn expand_call(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        index: usize,
        mut arguments: Vec<ScalarExpression>,
        call_depth: usize,
    ) -> Result<BoolExpression> {
        if call_depth >= MAX_LOCAL_CALL_DEPTH {
            return Err(Error::new(
                "E_LIMIT",
                format!("local function call depth exceeds {MAX_LOCAL_CALL_DEPTH}"),
            ));
        }
        self.consume_step()?;
        let definition = self.definitions[index].clone();
        if arguments.len() != definition.parameters.len() {
            return Err(Error::new(
                "E_QUERY",
                format!(
                    "local function '{}' expects {} argument(s), got {}",
                    definition.name,
                    definition.parameters.len(),
                    arguments.len()
                ),
            ));
        }
        for (position, ((_, inferred), argument)) in definition
            .parameters
            .iter()
            .zip(arguments.iter_mut())
            .enumerate()
        {
            let expected = self.definitions[index].parameters[position].1.clone();
            let ty = if let Some(expected) = expected {
                match crate::expression::bind_scalar(
                    catalog,
                    schema,
                    argument,
                    Some(&expected),
                    "local function argument",
                ) {
                    Ok(_) => {}
                    Err(error) if error.code == "E_FIELD" => {}
                    Err(error) => return Err(error),
                }
                Some(expected)
            } else {
                match crate::expression::infer_scalar(
                    catalog,
                    schema,
                    argument,
                    "local function argument",
                ) {
                    Ok(Some(ty)) => Some(ty),
                    Err(error) if error.code == "E_FIELD" => None,
                    Ok(None) => {
                        return Err(Error::new(
                            "E_TYPE",
                            format!(
                                "cannot infer argument {} of local function '{}'; add a parameter type",
                                position + 1,
                                definition.name
                            ),
                        ));
                    }
                    Err(error) => return Err(error),
                }
            };
            if let Some(ty) = ty {
                *argument = ScalarExpression::Ascribed {
                    value: Box::new(argument.clone()),
                    ty: ty.clone(),
                };
                if inferred.is_none() {
                    self.definitions[index].parameters[position].1 = Some(ty);
                }
            }
        }
        let substitutions = definition
            .parameters
            .iter()
            .map(|(name, _)| name.clone())
            .zip(arguments)
            .collect::<BTreeMap<_, _>>();
        let mut expression = definition.expression;
        self.expand_bool_in(
            catalog,
            schema,
            &mut expression,
            &definition.captured,
            &substitutions,
            &BTreeSet::new(),
            call_depth + 1,
        )?;
        if let Some(expected) = definition.result_type {
            expression = ascribe_result(expression, expected)?;
        }
        Ok(expression)
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_match_value_in(
        &mut self,
        catalog: &Catalog,
        schema: &[Column],
        value: &mut MatchValue,
        visible: &BTreeMap<String, usize>,
        substitutions: &BTreeMap<String, ScalarExpression>,
        shadowed: &BTreeSet<String>,
        call_depth: usize,
    ) -> Result<()> {
        match value {
            MatchValue::Expression(expression) => {
                self.expand_bool_in(
                    catalog,
                    schema,
                    expression,
                    visible,
                    substitutions,
                    shadowed,
                    call_depth,
                )?;
            }
            MatchValue::Constructor { payload, .. } => match payload {
                MatchValuePayload::Unit => {}
                MatchValuePayload::Record(fields) => {
                    for field in fields {
                        self.expand_match_value_in(
                            catalog,
                            schema,
                            &mut field.value,
                            visible,
                            substitutions,
                            shadowed,
                            call_depth,
                        )?;
                    }
                }
                MatchValuePayload::Positional(values) => {
                    for value in values {
                        self.expand_match_value_in(
                            catalog,
                            schema,
                            value,
                            visible,
                            substitutions,
                            shadowed,
                            call_depth,
                        )?;
                    }
                }
            },
            MatchValue::Record(fields) => {
                for field in fields {
                    self.expand_match_value_in(
                        catalog,
                        schema,
                        &mut field.value,
                        visible,
                        substitutions,
                        shadowed,
                        call_depth,
                    )?;
                }
            }
            MatchValue::Tuple(values) | MatchValue::List(values) => {
                for value in values {
                    self.expand_match_value_in(
                        catalog,
                        schema,
                        value,
                        visible,
                        substitutions,
                        shadowed,
                        call_depth,
                    )?;
                }
            }
            MatchValue::Binding(_) | MatchValue::Literal(_) => {}
        }
        Ok(())
    }
}

fn split_reference(path: &str) -> Option<(&str, Option<&str>)> {
    if path.is_empty() {
        None
    } else if let Some((head, tail)) = path.split_once('.') {
        Some((head, Some(tail)))
    } else {
        Some((path, None))
    }
}

fn substitute_reference(
    path: &str,
    substitutions: &BTreeMap<String, ScalarExpression>,
    shadowed: &BTreeSet<String>,
) -> Result<Option<ScalarExpression>> {
    let Some((head, suffix)) = split_reference(path) else {
        return Ok(None);
    };
    if shadowed.contains(head) {
        return Ok(None);
    }
    let Some(value) = substitutions.get(head) else {
        return Ok(None);
    };
    if let Some(suffix) = suffix {
        let reference = match value {
            ScalarExpression::Reference(base) => Some(base),
            ScalarExpression::Ascribed { value, .. } => match value.as_ref() {
                ScalarExpression::Reference(base) => Some(base),
                _ => None,
            },
            _ => None,
        };
        let Some(base) = reference else {
            return Err(Error::new(
                "E_QUERY",
                format!(
                    "parameter '{head}' is projected as '{path}' but its argument is not a field path"
                ),
            ));
        };
        Ok(Some(ScalarExpression::Reference(format!(
            "{base}.{suffix}"
        ))))
    } else {
        Ok(Some(value.clone()))
    }
}

fn bool_to_scalar(expression: BoolExpression, suffix: Option<&str>) -> Result<ScalarExpression> {
    let BoolExpression::Value(value) = expression else {
        return Err(Error::new(
            "E_TYPE",
            "boolean local expression cannot be used where a scalar value is required",
        ));
    };
    if let Some(suffix) = suffix {
        let base = match value {
            ScalarExpression::Reference(base) => Some(base),
            ScalarExpression::Ascribed { value, .. } => match *value {
                ScalarExpression::Reference(base) => Some(base),
                _ => None,
            },
            _ => None,
        };
        let Some(base) = base else {
            return Err(Error::new(
                "E_QUERY",
                "a local value can be projected only when it aliases a field path",
            ));
        };
        Ok(ScalarExpression::Reference(format!("{base}.{suffix}")))
    } else {
        Ok(value)
    }
}

fn ascribe_result(expression: BoolExpression, ty: ScalarType) -> Result<BoolExpression> {
    match expression {
        BoolExpression::Value(value) => Ok(BoolExpression::Value(ScalarExpression::Ascribed {
            value: Box::new(value),
            ty,
        })),
        boolean if matches!(ty, ScalarType::Bool) => Ok(boolean),
        _ => Err(Error::new(
            "E_TYPE",
            "a boolean local function must declare bool as its result type",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_definition(
    catalog: &Catalog,
    schema: &[Column],
    binding_name: &str,
    expression: &BoolExpression,
    parameters: &BTreeSet<String>,
    visible: &BTreeMap<String, usize>,
    definitions: &[Definition],
    shadowed: &BTreeSet<String>,
) -> Result<()> {
    let scalar = |value: &ScalarExpression, nested_shadowed: &BTreeSet<String>| {
        validate_scalar(
            catalog,
            schema,
            binding_name,
            value,
            parameters,
            visible,
            definitions,
            nested_shadowed,
        )
    };
    match expression {
        BoolExpression::Value(value)
        | BoolExpression::IsSome(value)
        | BoolExpression::IsNone(value) => scalar(value, shadowed),
        BoolExpression::Compare { left, right, .. }
        | BoolExpression::Contains {
            collection: left,
            item: right,
        } => {
            scalar(left, shadowed)?;
            scalar(right, shadowed)
        }
        BoolExpression::Any {
            collection,
            binding,
            predicate,
        }
        | BoolExpression::All {
            collection,
            binding,
            predicate,
        } => {
            scalar(collection, shadowed)?;
            let mut nested = shadowed.clone();
            nested.insert(binding.clone());
            validate_definition(
                catalog,
                schema,
                binding_name,
                predicate,
                parameters,
                visible,
                definitions,
                &nested,
            )
        }
        BoolExpression::Not(value) => validate_definition(
            catalog,
            schema,
            binding_name,
            value,
            parameters,
            visible,
            definitions,
            shadowed,
        ),
        BoolExpression::And(left, right) | BoolExpression::Or(left, right) => {
            validate_definition(
                catalog,
                schema,
                binding_name,
                left,
                parameters,
                visible,
                definitions,
                shadowed,
            )?;
            validate_definition(
                catalog,
                schema,
                binding_name,
                right,
                parameters,
                visible,
                definitions,
                shadowed,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_scalar(
    catalog: &Catalog,
    schema: &[Column],
    binding_name: &str,
    expression: &ScalarExpression,
    parameters: &BTreeSet<String>,
    visible: &BTreeMap<String, usize>,
    definitions: &[Definition],
    shadowed: &BTreeSet<String>,
) -> Result<()> {
    match expression {
        ScalarExpression::Reference(path) => {
            let (head, _) = split_reference(path).expect("references are non-empty");
            if shadowed.contains(head) || parameters.contains(head) || visible.contains_key(head) {
                return Ok(());
            }
            if head == binding_name {
                return Err(Error::new(
                    "E_QUERY",
                    format!("local binding '{binding_name}' cannot reference itself"),
                ));
            }
            catalog.field_type(schema, path).map(|_| ())
        }
        ScalarExpression::Call {
            name,
            arguments,
            span,
        } => {
            if shadowed.contains(name) || parameters.contains(name) {
                return Err(Error::new(
                    "E_QUERY",
                    format!("function values are not supported; '{name}' is a local value"),
                ));
            }
            let Some(index) = visible.get(name).copied() else {
                let message = if name == binding_name {
                    format!("local function '{name}' cannot call itself")
                } else {
                    format!(
                        "unknown local function '{name}'; local functions may call only earlier definitions"
                    )
                };
                return Err(Error::new("E_QUERY", message).at(*span));
            };
            let expected = definitions[index].parameters.len();
            if arguments.len() != expected {
                return Err(Error::new(
                    "E_QUERY",
                    format!(
                        "local function '{name}' expects {expected} argument(s), got {}",
                        arguments.len()
                    ),
                )
                .at(*span));
            }
            for argument in arguments {
                validate_scalar(
                    catalog,
                    schema,
                    binding_name,
                    argument,
                    parameters,
                    visible,
                    definitions,
                    shadowed,
                )?;
            }
            Ok(())
        }
        ScalarExpression::Ascribed { value, .. }
        | ScalarExpression::Length(value)
        | ScalarExpression::Negate { value, .. } => validate_scalar(
            catalog,
            schema,
            binding_name,
            value,
            parameters,
            visible,
            definitions,
            shadowed,
        ),
        ScalarExpression::Arithmetic { left, right, .. } => {
            validate_scalar(
                catalog,
                schema,
                binding_name,
                left,
                parameters,
                visible,
                definitions,
                shadowed,
            )?;
            validate_scalar(
                catalog,
                schema,
                binding_name,
                right,
                parameters,
                visible,
                definitions,
                shadowed,
            )
        }
        ScalarExpression::Parameter { .. } | ScalarExpression::Literal(_) => Ok(()),
    }
}

fn resolve_annotation(catalog: &Catalog, ty: ScalarType, depth: usize) -> Result<ScalarType> {
    if depth >= MAX_DEPTH {
        return Err(Error::new("E_LIMIT", "local type annotation is too deep"));
    }
    Ok(match ty {
        ScalarType::Named(name) => ScalarType::Ref(
            catalog
                .types
                .get(&name)
                .ok_or_else(|| Error::new("E_TYPE", format!("unknown local type '{name}'")))?
                .id,
        ),
        ScalarType::Option(inner) => {
            ScalarType::Option(Box::new(resolve_annotation(catalog, *inner, depth + 1)?))
        }
        ScalarType::List(inner) => {
            ScalarType::List(Box::new(resolve_annotation(catalog, *inner, depth + 1)?))
        }
        ScalarType::Tuple(items) => ScalarType::Tuple(
            items
                .into_iter()
                .map(|item| resolve_annotation(catalog, item, depth + 1))
                .collect::<Result<_>>()?,
        ),
        ScalarType::Record(_) | ScalarType::Enum(_) => {
            return Err(Error::new(
                "E_TYPE",
                "local parameter annotations use primitive, named, option, list, or tuple types",
            ));
        }
        primitive => primitive,
    })
}
