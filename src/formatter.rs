//! Canonical source formatting for the executable language.

use crate::error::Result;
use crate::model::{Column, EnumType, ScalarType, Value};
use crate::query::{
    Aggregate, AggregateFunction, ArithmeticOp, BoolExpression, DeriveMatch, LocalBinding,
    MatchField, MatchPattern, MatchPayload, MatchPredicate, MatchValue, MatchValueField,
    MatchValuePayload, MigrationTransform, Pipeline, ScalarExpression, SchemaMigration, SetValue,
    Stage, Statement,
};

/// Parse a script and return its canonical semicolon-free representation.
pub fn format_source(source: &str) -> Result<String> {
    let (comments, has_code) = comments(source);
    if !has_code {
        let mut output = String::new();
        for (_, comment) in comments {
            line(&mut output, 0, &comment);
        }
        return Ok(output);
    }
    let statements = crate::syntax::parse(source)?;
    let mut comment_index = 0;
    let mut output = String::new();
    for (index, located) in statements.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        while comment_index < comments.len() && comments[comment_index].0 < located.span.line {
            line(&mut output, 0, &comments[comment_index].1);
            comment_index += 1;
        }
        statement(&mut output, &located.statement, 0);
    }
    if comment_index < comments.len() {
        if !output.is_empty() {
            output.push('\n');
        }
        for (_, comment) in &comments[comment_index..] {
            line(&mut output, 0, comment);
        }
    }
    Ok(output)
}

fn comments(source: &str) -> (Vec<(usize, String)>, bool) {
    let mut comments = Vec::new();
    let mut has_code = false;
    for (line_index, line) in source.lines().enumerate() {
        let mut quoted = false;
        let mut escaped = false;
        let mut comment_offset = None;
        for (offset, character) in line.char_indices() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quoted = false;
                }
            } else if character == '"' {
                quoted = true;
            } else if character == '#' {
                comments.push((line_index + 1, line[offset..].trim_end().to_string()));
                comment_offset = Some(offset);
                break;
            }
        }
        let code = comment_offset.map_or(line, |offset| &line[..offset]);
        has_code |= !code.trim().is_empty();
    }
    (comments, has_code)
}

fn statement(output: &mut String, value: &Statement, depth: usize) {
    match value {
        Statement::DefineType { name, ty } => type_definition(output, "type", name, ty, depth),
        Statement::CreateTable { table, columns } => line(
            output,
            depth,
            &format!("create table {table} ({})", columns_text(columns)),
        ),
        Statement::TypedTable {
            table,
            row_type,
            key,
        } => {
            line(output, depth, &format!("table {table} {row_type}"));
            if let Some(key) = key {
                line(output, depth + 1, &format!("key {key}"));
            }
        }
        Statement::CreateIndex { table, column } => {
            line(output, depth, &format!("create index {table} ({column})"));
        }
        Statement::Insert {
            table,
            values,
            returning,
        } => {
            row_write(output, "insert", table, values, depth);
            returning_text(output, returning.as_ref(), depth);
        }
        Statement::InsertParameter {
            table,
            parameter,
            returning,
        } => {
            line(output, depth, &format!("insert {table} ${parameter}"));
            returning_text(output, returning.as_ref(), depth);
        }
        Statement::Upsert {
            table,
            values,
            returning,
        } => {
            row_write(output, "upsert", table, values, depth);
            returning_text(output, returning.as_ref(), depth);
        }
        Statement::UpsertParameter {
            table,
            parameter,
            returning,
        } => {
            line(output, depth, &format!("upsert {table} ${parameter}"));
            returning_text(output, returning.as_ref(), depth);
        }
        Statement::Update {
            target,
            assignments,
            returning,
        } => {
            line(output, depth, &format!("update {}", target.from));
            for stage in &target.stages {
                stage_text(output, stage, depth);
            }
            for assignment in assignments {
                match &assignment.value {
                    SetValue::Expression(value) => line(
                        output,
                        depth,
                        &format!("set {} = {}", assignment.path, scalar(value, 0, false)),
                    ),
                    SetValue::Match(value) => {
                        line(output, depth, &format!("set {} =", assignment.path));
                        match_value_arms(output, value, depth + 1);
                    }
                }
            }
            returning_text(output, returning.as_ref(), depth);
        }
        Statement::Delete { target, returning } => {
            line(output, depth, &format!("delete {}", target.from));
            for stage in &target.stages {
                stage_text(output, stage, depth);
            }
            returning_text(output, returning.as_ref(), depth);
        }
        Statement::Migration {
            name,
            parent,
            steps,
        } => {
            line(output, depth, &format!("migration {name}"));
            if let Some(parent) = parent {
                line(output, depth + 1, &format!("parent {parent}"));
            }
            for step in steps {
                migration_step(output, step, depth + 1);
            }
        }
        Statement::Explain(pipeline) => {
            line(output, depth, "explain");
            pipeline_text(output, pipeline, depth + 1);
        }
        Statement::Pipeline(pipeline) => pipeline_text(output, pipeline, depth),
    }
}

fn returning_text(output: &mut String, returning: Option<&crate::query::Returning>, depth: usize) {
    let Some(returning) = returning else {
        return;
    };
    if returning.fields.is_empty() {
        line(output, depth, "returning");
    } else {
        line(
            output,
            depth,
            &format!("returning {}", returning.fields.join(", ")),
        );
    }
}

fn row_write(output: &mut String, operation: &str, table: &str, value: &Value, depth: usize) {
    if let Value::Record(fields) = value {
        line(output, depth, &format!("{operation} {table}"));
        record_lines(output, fields, depth + 1);
    } else {
        line(
            output,
            depth,
            &format!("{operation} {table} {}", value.source_text()),
        );
    }
}

fn record_lines(
    output: &mut String,
    fields: &std::collections::BTreeMap<String, Value>,
    depth: usize,
) {
    for (name, value) in fields {
        if let Value::Record(nested) = value {
            line(output, depth, &format!("{name} ="));
            record_lines(output, nested, depth + 1);
        } else {
            line(output, depth, &format!("{name} = {}", value.source_text()));
        }
    }
}

fn type_definition(output: &mut String, prefix: &str, name: &str, ty: &ScalarType, depth: usize) {
    match ty {
        ScalarType::Record(fields) => {
            line(output, depth, &format!("{prefix} {name} ="));
            for field in fields {
                line(output, depth + 1, &column(field));
            }
        }
        ScalarType::Enum(EnumType { variants }) => {
            line(output, depth, &format!("{prefix} {name} ="));
            for (index, variant) in variants.iter().enumerate() {
                let lead = if index == 0 { "" } else { "| " };
                if let [ScalarType::Record(fields)] = variant.args.as_slice() {
                    line(output, depth + 1, &format!("{lead}{}", variant.name));
                    for field in fields {
                        line(output, depth + 2, &column(field));
                    }
                } else {
                    line(
                        output,
                        depth + 1,
                        &format!("{lead}{}{}", variant.name, variant_arguments(&variant.args)),
                    );
                }
            }
        }
        _ => line(
            output,
            depth,
            &format!("{prefix} {name} = {}", type_text(ty)),
        ),
    }
}

fn pipeline_text(output: &mut String, pipeline: &Pipeline, depth: usize) {
    line(output, depth, &format!("from {}", pipeline.from));
    for stage in &pipeline.stages {
        stage_text(output, stage, depth);
    }
}

fn stage_text(output: &mut String, stage: &Stage, depth: usize) {
    match stage {
        Stage::Let(binding) => line(output, depth, &local_binding(binding)),
        Stage::Filter(expression) => line(
            output,
            depth,
            &format!("filter {}", boolean(expression, 0, false)),
        ),
        Stage::FilterMatch(predicate) => match_predicate(output, predicate, depth),
        Stage::Derive(derive) => line(
            output,
            depth,
            &format!(
                "derive {} = {}",
                derive.name,
                boolean(&derive.expression, 0, false)
            ),
        ),
        Stage::DeriveMatch(derive) => derive_match(output, derive, depth),
        Stage::Aggregate(aggregate) => aggregate_text(output, aggregate, depth),
        Stage::Select(fields) => line(output, depth, &format!("select {{{}}}", fields.join(", "))),
        Stage::Sort(keys) => {
            let keys = keys
                .iter()
                .map(|key| format!("{}{}", if key.descending { "-" } else { "" }, key.column))
                .collect::<Vec<_>>();
            let keys = if keys.len() == 1 {
                keys[0].clone()
            } else {
                format!("{{{}}}", keys.join(", "))
            };
            line(output, depth, &format!("sort {keys}"));
        }
        Stage::Take { offset, limit } => {
            let range = if *offset == 0 {
                limit.to_string()
            } else {
                format!("{}..{}", offset + 1, offset.saturating_add(*limit))
            };
            line(output, depth, &format!("take {range}"));
        }
    }
}

fn local_binding(binding: &LocalBinding) -> String {
    let mut text = format!("let {}", binding.name);
    if let Some(annotation) = &binding.annotation {
        text.push(' ');
        text.push_str(&type_text(annotation));
    }
    text.push_str(" = ");
    match binding.parameters.as_slice() {
        [] => {}
        [parameter] if parameter.annotation.is_none() => {
            text.push_str(&parameter.name);
            text.push_str(" -> ");
        }
        parameters => {
            text.push('(');
            text.push_str(
                &parameters
                    .iter()
                    .map(|parameter| match &parameter.annotation {
                        Some(annotation) => {
                            format!("{} {}", parameter.name, type_text(annotation))
                        }
                        None => parameter.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            text.push_str(") -> ");
        }
    }
    text.push_str(&boolean(&binding.expression, 0, false));
    text
}

fn match_predicate(output: &mut String, predicate: &MatchPredicate, depth: usize) {
    line(output, depth, &format!("filter match {}", predicate.column));
    for arm in &predicate.arms {
        line(
            output,
            depth + 1,
            &format!(
                "{} => {}",
                pattern(&arm.pattern, false),
                boolean(&arm.condition, 0, false)
            ),
        );
    }
}

fn derive_match(output: &mut String, derive: &DeriveMatch, depth: usize) {
    line(
        output,
        depth,
        &format!("derive {} = match {}", derive.name, derive.source),
    );
    for arm in &derive.arms {
        line(
            output,
            depth + 1,
            &format!(
                "{} => {}",
                pattern(&arm.pattern, false),
                match_value(&arm.result, false)
            ),
        );
    }
}

fn match_value_arms(output: &mut String, value: &DeriveMatch, depth: usize) {
    line(output, depth, &format!("match {}", value.source));
    for arm in &value.arms {
        line(
            output,
            depth + 1,
            &format!(
                "{} => {}",
                pattern(&arm.pattern, false),
                match_value(&arm.result, false)
            ),
        );
    }
}

fn aggregate_text(output: &mut String, aggregate: &Aggregate, depth: usize) {
    let assignment_depth = if aggregate.group_by.is_empty() {
        line(output, depth, "aggregate");
        depth + 1
    } else {
        let keys = if aggregate.group_by.len() == 1 {
            aggregate.group_by[0].clone()
        } else {
            format!("{{{}}}", aggregate.group_by.join(", "))
        };
        line(output, depth, &format!("group {keys}"));
        line(output, depth + 1, "aggregate");
        depth + 2
    };
    for assignment in &aggregate.assignments {
        let function = match assignment.function {
            AggregateFunction::Count => "count",
            AggregateFunction::Sum => "sum",
            AggregateFunction::Min => "min",
            AggregateFunction::Max => "max",
        };
        let input = assignment
            .input
            .as_ref()
            .map(|input| format!(" {}", scalar(input, 0, false)))
            .unwrap_or_default();
        line(
            output,
            assignment_depth,
            &format!("{} = {function}{input}", assignment.name),
        );
    }
}

fn migration_step(output: &mut String, step: &SchemaMigration, depth: usize) {
    match step {
        SchemaMigration::AddType { name, ty } => {
            type_definition(output, "add type", name, ty, depth)
        }
        SchemaMigration::DropType { name } => line(output, depth, &format!("drop type {name}")),
        SchemaMigration::AddTable {
            table,
            row_type,
            key,
        } => line(
            output,
            depth,
            &format!(
                "add table {table} {row_type}{}",
                key.as_ref()
                    .map(|key| format!(" key {key}"))
                    .unwrap_or_default()
            ),
        ),
        SchemaMigration::DropTable { table } => line(output, depth, &format!("drop table {table}")),
        SchemaMigration::RenameTable { from, to } => {
            line(output, depth, &format!("rename table {from} to {to}"))
        }
        SchemaMigration::RenameType { from, to } => {
            line(output, depth, &format!("rename type {from} to {to}"))
        }
        SchemaMigration::AddField {
            owner,
            column: value,
        } => line(
            output,
            depth,
            &format!("add field {owner}.{}", column(value)),
        ),
        SchemaMigration::DropField { owner, field } => {
            line(output, depth, &format!("drop field {owner}.{field}"))
        }
        SchemaMigration::ChangeDefault {
            owner,
            field,
            value,
        } => line(
            output,
            depth,
            &format!("change default {owner}.{field} to {}", value.source_text()),
        ),
        SchemaMigration::DropDefault { owner, field } => {
            line(output, depth, &format!("drop default {owner}.{field}"))
        }
        SchemaMigration::RenameField { owner, from, to } => line(
            output,
            depth,
            &format!("rename field {owner}.{from} to {to}"),
        ),
        SchemaMigration::ChangeField {
            owner,
            field,
            ty,
            transform,
        } => line(
            output,
            depth,
            &format!(
                "change field {owner}.{field} to {} {}",
                type_text(ty),
                transform_text(transform)
            ),
        ),
        SchemaMigration::AddVariant { owner, name, args } => line(
            output,
            depth,
            &format!("add variant {owner}.{name}{}", variant_arguments(args)),
        ),
        SchemaMigration::DropVariant {
            owner,
            variant,
            transform,
        } => line(
            output,
            depth,
            &format!(
                "drop variant {owner}.{variant}{}",
                transform
                    .as_ref()
                    .map(|transform| format!(" {}", transform_text(transform)))
                    .unwrap_or_default()
            ),
        ),
        SchemaMigration::RenameVariant { owner, from, to } => line(
            output,
            depth,
            &format!("rename variant {owner}.{from} to {to}"),
        ),
        SchemaMigration::ChangeVariant {
            owner,
            variant,
            args,
            transform,
        } => line(
            output,
            depth,
            &format!(
                "change variant {owner}.{variant} to{} {}",
                variant_arguments(args),
                transform_text(transform)
            ),
        ),
        SchemaMigration::AddIndex { table, column } => {
            line(output, depth, &format!("add index {table}.{column}"))
        }
        SchemaMigration::DropIndex { table, column } => {
            line(output, depth, &format!("drop index {table}.{column}"))
        }
        SchemaMigration::SetKey { table, column } => {
            line(output, depth, &format!("set key {table}.{column}"))
        }
        SchemaMigration::DropKey { table } => line(output, depth, &format!("drop key {table}")),
    }
}

fn transform_text(transform: &MigrationTransform) -> String {
    format!(
        "using {} -> {}",
        transform.binding,
        match_value(&transform.value, false)
    )
}

fn boolean(value: &BoolExpression, parent: u8, right: bool) -> String {
    let precedence = match value {
        BoolExpression::Or(..) => 1,
        BoolExpression::And(..) => 2,
        BoolExpression::Not(_) => 3,
        _ => 4,
    };
    let text = match value {
        BoolExpression::Value(value) => scalar(value, 0, false),
        BoolExpression::Compare { left, op, right } => format!(
            "{} {} {}",
            scalar(left, 0, false),
            match op {
                crate::query::CmpOp::Eq => "==",
                crate::query::CmpOp::Ne => "!=",
                crate::query::CmpOp::Gt => ">",
                crate::query::CmpOp::Gte => ">=",
                crate::query::CmpOp::Lt => "<",
                crate::query::CmpOp::Lte => "<=",
            },
            scalar(right, 0, false)
        ),
        BoolExpression::Contains { collection, item } => format!(
            "contains {} {}",
            scalar_argument(collection),
            scalar_argument(item)
        ),
        BoolExpression::Any {
            collection,
            binding,
            predicate,
        }
        | BoolExpression::All {
            collection,
            binding,
            predicate,
        } => format!(
            "{} {} ({} -> {})",
            if matches!(value, BoolExpression::All { .. }) {
                "all"
            } else {
                "any"
            },
            scalar_argument(collection),
            binding,
            boolean(predicate, 0, false)
        ),
        BoolExpression::IsSome(value) => format!("is_some {}", scalar_argument(value)),
        BoolExpression::IsNone(value) => format!("is_none {}", scalar_argument(value)),
        BoolExpression::Not(value) => format!("not {}", boolean(value, precedence, false)),
        BoolExpression::And(left, right_value) => format!(
            "{} and {}",
            boolean(left, precedence, false),
            boolean(right_value, precedence, true)
        ),
        BoolExpression::Or(left, right_value) => format!(
            "{} or {}",
            boolean(left, precedence, false),
            boolean(right_value, precedence, true)
        ),
    };
    if precedence < parent || (right && precedence == parent) {
        format!("({text})")
    } else {
        text
    }
}

fn scalar(value: &ScalarExpression, parent: u8, right: bool) -> String {
    let precedence = match value {
        ScalarExpression::Arithmetic {
            op: ArithmeticOp::Add | ArithmeticOp::Subtract,
            ..
        } => 1,
        ScalarExpression::Arithmetic { .. } => 2,
        ScalarExpression::Negate { .. } | ScalarExpression::Length(_) => 3,
        ScalarExpression::Call { .. } => 4,
        _ => 5,
    };
    let text = match value {
        ScalarExpression::Reference(path) => path.clone(),
        ScalarExpression::Parameter { name, .. } => format!("${name}"),
        ScalarExpression::Literal(value) => value.source_text(),
        ScalarExpression::Ascribed { value, ty } => {
            format!("({} {})", scalar(value, 0, false), type_text(ty))
        }
        ScalarExpression::Call {
            name, arguments, ..
        } => format!(
            "{name} {}",
            arguments
                .iter()
                .map(scalar_argument)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        ScalarExpression::Length(value) => format!("length {}", scalar_argument(value)),
        ScalarExpression::Negate { value, .. } => format!("-{}", scalar(value, precedence, false)),
        ScalarExpression::Arithmetic {
            left,
            op,
            right: right_value,
            ..
        } => format!(
            "{} {} {}",
            scalar(left, precedence, false),
            match op {
                ArithmeticOp::Add => "+",
                ArithmeticOp::Subtract => "-",
                ArithmeticOp::Multiply => "*",
                ArithmeticOp::Divide => "/",
            },
            scalar(right_value, precedence, true)
        ),
    };
    if precedence < parent || (right && precedence == parent) {
        format!("({text})")
    } else {
        text
    }
}

fn scalar_argument(value: &ScalarExpression) -> String {
    if matches!(
        value,
        ScalarExpression::Reference(_)
            | ScalarExpression::Parameter { .. }
            | ScalarExpression::Literal(_)
            | ScalarExpression::Negate { .. }
            | ScalarExpression::Length(_)
    ) {
        scalar(value, 3, false)
    } else {
        format!("({})", scalar(value, 0, false))
    }
}

fn pattern(value: &MatchPattern, argument: bool) -> String {
    let text = match value {
        MatchPattern::Wildcard => "_".into(),
        MatchPattern::Binding(name) => name.clone(),
        MatchPattern::Constructor { name, payload, .. } => match payload {
            MatchPayload::Unit => name.clone(),
            MatchPayload::Record { fields, rest } => {
                format!("{name} {}", pattern_fields(fields, *rest))
            }
            MatchPayload::Positional(values) => format!(
                "{name} {}",
                values
                    .iter()
                    .map(|value| pattern(value, true))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        },
        MatchPattern::Record { fields, rest } => pattern_fields(fields, *rest),
        MatchPattern::Tuple(values) => format!(
            "({})",
            values
                .iter()
                .map(|value| pattern(value, false))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if argument
        && matches!(
            value,
            MatchPattern::Constructor {
                payload: MatchPayload::Positional(_),
                ..
            }
        )
    {
        format!("({text})")
    } else {
        text
    }
}

fn pattern_fields(fields: &[MatchField], rest: bool) -> String {
    let mut values = fields
        .iter()
        .map(|field| {
            if matches!(&field.pattern, MatchPattern::Binding(name) if name == &field.field) {
                field.field.clone()
            } else {
                format!("{} = {}", field.field, pattern(&field.pattern, false))
            }
        })
        .collect::<Vec<_>>();
    if rest {
        values.push("..".into());
    }
    format!("{{{}}}", values.join(", "))
}

fn match_value(value: &MatchValue, argument: bool) -> String {
    let text = match value {
        MatchValue::Binding(name) => name.clone(),
        MatchValue::Literal(value) => value.source_text(),
        MatchValue::Expression(value) => scalar(value, 0, false),
        MatchValue::Constructor { name, payload } => match payload {
            MatchValuePayload::Unit => name.clone(),
            MatchValuePayload::Record(fields) => {
                format!("{name} {}", match_value_fields(fields))
            }
            MatchValuePayload::Positional(values) => format!(
                "{name} {}",
                values
                    .iter()
                    .map(|value| match_value(value, true))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        },
        MatchValue::Record(fields) => match_value_fields(fields),
        MatchValue::Tuple(values) => format!(
            "({})",
            values
                .iter()
                .map(|value| match_value(value, false))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchValue::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| match_value(value, false))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if argument
        && matches!(
            value,
            MatchValue::Constructor {
                payload: MatchValuePayload::Positional(_),
                ..
            } | MatchValue::Expression(ScalarExpression::Arithmetic { .. })
        )
    {
        format!("({text})")
    } else {
        text
    }
}

fn match_value_fields(fields: &[MatchValueField]) -> String {
    format!(
        "{{{}}}",
        fields
            .iter()
            .map(|field| format!("{} = {}", field.name, match_value(&field.value, false)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn type_text(value: &ScalarType) -> String {
    match value {
        ScalarType::Int => "int".into(),
        ScalarType::Float => "float".into(),
        ScalarType::Bool => "bool".into(),
        ScalarType::Text => "text".into(),
        ScalarType::Named(name) => name.clone(),
        ScalarType::Ref(id) => format!("type#{id}"),
        ScalarType::Option(value) => format!("option {}", type_argument(value)),
        ScalarType::List(value) => format!("list {}", type_argument(value)),
        ScalarType::Tuple(values) => format!(
            "({})",
            values.iter().map(type_text).collect::<Vec<_>>().join(", ")
        ),
        ScalarType::Record(fields) => format!("{{{}}}", columns_text(fields)),
        ScalarType::Enum(EnumType { variants }) => format!(
            "enum({})",
            variants
                .iter()
                .map(|variant| format!("{}{}", variant.name, variant_arguments(&variant.args)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn type_argument(value: &ScalarType) -> String {
    if matches!(value, ScalarType::Option(_) | ScalarType::List(_)) {
        format!("({})", type_text(value))
    } else {
        type_text(value)
    }
}

fn variant_arguments(values: &[ScalarType]) -> String {
    match values {
        [] => String::new(),
        [value] if !matches!(value, ScalarType::Tuple(_)) => format!(" {}", type_text(value)),
        values => format!(
            " ({})",
            values.iter().map(type_text).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn column(value: &Column) -> String {
    let mut output = format!("{} {}", value.name, type_text(&value.ty));
    if let Some(default) = &value.default {
        output.push_str(" = ");
        output.push_str(&default.source_text());
    }
    output
}

fn columns_text(values: &[Column]) -> String {
    values.iter().map(column).collect::<Vec<_>>().join(", ")
}

fn line(output: &mut String, depth: usize, text: &str) {
    output.push_str(&"  ".repeat(depth));
    output.push_str(text);
    output.push('\n');
}
