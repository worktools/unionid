use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScalarType {
    Int,
    Float,
    Bool,
    Text,
    Enum(EnumType),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumType {
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumVariantDef {
    pub name: String,
    pub args: Vec<ScalarType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumValue {
    pub variant: String,
    pub args: Vec<Value>,
}

impl ScalarType {
    pub fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        let lower = trimmed.to_lowercase();

        if lower.starts_with("enum(") && trimmed.ends_with(')') {
            return parse_enum_type(trimmed).map(Self::Enum);
        }

        match lower.as_str() {
            "int" | "i64" | "integer" => Ok(Self::Int),
            "float" | "f64" | "double" => Ok(Self::Float),
            "bool" | "boolean" => Ok(Self::Bool),
            "text" | "string" => Ok(Self::Text),
            other => Err(format!("unknown scalar type: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Null,
    Enum(EnumValue),
}

impl Value {
    pub fn parse_literal(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();

        if trimmed.eq_ignore_ascii_case("null") {
            return Ok(Self::Null);
        }

        if trimmed.eq_ignore_ascii_case("true") {
            return Ok(Self::Bool(true));
        }

        if trimmed.eq_ignore_ascii_case("false") {
            return Ok(Self::Bool(false));
        }

        if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
            return Ok(Self::Text(trimmed[1..trimmed.len() - 1].to_string()));
        }

        if let Ok(val) = trimmed.parse::<i64>() {
            return Ok(Self::Int(val));
        }

        if let Ok(val) = trimmed.parse::<f64>() {
            return Ok(Self::Float(val));
        }

        if let Ok(enum_value) = parse_enum_literal(trimmed) {
            return Ok(Self::Enum(enum_value));
        }

        Err(format!("cannot parse literal: {trimmed}"))
    }

    pub fn coerce_to(&self, ty: &ScalarType) -> Result<Self, String> {
        match (self, ty) {
            (Self::Null, _) => Ok(Self::Null),
            (Self::Int(v), ScalarType::Int) => Ok(Self::Int(*v)),
            (Self::Int(v), ScalarType::Float) => Ok(Self::Float(*v as f64)),
            (Self::Int(v), ScalarType::Text) => Ok(Self::Text(v.to_string())),

            (Self::Float(v), ScalarType::Float) => Ok(Self::Float(*v)),
            (Self::Float(v), ScalarType::Text) => Ok(Self::Text(v.to_string())),

            (Self::Bool(v), ScalarType::Bool) => Ok(Self::Bool(*v)),
            (Self::Bool(v), ScalarType::Text) => Ok(Self::Text(v.to_string())),

            (Self::Text(v), ScalarType::Text) => Ok(Self::Text(v.clone())),
            (Self::Enum(v), ScalarType::Enum(def)) => Ok(Self::Enum(coerce_enum_value(v, def)?)),

            _ => Err(format!(
                "type mismatch: cannot coerce {:?} to {:?}",
                self, ty
            )),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(v) => Some(*v as f64),
            Self::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn cmp_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Enum(a), Self::Enum(b)) => {
                a.variant == b.variant
                    && a.args.len() == b.args.len()
                    && a.args.iter().zip(&b.args).all(|(x, y)| x.cmp_eq(y))
            }
            _ => {
                if let (Some(a), Some(b)) = (self.as_f64(), other.as_f64()) {
                    (a - b).abs() < f64::EPSILON
                } else {
                    false
                }
            }
        }
    }

    pub fn cmp_ord(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => Some(a.cmp(b)),
            _ => {
                let (a, b) = (self.as_f64()?, other.as_f64()?);
                a.partial_cmp(&b)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: ScalarType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub schema: Vec<Column>,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "object")]
pub enum DbObject {
    Table(Table),
}

fn parse_enum_type(input: &str) -> Result<EnumType, String> {
    let open = input.find('(').ok_or("invalid enum type syntax")?;
    let body = &input[open + 1..input.len() - 1];
    let parts = split_top_level_csv(body);

    if parts.is_empty() {
        return Err("enum type must contain at least one variant".to_string());
    }

    let mut variants = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for raw in parts {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (name, rest) = split_identifier(trimmed)?;
        if !seen.insert(name.to_string()) {
            return Err(format!("duplicate enum variant '{name}'"));
        }

        let args = if rest.is_empty() {
            Vec::new()
        } else {
            if !(rest.starts_with('(') && rest.ends_with(')')) {
                return Err(format!("invalid enum variant syntax: '{trimmed}'"));
            }
            let args_body = &rest[1..rest.len() - 1];
            if args_body.trim().is_empty() {
                Vec::new()
            } else {
                split_top_level_csv(args_body)
                    .into_iter()
                    .map(|part| ScalarType::parse(part.trim()))
                    .collect::<Result<Vec<_>, _>>()?
            }
        };

        variants.push(EnumVariantDef {
            name: name.to_string(),
            args,
        });
    }

    if variants.is_empty() {
        return Err("enum type must contain at least one variant".to_string());
    }

    Ok(EnumType { variants })
}

fn parse_enum_literal(input: &str) -> Result<EnumValue, String> {
    let (variant, rest) = split_identifier(input)?;

    let args = if rest.is_empty() {
        Vec::new()
    } else {
        if !(rest.starts_with('(') && rest.ends_with(')')) {
            return Err(format!("invalid enum literal syntax: '{input}'"));
        }
        let body = &rest[1..rest.len() - 1];
        if body.trim().is_empty() {
            Vec::new()
        } else {
            split_top_level_csv(body)
                .into_iter()
                .map(|part| Value::parse_literal(part.trim()))
                .collect::<Result<Vec<_>, _>>()?
        }
    };

    Ok(EnumValue {
        variant: variant.to_string(),
        args,
    })
}

fn coerce_enum_value(value: &EnumValue, def: &EnumType) -> Result<EnumValue, String> {
    let Some(variant) = def.variants.iter().find(|v| v.name == value.variant) else {
        return Err(format!("unknown enum variant '{}'", value.variant));
    };

    if value.args.len() != variant.args.len() {
        return Err(format!(
            "enum variant '{}' expects {} arg(s), got {}",
            value.variant,
            variant.args.len(),
            value.args.len()
        ));
    }

    let mut coerced = Vec::new();
    for (arg, ty) in value.args.iter().zip(&variant.args) {
        coerced.push(arg.coerce_to(ty)?);
    }

    Ok(EnumValue {
        variant: value.variant.clone(),
        args: coerced,
    })
}

fn split_identifier(input: &str) -> Result<(&str, &str), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty identifier".to_string());
    }

    let mut end = 0usize;
    for (idx, ch) in trimmed.char_indices() {
        let ok = if idx == 0 {
            ch.is_ascii_alphabetic() || ch == '_'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_'
        };
        if ok {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }

    if end == 0 {
        return Err(format!("invalid identifier in '{input}'"));
    }

    Ok((&trimmed[..end], trimmed[end..].trim()))
}

fn split_top_level_csv(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut paren_depth = 0i32;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '(' if !in_quotes => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' if !in_quotes => {
                paren_depth -= 1;
                current.push(ch);
            }
            ',' if !in_quotes && paren_depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }

    out
}
