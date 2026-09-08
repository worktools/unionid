use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::{Catalog, Column, EnumType, ScalarType, Value};
use crate::query::{
    DeriveMatch, MatchPattern, MatchPayload, MatchPredicate, MatchTag, MatchValue, MatchValueField,
    MatchValuePayload,
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
        crate::expression::bind_match(catalog, bindings, &mut arm.condition)?;
    }
    Ok(())
}

/// Bind a match expression and return the column it appends to the pipeline.
pub(crate) fn bind_derive(
    catalog: &Catalog,
    schema: &[Column],
    derive: &mut DeriveMatch,
) -> Result<Column> {
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

    let context = format!("derive '{}'", derive.name);
    for (arm, bindings) in derive.arms.iter_mut().zip(&bindings) {
        bind_result(catalog, &context, &output_type, &mut arm.result, bindings)?;
    }
    derive.output_type = Some(output_type.clone());
    Ok(Column {
        name: derive.name.clone(),
        ty: output_type,
        default: None,
        id: 0,
    })
}

/// Bind a match used by an update assignment. The target field supplies the
/// output type, so unlike derive this does not need result inference.
pub(crate) fn bind_assignment(
    catalog: &Catalog,
    schema: &[Column],
    expected: &ScalarType,
    value: &mut DeriveMatch,
) -> Result<()> {
    let source_ty = catalog.field_type(schema, &value.source)?;
    let arm_count = value.arms.len();
    let bindings = bind_patterns(
        catalog,
        &value.source,
        source_ty,
        value.arms.iter_mut().map(|arm| &mut arm.pattern),
        arm_count,
    )?;
    let context = format!("update field '{}'", value.name);
    for (arm, bindings) in value.arms.iter_mut().zip(&bindings) {
        bind_result(catalog, &context, expected, &mut arm.result, bindings)?;
    }
    value.output_type = Some(expected.clone());
    Ok(())
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
    let source = match_source(catalog, source_ty, source_name)?;
    let mut matrix: Vec<Vec<CoveragePattern>> = Vec::with_capacity(arm_count);
    let mut budget = CoverageBudget::default();
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
                Vec::new()
            }
            MatchPattern::Binding(name) => {
                if index + 1 != arm_count {
                    return Err(Error::new(
                        "E_MATCH",
                        "top-level binding match branch must be last; later branches are unreachable",
                    ));
                }
                vec![Column {
                    name: name.clone(),
                    ty: source_ty.clone(),
                    default: None,
                    id: 0,
                }]
            }
            MatchPattern::Constructor { name, payload, tag } => {
                let (resolved_tag, argument_types, display_name) =
                    resolve_constructor(catalog, &source, source_name, name)?;
                *tag = Some(resolved_tag);
                let info = bind_payload(catalog, &display_name, &argument_types, payload)?;
                info.bindings
            }
            MatchPattern::Record { .. } | MatchPattern::Tuple(_) => {
                return Err(Error::new(
                    "E_MATCH",
                    "top-level match branch must name a constructor, binding, or '_'",
                ));
            }
        };
        let coverage = coverage_pattern(catalog, source_ty, pattern, source_name)?;
        if !pattern_is_useful(
            catalog,
            &matrix,
            std::slice::from_ref(&coverage),
            std::slice::from_ref(source_ty),
            &mut budget,
        )? {
            let duplicate = matrix
                .iter()
                .any(|row| row.as_slice() == std::slice::from_ref(&coverage));
            let message = if duplicate {
                coverage.root_name().map_or_else(
                    || format!("match branch {} is matched more than once", index + 1),
                    |name| {
                        format!(
                            "constructor '{name}' is matched more than once with the same pattern"
                        )
                    },
                )
            } else {
                format!(
                    "match branch {} is unreachable; its pattern is already covered by earlier branches",
                    index + 1
                )
            };
            return Err(Error::new("E_MATCH", message));
        }
        matrix.push(vec![coverage]);
        all_bindings.push(bindings);
    }
    if let Some(witness) = uncovered_patterns(
        catalog,
        &matrix,
        std::slice::from_ref(source_ty),
        &mut budget,
    )? {
        let missing = missing_top_level_constructors(catalog, source_ty, &matrix, &mut budget)?;
        let witness = witness
            .first()
            .map(CoveragePattern::display)
            .unwrap_or_else(|| "_".into());
        return Err(Error::new(
            "E_MATCH",
            format!(
                "non-exhaustive match; missing {}; uncovered example {witness}",
                missing.join(", ")
            ),
        ));
    }
    Ok(all_bindings)
}

fn match_source(catalog: &Catalog, ty: &ScalarType, name: &str) -> Result<MatchSource> {
    match catalog.underlying(ty)? {
        ScalarType::Enum(enum_type) => Ok(MatchSource::Enum(enum_type.clone())),
        ScalarType::Option(inner) => Ok(MatchSource::Option(inner.as_ref().clone())),
        _ => Err(Error::new(
            "E_TYPE",
            format!("match source '{name}' must be a sum type or option"),
        )),
    }
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
    payload: &mut MatchPayload,
) -> Result<PatternInfo> {
    match payload {
        MatchPayload::Unit => {
            if argument_types.is_empty() {
                Ok(PatternInfo::irrefutable())
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
            bind_record_fields(
                catalog,
                payload_fields,
                fields,
                *rest,
                &format!("constructor '{constructor}'"),
                true,
            )
        }
        MatchPayload::Positional(patterns) => {
            if patterns.len() != argument_types.len() {
                return Err(Error::new(
                    "E_MATCH",
                    format!(
                        "constructor '{constructor}' expects {} payload binding(s), got {}",
                        argument_types.len(),
                        patterns.len()
                    ),
                ));
            }
            let mut infos = Vec::with_capacity(patterns.len());
            for (index, (pattern, ty)) in patterns.iter_mut().zip(argument_types).enumerate() {
                infos.push(bind_nested_pattern(
                    catalog,
                    ty,
                    pattern,
                    &format!("argument {} of constructor '{constructor}'", index + 1),
                )?);
            }
            combine_patterns(infos)
        }
    }
}

struct PatternInfo {
    bindings: Vec<Column>,
}

impl PatternInfo {
    fn irrefutable() -> Self {
        Self {
            bindings: Vec::new(),
        }
    }
}

fn bind_nested_pattern(
    catalog: &Catalog,
    expected: &ScalarType,
    pattern: &mut MatchPattern,
    context: &str,
) -> Result<PatternInfo> {
    match pattern {
        MatchPattern::Wildcard => Ok(PatternInfo::irrefutable()),
        MatchPattern::Binding(name) => Ok(PatternInfo {
            bindings: vec![Column {
                name: name.clone(),
                ty: expected.clone(),
                default: None,
                id: 0,
            }],
        }),
        MatchPattern::Constructor { name, payload, tag } => {
            let source = match_source(catalog, expected, context)?;
            let (resolved_tag, argument_types, display_name) =
                resolve_constructor(catalog, &source, context, name)?;
            *tag = Some(resolved_tag);
            bind_payload(catalog, &display_name, &argument_types, payload)
        }
        MatchPattern::Record { fields, rest } => {
            let ScalarType::Record(definitions) = catalog.underlying(expected)? else {
                return Err(Error::new(
                    "E_MATCH",
                    format!("{context} is not a record and cannot use a record pattern"),
                ));
            };
            bind_record_fields(catalog, definitions, fields, *rest, context, false)
        }
        MatchPattern::Tuple(patterns) => {
            let ScalarType::Tuple(items) = catalog.underlying(expected)? else {
                return Err(Error::new(
                    "E_MATCH",
                    format!("{context} is not a tuple and cannot use a tuple pattern"),
                ));
            };
            if patterns.len() != items.len() {
                return Err(Error::new(
                    "E_MATCH",
                    format!(
                        "tuple pattern for {context} expects {} item(s), got {}",
                        items.len(),
                        patterns.len()
                    ),
                ));
            }
            let mut infos = Vec::with_capacity(patterns.len());
            for (index, (pattern, ty)) in patterns.iter_mut().zip(items).enumerate() {
                infos.push(bind_nested_pattern(
                    catalog,
                    ty,
                    pattern,
                    &format!("tuple item {} of {context}", index + 1),
                )?);
            }
            combine_patterns(infos)
        }
    }
}

fn bind_record_fields(
    catalog: &Catalog,
    definitions: &[Column],
    fields: &mut [crate::query::MatchField],
    rest: bool,
    context: &str,
    constructor_payload: bool,
) -> Result<PatternInfo> {
    let mut seen_fields = BTreeSet::new();
    let mut infos = Vec::with_capacity(fields.len());
    for field in fields {
        if !seen_fields.insert(&field.field) {
            return Err(Error::new(
                "E_MATCH",
                format!("field '{}' is bound more than once", field.field),
            ));
        }
        let definition = definitions
            .iter()
            .find(|definition| definition.name == field.field)
            .ok_or_else(|| {
                let message = if constructor_payload {
                    format!("{context} has no payload field '{}'", field.field)
                } else {
                    format!("{context} has no field '{}'", field.field)
                };
                Error::new("E_MATCH", message)
            })?;
        infos.push(bind_nested_pattern(
            catalog,
            &definition.ty,
            &mut field.pattern,
            &format!("field '{}' of {context}", field.field),
        )?);
    }
    if !rest {
        let missing = definitions
            .iter()
            .filter(|field| !seen_fields.contains(&field.name))
            .map(|field| field.name.clone())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(Error::new(
                "E_MATCH",
                format!(
                    "record pattern for {context} omits {}; add '..' to ignore them",
                    missing.join(", ")
                ),
            ));
        }
    }
    combine_patterns(infos)
}

fn combine_patterns(infos: Vec<PatternInfo>) -> Result<PatternInfo> {
    let mut seen = BTreeSet::new();
    let mut bindings = Vec::new();
    for info in infos {
        for binding in info.bindings {
            if !seen.insert(binding.name.clone()) {
                return Err(Error::new(
                    "E_MATCH",
                    format!("binding '{}' is declared more than once", binding.name),
                ));
            }
            bindings.push(binding);
        }
    }
    Ok(PatternInfo { bindings })
}

const MAX_COVERAGE_STEPS: usize = 100_000;

// Bindings and wildcards cover the same value space. Constructors retain only
// the shape needed by the usefulness algorithm; binding types stay in the
// regular typed pattern tree used during evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoveragePattern {
    Wildcard,
    Constructor(CoverageConstructor, Vec<CoveragePattern>),
}

impl CoveragePattern {
    fn root_name(&self) -> Option<String> {
        match self {
            Self::Wildcard => None,
            Self::Constructor(constructor, _) => constructor.name(),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Wildcard => "_".into(),
            Self::Constructor(CoverageConstructor::Variant { name, .. }, arguments) => {
                if arguments.is_empty() {
                    name.clone()
                } else if let [Self::Constructor(CoverageConstructor::Record(fields), values)] =
                    arguments.as_slice()
                {
                    format!("{name} {}", display_record(fields, values))
                } else {
                    format!(
                        "{name}({})",
                        arguments
                            .iter()
                            .map(Self::display)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
            Self::Constructor(CoverageConstructor::None, _) => "None".into(),
            Self::Constructor(CoverageConstructor::Some, arguments) => {
                format!(
                    "Some {}",
                    arguments
                        .first()
                        .map(Self::display)
                        .unwrap_or_else(|| "_".into())
                )
            }
            Self::Constructor(CoverageConstructor::Record(fields), values) => {
                display_record(fields, values)
            }
            Self::Constructor(CoverageConstructor::Tuple, values) => format!(
                "({})",
                values
                    .iter()
                    .map(Self::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn display_record(fields: &[String], values: &[CoveragePattern]) -> String {
    format!(
        "{{{}}}",
        fields
            .iter()
            .zip(values)
            .map(|(field, value)| format!("{field} = {}", value.display()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoverageConstructor {
    Variant { id: u64, name: String },
    None,
    Some,
    Record(Vec<String>),
    Tuple,
}

impl CoverageConstructor {
    fn name(&self) -> Option<String> {
        match self {
            Self::Variant { name, .. } => Some(name.clone()),
            Self::None => Some("None".into()),
            Self::Some => Some("Some".into()),
            Self::Record(_) | Self::Tuple => None,
        }
    }
}

#[derive(Clone)]
struct ConstructorSpec {
    constructor: CoverageConstructor,
    arguments: Vec<ScalarType>,
}

struct CoverageBudget {
    remaining: usize,
}

impl Default for CoverageBudget {
    fn default() -> Self {
        Self {
            remaining: MAX_COVERAGE_STEPS,
        }
    }
}

impl CoverageBudget {
    fn step(&mut self) -> Result<()> {
        self.remaining = self.remaining.checked_sub(1).ok_or_else(|| {
            Error::new(
                "E_LIMIT",
                format!(
                    "match coverage analysis exceeds {MAX_COVERAGE_STEPS} steps; simplify the nested patterns"
                ),
            )
        })?;
        Ok(())
    }
}

fn coverage_pattern(
    catalog: &Catalog,
    expected: &ScalarType,
    pattern: &MatchPattern,
    context: &str,
) -> Result<CoveragePattern> {
    match pattern {
        MatchPattern::Wildcard | MatchPattern::Binding(_) => Ok(CoveragePattern::Wildcard),
        MatchPattern::Constructor { name, payload, .. } => {
            let source = match_source(catalog, expected, context)?;
            let (tag, arguments, display_name) =
                resolve_constructor(catalog, &source, context, name)?;
            let constructor = match tag {
                MatchTag::Variant(id) => CoverageConstructor::Variant {
                    id,
                    name: display_name,
                },
                MatchTag::None => CoverageConstructor::None,
                MatchTag::Some => CoverageConstructor::Some,
            };
            let values = match payload {
                MatchPayload::Unit => Vec::new(),
                MatchPayload::Record { fields, .. } => {
                    let [record_type] = arguments.as_slice() else {
                        return Err(Error::new(
                            "E_MATCH",
                            format!("invalid record payload in pattern for {context}"),
                        ));
                    };
                    vec![coverage_record_pattern(
                        catalog,
                        record_type,
                        fields,
                        context,
                    )?]
                }
                MatchPayload::Positional(patterns) => patterns
                    .iter()
                    .zip(&arguments)
                    .map(|(pattern, ty)| coverage_pattern(catalog, ty, pattern, context))
                    .collect::<Result<_>>()?,
            };
            Ok(CoveragePattern::Constructor(constructor, values))
        }
        MatchPattern::Record { fields, .. } => {
            coverage_record_pattern(catalog, expected, fields, context)
        }
        MatchPattern::Tuple(patterns) => {
            let ScalarType::Tuple(items) = catalog.underlying(expected)? else {
                return Err(Error::new(
                    "E_MATCH",
                    format!("{context} is not a tuple and cannot use a tuple pattern"),
                ));
            };
            let values = patterns
                .iter()
                .zip(items)
                .map(|(pattern, ty)| coverage_pattern(catalog, ty, pattern, context))
                .collect::<Result<_>>()?;
            Ok(CoveragePattern::Constructor(
                CoverageConstructor::Tuple,
                values,
            ))
        }
    }
}

fn coverage_record_pattern(
    catalog: &Catalog,
    expected: &ScalarType,
    fields: &[crate::query::MatchField],
    context: &str,
) -> Result<CoveragePattern> {
    let ScalarType::Record(definitions) = catalog.underlying(expected)? else {
        return Err(Error::new(
            "E_MATCH",
            format!("{context} is not a record and cannot use a record pattern"),
        ));
    };
    let values = definitions
        .iter()
        .map(|definition| {
            fields
                .iter()
                .find(|field| field.field == definition.name)
                .map_or(Ok(CoveragePattern::Wildcard), |field| {
                    coverage_pattern(catalog, &definition.ty, &field.pattern, context)
                })
        })
        .collect::<Result<_>>()?;
    Ok(CoveragePattern::Constructor(
        CoverageConstructor::Record(definitions.iter().map(|field| field.name.clone()).collect()),
        values,
    ))
}

fn constructor_specs(catalog: &Catalog, ty: &ScalarType) -> Result<Option<Vec<ConstructorSpec>>> {
    Ok(match catalog.underlying(ty)? {
        ScalarType::Enum(enum_type) => {
            let recursive_id = match ty {
                ScalarType::Ref(id) => Some(*id),
                _ => None,
            };
            let mut variants = enum_type.variants.iter().collect::<Vec<_>>();
            if let Some(recursive_id) = recursive_id {
                // Prefer a terminating branch when constructing a missing
                // witness. Otherwise `Next Tree | End` would descend through
                // Next forever before considering End.
                variants.sort_by_key(|variant| {
                    variant
                        .args
                        .iter()
                        .any(|argument| type_contains_ref(argument, recursive_id))
                });
            }
            Some(
                variants
                    .into_iter()
                    .map(|variant| ConstructorSpec {
                        constructor: CoverageConstructor::Variant {
                            id: variant.id,
                            name: variant.name.clone(),
                        },
                        arguments: variant.args.clone(),
                    })
                    .collect(),
            )
        }
        ScalarType::Option(inner) => Some(vec![
            ConstructorSpec {
                constructor: CoverageConstructor::None,
                arguments: Vec::new(),
            },
            ConstructorSpec {
                constructor: CoverageConstructor::Some,
                arguments: vec![inner.as_ref().clone()],
            },
        ]),
        ScalarType::Record(fields) => Some(vec![ConstructorSpec {
            constructor: CoverageConstructor::Record(
                fields.iter().map(|field| field.name.clone()).collect(),
            ),
            arguments: fields.iter().map(|field| field.ty.clone()).collect(),
        }]),
        ScalarType::Tuple(items) => Some(vec![ConstructorSpec {
            constructor: CoverageConstructor::Tuple,
            arguments: items.clone(),
        }]),
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Uuid
        | ScalarType::Date
        | ScalarType::Timestamp
        | ScalarType::Duration
        | ScalarType::Decimal { .. }
        | ScalarType::Bytes
        | ScalarType::List(_)
        | ScalarType::Named(_)
        | ScalarType::Ref(_) => None,
    })
}

fn type_contains_ref(ty: &ScalarType, target: u64) -> bool {
    match ty {
        ScalarType::Ref(id) => *id == target,
        ScalarType::Record(fields) => fields
            .iter()
            .any(|field| type_contains_ref(&field.ty, target)),
        ScalarType::Enum(sum) => sum.variants.iter().any(|variant| {
            variant
                .args
                .iter()
                .any(|argument| type_contains_ref(argument, target))
        }),
        ScalarType::Tuple(items) => items.iter().any(|item| type_contains_ref(item, target)),
        ScalarType::Option(inner) | ScalarType::List(inner) => type_contains_ref(inner, target),
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Uuid
        | ScalarType::Date
        | ScalarType::Timestamp
        | ScalarType::Duration
        | ScalarType::Decimal { .. }
        | ScalarType::Bytes
        | ScalarType::Named(_) => false,
    }
}

fn pattern_is_useful(
    catalog: &Catalog,
    matrix: &[Vec<CoveragePattern>],
    query: &[CoveragePattern],
    types: &[ScalarType],
    budget: &mut CoverageBudget,
) -> Result<bool> {
    // This is the single-scrutinee form of pattern-matrix usefulness analysis.
    // Expanding product constructors into more columns preserves correlations
    // between tuple/record fields instead of checking each field independently.
    budget.step()?;
    if matrix.iter().any(|row| {
        row.len() == query.len()
            && row
                .iter()
                .all(|pattern| matches!(pattern, CoveragePattern::Wildcard))
    }) {
        return Ok(false);
    }
    if query.is_empty() {
        return Ok(matrix.is_empty());
    }
    let Some((head_type, tail_types)) = types.split_first() else {
        return Err(Error::new(
            "E_MATCH",
            "invalid pattern coverage type vector",
        ));
    };
    match &query[0] {
        CoveragePattern::Wildcard => {
            if let Some(constructors) = constructor_specs(catalog, head_type)? {
                for constructor in constructors {
                    let specialized = specialize_matrix(
                        matrix,
                        &constructor.constructor,
                        constructor.arguments.len(),
                    );
                    let mut specialized_query =
                        vec![CoveragePattern::Wildcard; constructor.arguments.len()];
                    specialized_query.extend_from_slice(&query[1..]);
                    let mut specialized_types = constructor.arguments;
                    specialized_types.extend_from_slice(tail_types);
                    if pattern_is_useful(
                        catalog,
                        &specialized,
                        &specialized_query,
                        &specialized_types,
                        budget,
                    )? {
                        return Ok(true);
                    }
                }
                Ok(false)
            } else {
                pattern_is_useful(
                    catalog,
                    &default_matrix(matrix),
                    &query[1..],
                    tail_types,
                    budget,
                )
            }
        }
        CoveragePattern::Constructor(constructor, arguments) => {
            let spec = constructor_specs(catalog, head_type)?
                .and_then(|constructors| {
                    constructors
                        .into_iter()
                        .find(|candidate| candidate.constructor == *constructor)
                })
                .ok_or_else(|| {
                    Error::new("E_MATCH", "pattern constructor does not match its type")
                })?;
            let specialized = specialize_matrix(matrix, constructor, spec.arguments.len());
            let mut specialized_query = arguments.clone();
            specialized_query.extend_from_slice(&query[1..]);
            let mut specialized_types = spec.arguments;
            specialized_types.extend_from_slice(tail_types);
            pattern_is_useful(
                catalog,
                &specialized,
                &specialized_query,
                &specialized_types,
                budget,
            )
        }
    }
}

fn uncovered_patterns(
    catalog: &Catalog,
    matrix: &[Vec<CoveragePattern>],
    types: &[ScalarType],
    budget: &mut CoverageBudget,
) -> Result<Option<Vec<CoveragePattern>>> {
    budget.step()?;
    if matrix.iter().any(|row| {
        row.len() == types.len()
            && row
                .iter()
                .all(|pattern| matches!(pattern, CoveragePattern::Wildcard))
    }) {
        return Ok(None);
    }
    let Some((head_type, tail_types)) = types.split_first() else {
        return Ok(matrix.is_empty().then(Vec::new));
    };
    if let Some(constructors) = constructor_specs(catalog, head_type)? {
        for constructor in constructors {
            let specialized = specialize_matrix(
                matrix,
                &constructor.constructor,
                constructor.arguments.len(),
            );
            let mut specialized_types = constructor.arguments.clone();
            specialized_types.extend_from_slice(tail_types);
            if let Some(mut witness) =
                uncovered_patterns(catalog, &specialized, &specialized_types, budget)?
            {
                let tail = witness.split_off(constructor.arguments.len());
                let mut result = vec![CoveragePattern::Constructor(
                    constructor.constructor,
                    witness,
                )];
                result.extend(tail);
                return Ok(Some(result));
            }
        }
        Ok(None)
    } else {
        uncovered_patterns(catalog, &default_matrix(matrix), tail_types, budget).map(|result| {
            result.map(|mut tail| {
                tail.insert(0, CoveragePattern::Wildcard);
                tail
            })
        })
    }
}

fn missing_top_level_constructors(
    catalog: &Catalog,
    source_type: &ScalarType,
    matrix: &[Vec<CoveragePattern>],
    budget: &mut CoverageBudget,
) -> Result<Vec<String>> {
    let constructors = constructor_specs(catalog, source_type)?.ok_or_else(|| {
        Error::new(
            "E_MATCH",
            "top-level match source does not have constructors",
        )
    })?;
    let mut missing = Vec::new();
    for constructor in constructors {
        let pattern = CoveragePattern::Constructor(
            constructor.constructor.clone(),
            vec![CoveragePattern::Wildcard; constructor.arguments.len()],
        );
        if pattern_is_useful(
            catalog,
            matrix,
            std::slice::from_ref(&pattern),
            std::slice::from_ref(source_type),
            budget,
        )? {
            missing.push(
                constructor
                    .constructor
                    .name()
                    .unwrap_or_else(|| pattern.display()),
            );
        }
    }
    if missing.is_empty() {
        missing.push("a nested value".into());
    }
    Ok(missing)
}

fn specialize_matrix(
    matrix: &[Vec<CoveragePattern>],
    constructor: &CoverageConstructor,
    arity: usize,
) -> Vec<Vec<CoveragePattern>> {
    matrix
        .iter()
        .filter_map(|row| {
            let (head, tail) = row.split_first()?;
            match head {
                CoveragePattern::Wildcard => {
                    let mut specialized = vec![CoveragePattern::Wildcard; arity];
                    specialized.extend_from_slice(tail);
                    Some(specialized)
                }
                CoveragePattern::Constructor(candidate, arguments) if candidate == constructor => {
                    let mut specialized = arguments.clone();
                    specialized.extend_from_slice(tail);
                    Some(specialized)
                }
                CoveragePattern::Constructor(_, _) => None,
            }
        })
        .collect()
}

fn default_matrix(matrix: &[Vec<CoveragePattern>]) -> Vec<Vec<CoveragePattern>> {
    matrix
        .iter()
        .filter_map(|row| {
            let (head, tail) = row.split_first()?;
            matches!(head, CoveragePattern::Wildcard).then(|| tail.to_vec())
        })
        .collect()
}

fn infer_result_type(
    catalog: &Catalog,
    result: &MatchValue,
    bindings: &[Column],
) -> Result<Option<ScalarType>> {
    match result {
        MatchValue::Binding(path) => Ok(Some(binding_type(catalog, bindings, path)?.clone())),
        MatchValue::Literal(value) => literal_type(catalog, value),
        MatchValue::Expression(expression) => crate::expression::infer_value_expression(
            catalog,
            bindings,
            expression,
            "match binding",
        ),
        MatchValue::Constructor { name, payload } => {
            if let Some(definition) = catalog.types.get(name)
                && matches!(payload, MatchValuePayload::Record(_))
                && matches!(catalog.underlying(&definition.ty)?, ScalarType::Record(_))
            {
                return Ok(Some(ScalarType::Ref(definition.id)));
            }
            if let Some((qualifier, _)) = name.rsplit_once('.') {
                return Ok(catalog
                    .types
                    .get(qualifier)
                    .map(|definition| ScalarType::Ref(definition.id)));
            }
            if name == "Some" {
                return infer_single_payload(catalog, payload, bindings)
                    .map(|ty| ty.map(|ty| ScalarType::Option(Box::new(ty))));
            }
            Ok(None)
        }
        MatchValue::Record(fields) => infer_record_type(catalog, fields, bindings),
        MatchValue::Tuple(values) => {
            let types = values
                .iter()
                .map(|value| infer_result_type(catalog, value, bindings))
                .collect::<Result<Option<Vec<_>>>>()?;
            Ok(types.map(ScalarType::Tuple))
        }
        MatchValue::List(values) if !values.is_empty() => {
            let Some(first) = infer_result_type(catalog, &values[0], bindings)? else {
                return Ok(None);
            };
            let rest = values
                .iter()
                .skip(1)
                .map(|value| infer_result_type(catalog, value, bindings))
                .collect::<Result<Vec<_>>>()?;
            if rest
                .iter()
                .all(|ty| ty.as_ref().is_some_and(|ty| same_type(&first, ty)))
            {
                Ok(Some(ScalarType::List(Box::new(first))))
            } else {
                Ok(None)
            }
        }
        MatchValue::List(_) => Ok(None),
    }
}

fn infer_single_payload(
    catalog: &Catalog,
    payload: &MatchValuePayload,
    bindings: &[Column],
) -> Result<Option<ScalarType>> {
    match payload {
        MatchValuePayload::Unit => Ok(None),
        MatchValuePayload::Record(fields) => infer_record_type(catalog, fields, bindings),
        MatchValuePayload::Positional(values) if values.len() == 1 => {
            infer_result_type(catalog, &values[0], bindings)
        }
        MatchValuePayload::Positional(_) => Ok(None),
    }
}

fn infer_record_type(
    catalog: &Catalog,
    fields: &[MatchValueField],
    bindings: &[Column],
) -> Result<Option<ScalarType>> {
    if fields.is_empty() {
        return Ok(None);
    }
    let columns = fields
        .iter()
        .map(|field| {
            Ok(
                infer_result_type(catalog, &field.value, bindings)?.map(|ty| Column {
                    name: field.name.clone(),
                    ty,
                    default: None,
                    id: 0,
                }),
            )
        })
        .collect::<Result<Option<Vec<_>>>>()?;
    Ok(columns.map(ScalarType::Record))
}

fn literal_type(catalog: &Catalog, value: &Value) -> Result<Option<ScalarType>> {
    Ok(match value {
        Value::Int(_) => Some(ScalarType::Int),
        Value::Float(_) => Some(ScalarType::Float),
        Value::Bool(_) => Some(ScalarType::Bool),
        Value::Text(_) => Some(ScalarType::Text),
        Value::Uuid(_) => Some(ScalarType::Uuid),
        Value::Date(_) => Some(ScalarType::Date),
        Value::Timestamp(_) => Some(ScalarType::Timestamp),
        Value::Duration(_) => Some(ScalarType::Duration),
        Value::Bytes(_) => Some(ScalarType::Bytes),
        Value::Decimal(value) => Some(ScalarType::Decimal {
            precision: value.precision(),
            scale: value.scale(),
        }),

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
    context: &str,
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
                        "{context} branch returns {}, expected {}",
                        catalog.describe(actual),
                        catalog.describe(expected)
                    ),
                ))
            }
        }
        MatchValue::Literal(value) => {
            *value = catalog.coerce(value, expected, &format!("{context} branch"))?;
            Ok(())
        }
        MatchValue::Expression(expression) => crate::expression::bind_value_expression(
            catalog,
            bindings,
            expression,
            expected,
            "match binding",
            &format!("{context} branch"),
        ),
        MatchValue::Constructor { name, payload } => {
            bind_constructor_result(catalog, context, expected, name, payload, bindings)
        }
        MatchValue::Record(fields) => {
            bind_record_result(catalog, context, expected, fields, bindings)
        }
        MatchValue::Tuple(values) => {
            let ScalarType::Tuple(items) = catalog.underlying(expected)? else {
                return Err(result_type_error(catalog, context, expected, "tuple"));
            };
            if values.len() != items.len() {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "{context} branch constructs a tuple with {} item(s), expected {}",
                        values.len(),
                        items.len()
                    ),
                ));
            }
            for (value, ty) in values.iter_mut().zip(items) {
                bind_result(catalog, context, ty, value, bindings)?;
            }
            Ok(())
        }
        MatchValue::List(values) => {
            let ScalarType::List(item) = catalog.underlying(expected)? else {
                return Err(result_type_error(catalog, context, expected, "list"));
            };
            for value in values {
                bind_result(catalog, context, item, value, bindings)?;
            }
            Ok(())
        }
    }
}

pub(crate) fn bind_migration_result(
    catalog: &Catalog,
    expected: &ScalarType,
    result: &mut MatchValue,
    binding: &str,
    binding_type: ScalarType,
) -> Result<()> {
    if matches!(result, MatchValue::Binding(path) if path == binding)
        && matches!(binding_type, ScalarType::Int)
        && matches!(catalog.underlying(expected)?, ScalarType::Float)
    {
        return Ok(());
    }
    let bindings = [Column {
        name: binding.into(),
        ty: binding_type,
        default: None,
        id: 0,
    }];
    bind_result(catalog, "migration", expected, result, &bindings)
}

fn bind_constructor_result(
    catalog: &Catalog,
    context: &str,
    expected: &ScalarType,
    name: &str,
    payload: &mut MatchValuePayload,
    bindings: &[Column],
) -> Result<()> {
    if let ScalarType::Ref(id) = expected {
        let definition = catalog.definition(*id)?;
        if definition.name == name
            && matches!(catalog.underlying(&definition.ty)?, ScalarType::Record(_))
        {
            let MatchValuePayload::Record(fields) = payload else {
                return Err(Error::new(
                    "E_TYPE",
                    format!("named record '{name}' requires a record value"),
                ));
            };
            return bind_record_result(catalog, context, expected, fields, bindings);
        }
    }

    match catalog.underlying(expected)? {
        ScalarType::Option(item) => match name {
            "None" => bind_value_payload(catalog, context, &[], payload, "None", bindings),
            "Some" => bind_value_payload(
                catalog,
                context,
                std::slice::from_ref(item.as_ref()),
                payload,
                "Some",
                bindings,
            ),
            _ => Err(Error::new(
                "E_TYPE",
                format!(
                    "{context} branch constructs '{name}', expected {}",
                    catalog.describe(expected)
                ),
            )),
        },
        ScalarType::Enum(enum_type) => {
            let source = MatchSource::Enum(enum_type.clone());
            let (_, argument_types, display_name) =
                resolve_constructor(catalog, &source, context, name)?;
            bind_value_payload(
                catalog,
                context,
                &argument_types,
                payload,
                &display_name,
                bindings,
            )
        }
        _ => Err(result_type_error(
            catalog,
            context,
            expected,
            &format!("constructor '{name}'"),
        )),
    }
}

fn bind_value_payload(
    catalog: &Catalog,
    context: &str,
    argument_types: &[ScalarType],
    payload: &mut MatchValuePayload,
    constructor: &str,
    bindings: &[Column],
) -> Result<()> {
    match payload {
        MatchValuePayload::Unit if argument_types.is_empty() => Ok(()),
        MatchValuePayload::Unit => Err(Error::new(
            "E_TYPE",
            format!(
                "constructor '{constructor}' expects {} argument(s), got 0",
                argument_types.len()
            ),
        )),
        MatchValuePayload::Record(fields) => {
            let [record_type] = argument_types else {
                return Err(Error::new(
                    "E_TYPE",
                    format!("constructor '{constructor}' does not have one record argument"),
                ));
            };
            bind_record_result(catalog, context, record_type, fields, bindings)
        }
        MatchValuePayload::Positional(values) => {
            if values.len() != argument_types.len() {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "constructor '{constructor}' expects {} argument(s), got {}",
                        argument_types.len(),
                        values.len()
                    ),
                ));
            }
            for (value, ty) in values.iter_mut().zip(argument_types) {
                bind_result(catalog, context, ty, value, bindings)?;
            }
            Ok(())
        }
    }
}

fn bind_record_result(
    catalog: &Catalog,
    context: &str,
    expected: &ScalarType,
    fields: &mut Vec<MatchValueField>,
    bindings: &[Column],
) -> Result<()> {
    let ScalarType::Record(definitions) = catalog.underlying(expected)? else {
        return Err(result_type_error(catalog, context, expected, "record"));
    };
    let mut supplied = std::mem::take(fields);
    let mut ordered = Vec::with_capacity(definitions.len());
    for definition in definitions {
        if let Some(index) = supplied
            .iter()
            .position(|field| field.name == definition.name)
        {
            let mut field = supplied.remove(index);
            bind_result(catalog, context, &definition.ty, &mut field.value, bindings)?;
            ordered.push(field);
        } else if let Some(default) = &definition.default {
            ordered.push(MatchValueField {
                name: definition.name.clone(),
                value: MatchValue::Literal(default.clone()),
            });
        } else {
            return Err(Error::new(
                "E_FIELD",
                format!(
                    "{context} branch is missing required field '{}'",
                    definition.name
                ),
            ));
        }
    }
    if let Some(field) = supplied.first() {
        return Err(Error::new(
            "E_FIELD",
            format!("{context} branch contains unknown field '{}'", field.name),
        ));
    }
    *fields = ordered;
    Ok(())
}

fn result_type_error(
    catalog: &Catalog,
    context: &str,
    expected: &ScalarType,
    actual: &str,
) -> Error {
    Error::new(
        "E_TYPE",
        format!(
            "{context} branch constructs {actual}, expected {}",
            catalog.describe(expected)
        ),
    )
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
        | (ScalarType::Text, ScalarType::Text)
        | (ScalarType::Uuid, ScalarType::Uuid)
        | (ScalarType::Date, ScalarType::Date)
        | (ScalarType::Timestamp, ScalarType::Timestamp)
        | (ScalarType::Duration, ScalarType::Duration)
        | (ScalarType::Bytes, ScalarType::Bytes) => true,
        (
            ScalarType::Decimal {
                precision: a,
                scale: b,
            },
            ScalarType::Decimal {
                precision: c,
                scale: d,
            },
        ) => a == c && b == d,
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

pub(crate) fn evaluate(
    catalog: &Catalog,
    row: &BTreeMap<String, Value>,
    pred: &MatchPredicate,
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<bool> {
    let Some(value) = row_field(row, &pred.column) else {
        return Ok(false);
    };
    for arm in &pred.arms {
        if let Some(bindings) = match_bindings(value, &arm.pattern) {
            return crate::expression::evaluate(
                catalog,
                &arm.condition,
                |path| binding_value(&bindings, path),
                budget,
            );
        }
    }
    Ok(false)
}

pub(crate) fn evaluate_derive(
    catalog: &Catalog,
    row: &BTreeMap<String, Value>,
    derive: &DeriveMatch,
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<Value> {
    evaluate_value_match(
        catalog,
        row,
        derive,
        &format!("derive '{}'", derive.name),
        budget,
    )
}

pub(crate) fn evaluate_assignment(
    catalog: &Catalog,
    row: &BTreeMap<String, Value>,
    value: &DeriveMatch,
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<Value> {
    evaluate_value_match(
        catalog,
        row,
        value,
        &format!("update field '{}'", value.name),
        budget,
    )
}

fn evaluate_value_match(
    catalog: &Catalog,
    row: &BTreeMap<String, Value>,
    value_match: &DeriveMatch,
    context: &str,
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<Value> {
    let value = row_field(row, &value_match.source).ok_or_else(|| {
        Error::new(
            "E_FIELD",
            format!(
                "missing match source '{}' during execution",
                value_match.source
            ),
        )
    })?;
    for arm in &value_match.arms {
        if let Some(bindings) = match_bindings(value, &arm.pattern) {
            let output_type = value_match
                .output_type
                .as_ref()
                .ok_or_else(|| Error::new("E_TYPE", "match value has no bound output type"))?;
            let raw = evaluate_result(catalog, output_type, &bindings, &arm.result, budget)?;
            return catalog.coerce(&raw, output_type, &format!("{context} result"));
        }
    }
    Err(Error::new(
        "E_MATCH",
        "exhaustive match did not select a branch",
    ))
}

fn evaluate_result(
    catalog: &Catalog,
    expected: &ScalarType,
    bindings: &BTreeMap<&str, &Value>,
    result: &MatchValue,
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<Value> {
    match result {
        MatchValue::Binding(path) => binding_value(bindings, path)
            .cloned()
            .ok_or_else(|| Error::new("E_MATCH", format!("missing binding '{path}'"))),
        MatchValue::Literal(value) => Ok(value.clone()),
        MatchValue::Expression(expression) => crate::expression::evaluate_derive(
            catalog,
            expression,
            |path| binding_value(bindings, path),
            budget,
        ),
        MatchValue::Constructor { name, payload } => {
            if let ScalarType::Ref(id) = expected {
                let definition = catalog.definition(*id)?;
                if definition.name == *name
                    && matches!(catalog.underlying(&definition.ty)?, ScalarType::Record(_))
                {
                    let MatchValuePayload::Record(fields) = payload else {
                        return Err(Error::new(
                            "E_TYPE",
                            format!("named record '{name}' requires a record value"),
                        ));
                    };
                    return evaluate_record(catalog, expected, bindings, fields, budget);
                }
            }
            match catalog.underlying(expected)? {
                ScalarType::Option(item) => {
                    let argument_types = if name == "None" {
                        Vec::new()
                    } else if name == "Some" {
                        vec![item.as_ref().clone()]
                    } else {
                        return Err(Error::new(
                            "E_TYPE",
                            format!("invalid option constructor '{name}' during execution"),
                        ));
                    };
                    Ok(Value::Enum(crate::model::EnumValue {
                        variant: name.clone(),
                        args: evaluate_value_payload(
                            catalog,
                            &argument_types,
                            bindings,
                            payload,
                            budget,
                        )?,
                        id: 0,
                    }))
                }
                ScalarType::Enum(enum_type) => {
                    let source = MatchSource::Enum(enum_type.clone());
                    let (_, argument_types, _) =
                        resolve_constructor(catalog, &source, "derive result", name)?;
                    Ok(Value::Enum(crate::model::EnumValue {
                        variant: name.clone(),
                        args: evaluate_value_payload(
                            catalog,
                            &argument_types,
                            bindings,
                            payload,
                            budget,
                        )?,
                        id: 0,
                    }))
                }
                _ => Err(Error::new(
                    "E_TYPE",
                    format!("invalid constructor '{name}' during execution"),
                )),
            }
        }
        MatchValue::Record(fields) => evaluate_record(catalog, expected, bindings, fields, budget),
        MatchValue::Tuple(values) => {
            let ScalarType::Tuple(items) = catalog.underlying(expected)? else {
                return Err(Error::new(
                    "E_TYPE",
                    "invalid tuple result during execution",
                ));
            };
            Ok(Value::Tuple(
                values
                    .iter()
                    .zip(items)
                    .map(|(value, ty)| evaluate_result(catalog, ty, bindings, value, budget))
                    .collect::<Result<_>>()?,
            ))
        }
        MatchValue::List(values) => {
            let ScalarType::List(item) = catalog.underlying(expected)? else {
                return Err(Error::new("E_TYPE", "invalid list result during execution"));
            };
            Ok(Value::List(
                values
                    .iter()
                    .map(|value| evaluate_result(catalog, item, bindings, value, budget))
                    .collect::<Result<_>>()?,
            ))
        }
    }
}

pub(crate) fn evaluate_migration_result(
    catalog: &Catalog,
    expected: &ScalarType,
    result: &MatchValue,
    binding: &str,
    value: &Value,
) -> Result<Value> {
    let bindings = BTreeMap::from([(binding, value)]);
    let raw = evaluate_result(
        catalog,
        expected,
        &bindings,
        result,
        &mut crate::expression::EvaluationBudget::new(),
    )?;
    catalog.coerce(&raw, expected, "migration result")
}

fn evaluate_value_payload(
    catalog: &Catalog,
    argument_types: &[ScalarType],
    bindings: &BTreeMap<&str, &Value>,
    payload: &MatchValuePayload,
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<Vec<Value>> {
    match payload {
        MatchValuePayload::Unit => Ok(Vec::new()),
        MatchValuePayload::Record(fields) => {
            let [record_type] = argument_types else {
                return Err(Error::new(
                    "E_TYPE",
                    "invalid record constructor result during execution",
                ));
            };
            Ok(vec![evaluate_record(
                catalog,
                record_type,
                bindings,
                fields,
                budget,
            )?])
        }
        MatchValuePayload::Positional(values) => values
            .iter()
            .zip(argument_types)
            .map(|(value, ty)| evaluate_result(catalog, ty, bindings, value, budget))
            .collect(),
    }
}

fn evaluate_record(
    catalog: &Catalog,
    expected: &ScalarType,
    bindings: &BTreeMap<&str, &Value>,
    fields: &[MatchValueField],
    budget: &mut crate::expression::EvaluationBudget,
) -> Result<Value> {
    let ScalarType::Record(definitions) = catalog.underlying(expected)? else {
        return Err(Error::new(
            "E_TYPE",
            "invalid record result during execution",
        ));
    };
    let values = fields
        .iter()
        .map(|field| {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == field.name)
                .ok_or_else(|| {
                    Error::new(
                        "E_FIELD",
                        format!("invalid result field '{}' during execution", field.name),
                    )
                })?;
            Ok((
                field.name.clone(),
                evaluate_result(catalog, &definition.ty, bindings, &field.value, budget)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(Value::Record(values))
}

fn match_bindings<'p, 'v>(
    value: &'v Value,
    pattern: &'p MatchPattern,
) -> Option<BTreeMap<&'p str, &'v Value>> {
    let mut bindings = BTreeMap::new();
    match_value_pattern(value, pattern, &mut bindings).then_some(bindings)
}

fn match_value_pattern<'p, 'v>(
    value: &'v Value,
    pattern: &'p MatchPattern,
    bindings: &mut BTreeMap<&'p str, &'v Value>,
) -> bool {
    match pattern {
        MatchPattern::Wildcard => true,
        MatchPattern::Binding(name) => bindings.insert(name, value).is_none(),
        MatchPattern::Constructor { payload, tag, .. } => {
            let Some(tag) = tag else {
                return false;
            };
            match (tag, value.unwrapped()) {
                (MatchTag::Variant(expected), Value::Enum(value)) if expected == &value.id => {
                    match_payload(&value.args, payload, bindings)
                }
                (MatchTag::None, Value::Option(None)) => match_payload(&[], payload, bindings),
                (MatchTag::Some, Value::Option(Some(value))) => {
                    match_payload(std::slice::from_ref(value.as_ref()), payload, bindings)
                }
                _ => false,
            }
        }
        MatchPattern::Record { fields, .. } => {
            let Value::Record(record) = value.unwrapped() else {
                return false;
            };
            fields.iter().all(|field| {
                record
                    .get(&field.field)
                    .is_some_and(|value| match_value_pattern(value, &field.pattern, bindings))
            })
        }
        MatchPattern::Tuple(patterns) => {
            let Value::Tuple(values) = value.unwrapped() else {
                return false;
            };
            patterns.len() == values.len()
                && values
                    .iter()
                    .zip(patterns)
                    .all(|(value, pattern)| match_value_pattern(value, pattern, bindings))
        }
    }
}

fn match_payload<'p, 'v>(
    values: &'v [Value],
    payload: &'p MatchPayload,
    bindings: &mut BTreeMap<&'p str, &'v Value>,
) -> bool {
    match payload {
        MatchPayload::Unit => values.is_empty(),
        MatchPayload::Record { fields, .. } => {
            let [value] = values else {
                return false;
            };
            let Value::Record(record) = value.unwrapped() else {
                return false;
            };
            fields.iter().all(|field| {
                record
                    .get(&field.field)
                    .is_some_and(|value| match_value_pattern(value, &field.pattern, bindings))
            })
        }
        MatchPayload::Positional(patterns) => {
            patterns.len() == values.len()
                && values
                    .iter()
                    .zip(patterns)
                    .all(|(value, pattern)| match_value_pattern(value, pattern, bindings))
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
