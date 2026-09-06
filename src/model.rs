use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScalarType {
    Int,
    Float,
    Bool,
    Text,
    Enum(EnumType),
    Record(Vec<Column>),
    Tuple(Vec<ScalarType>),
    Option(Box<ScalarType>),
    List(Box<ScalarType>),
    // Named exists only in parsed syntax; registered definitions use stable Ref IDs.
    Named(String),
    Ref(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumType {
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumVariantDef {
    pub name: String,
    pub args: Vec<ScalarType>,
    #[serde(default)]
    pub id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumValue {
    pub variant: String,
    pub args: Vec<Value>,
    #[serde(default)]
    pub id: u64,
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
    Record(BTreeMap<String, Value>),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Option(Option<Box<Value>>),
    Named { type_id: u64, value: Box<Value> },
}

impl Value {
    pub fn unwrapped(&self) -> &Self {
        match self {
            Self::Named { value, .. } => value.unwrapped(),
            _ => self,
        }
    }

    pub fn field(&self, path: &str) -> Option<&Self> {
        let mut value = self;
        for component in path.split('.') {
            value = match value.unwrapped() {
                Self::Record(fields) => fields.get(component)?,
                _ => return None,
            };
        }
        Some(value)
    }

    pub fn cmp_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Null, Self::Null) => true,
            (
                Self::Named {
                    type_id: a,
                    value: av,
                },
                Self::Named {
                    type_id: b,
                    value: bv,
                },
            ) => a == b && av.cmp_eq(bv),
            (Self::Enum(a), Self::Enum(b)) => {
                let same_tag = if a.id == 0 || b.id == 0 {
                    a.id == b.id && a.variant == b.variant
                } else {
                    a.id == b.id
                };
                same_tag && equal_items(&a.args, &b.args)
            }
            (Self::Record(a), Self::Record(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.cmp_eq(w)))
            }
            (Self::Tuple(a), Self::Tuple(b)) | (Self::List(a), Self::List(b)) => equal_items(a, b),
            (Self::Option(None), Self::Option(None)) => true,
            (Self::Option(Some(a)), Self::Option(Some(b))) => a.cmp_eq(b),
            _ => false,
        }
    }

    pub fn cmp_ord(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(b)),
            (Self::Float(a), Self::Float(b)) => a.partial_cmp(b),
            (Self::Text(a), Self::Text(b)) => Some(a.cmp(b)),
            (
                Self::Named {
                    type_id: a,
                    value: av,
                },
                Self::Named {
                    type_id: b,
                    value: bv,
                },
            ) if a == b => av.cmp_ord(bv),
            _ => None,
        }
    }

    /// A canonical, framed equality key. All nested float zeros share one key.
    pub fn index_key(&self) -> String {
        self.index_value().to_string()
    }

    // Keep children structured until the outer serialization. Embedding encoded
    // strings here would double escaping at each level of a nested ADT.
    fn index_value(&self) -> serde_json::Value {
        match self {
            Self::Int(v) => serde_json::json!(["int", v.to_string()]),
            Self::Float(v) => serde_json::json!(["float", if *v == 0.0 { 0 } else { v.to_bits() }]),
            Self::Bool(v) => serde_json::json!(["bool", v]),
            Self::Text(v) => serde_json::json!(["text", v]),
            Self::Null => serde_json::json!(["null"]),
            Self::Named { type_id, value } => {
                serde_json::json!(["named", type_id, value.index_value()])
            }
            Self::Enum(v) => serde_json::json!([
                "enum",
                v.id,
                if v.id == 0 { &v.variant } else { "" },
                v.args.iter().map(Self::index_value).collect::<Vec<_>>()
            ]),
            Self::Record(v) => serde_json::json!([
                "record",
                v.iter()
                    .map(|(k, v)| (k, v.index_value()))
                    .collect::<Vec<_>>()
            ]),
            Self::Tuple(v) => {
                serde_json::json!(["tuple", v.iter().map(Self::index_value).collect::<Vec<_>>()])
            }
            Self::List(v) => {
                serde_json::json!(["list", v.iter().map(Self::index_value).collect::<Vec<_>>()])
            }
            Self::Option(v) => serde_json::json!(["option", v.as_ref().map(|v| v.index_value())]),
        }
    }
}

fn equal_items(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.cmp_eq(b))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: ScalarType,
    #[serde(default)]
    pub id: u64,
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
    #[serde(default)]
    pub row_type: Option<u64>,
    #[serde(default)]
    pub primary_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "object")]
pub enum DbObject {
    Table(Table),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDefinition {
    pub id: u64,
    pub name: String,
    pub ty: ScalarType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub types: BTreeMap<String, TypeDefinition>,
    next_id: u64,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            types: BTreeMap::new(),
            next_id: 1,
        }
    }
}

impl Catalog {
    fn allocate(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))?;
        Ok(id)
    }

    pub fn define(&mut self, name: String, ty: ScalarType) -> Result<()> {
        if self.types.contains_key(&name) {
            return Err(Error::new(
                "E_SCHEMA",
                format!("type '{name}' already exists"),
            ));
        }
        let ty = self.resolve(ty, 0)?;
        let id = self.allocate()?;
        self.types
            .insert(name.clone(), TypeDefinition { id, name, ty });
        Ok(())
    }

    pub fn definition(&self, id: u64) -> Result<&TypeDefinition> {
        self.types
            .values()
            .find(|d| d.id == id)
            .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type ID {id}")))
    }

    pub fn resolve(&mut self, ty: ScalarType, depth: usize) -> Result<ScalarType> {
        check_depth(depth)?;
        Ok(match ty {
            ScalarType::Named(name) => ScalarType::Ref(self.types.get(&name).ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type '{name}'; declare referenced types first (recursive types are not supported yet)")))?.id),
            ScalarType::Record(columns) => {
                let mut seen = BTreeSet::new();
                let mut resolved = Vec::new();
                for column in columns {
                    if !seen.insert(column.name.clone()) { return Err(Error::new("E_SCHEMA", format!("duplicate field '{}'", column.name))); }
                    resolved.push(Column { name: column.name, ty: self.resolve(column.ty, depth + 1)?, id: self.allocate()? });
                }
                ScalarType::Record(resolved)
            }
            ScalarType::Enum(def) => {
                let mut seen = BTreeSet::new();
                let mut variants = Vec::new();
                for v in def.variants {
                    if !seen.insert(v.name.clone()) { return Err(Error::new("E_SCHEMA", format!("duplicate variant '{}'", v.name))); }
                    variants.push(EnumVariantDef { name: v.name, args: v.args.into_iter().map(|t| self.resolve(t, depth + 1)).collect::<Result<_>>()?, id: self.allocate()? });
                }
                ScalarType::Enum(EnumType { variants })
            }
            ScalarType::Option(t) => ScalarType::Option(Box::new(self.resolve(*t, depth + 1)?)),
            ScalarType::List(t) => ScalarType::List(Box::new(self.resolve(*t, depth + 1)?)),
            ScalarType::Tuple(ts) => ScalarType::Tuple(ts.into_iter().map(|t| self.resolve(t, depth + 1)).collect::<Result<_>>()?),
            other => other,
        })
    }

    pub fn underlying<'a>(&'a self, mut ty: &'a ScalarType) -> Result<&'a ScalarType> {
        for _ in 0..MAX_DEPTH {
            match ty {
                ScalarType::Ref(id) => ty = &self.definition(*id)?.ty,
                _ => return Ok(ty),
            }
        }
        Err(Error::new("E_LIMIT", "type reference depth exceeds limit"))
    }

    pub fn field_type<'a>(&'a self, fields: &'a [Column], path: &str) -> Result<&'a ScalarType> {
        if let Some(field) = fields.iter().find(|field| field.name == path) {
            return Ok(&field.ty);
        }
        let mut parts = path.split('.');
        let first = parts.next().unwrap_or_default();
        let mut ty = &fields
            .iter()
            .find(|f| f.name == first)
            .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{path}'")))?
            .ty;
        for part in parts {
            let ScalarType::Record(columns) = self.underlying(ty)? else {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "'{path}' traverses a non-record value; optional and variant values require explicit handling"
                    ),
                ));
            };
            ty = &columns
                .iter()
                .find(|c| c.name == part)
                .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{path}'")))?
                .ty;
        }
        Ok(ty)
    }

    pub fn coerce(&self, value: &Value, ty: &ScalarType, path: &str) -> Result<Value> {
        self.coerce_inner(value, ty, path, 0)
    }

    fn coerce_inner(
        &self,
        value: &Value,
        ty: &ScalarType,
        path: &str,
        depth: usize,
    ) -> Result<Value> {
        check_depth(depth)?;
        let bad = || Error::new("E_TYPE", format!("{path}: expected {}", self.describe(ty)));
        Ok(match (value, ty) {
            (v, ScalarType::Ref(id)) => {
                let raw = match v {
                    Value::Named { type_id, value } if type_id == id => value.as_ref(),
                    Value::Named { .. } => return Err(bad()),
                    _ => v,
                };
                Value::Named {
                    type_id: *id,
                    value: Box::new(self.coerce_inner(
                        raw,
                        &self.definition(*id)?.ty,
                        path,
                        depth + 1,
                    )?),
                }
            }
            (Value::Int(v), ScalarType::Int) => Value::Int(*v),
            (Value::Int(v), ScalarType::Float) if (*v as f64) as i128 == *v as i128 => {
                Value::Float(*v as f64)
            }
            (Value::Float(v), ScalarType::Float) if v.is_finite() => {
                Value::Float(if *v == 0.0 { 0.0 } else { *v })
            }
            (Value::Bool(v), ScalarType::Bool) => Value::Bool(*v),
            (Value::Text(v), ScalarType::Text) => Value::Text(v.clone()),
            (Value::Record(fields), ScalarType::Record(columns)) => {
                let mut out = BTreeMap::new();
                for column in columns {
                    let field_path = format!("{path}.{}", column.name);
                    let v = fields.get(&column.name).ok_or_else(|| Error::new("E_FIELD", format!("missing required field '{field_path}'; use None explicitly for option fields")))?;
                    out.insert(
                        column.name.clone(),
                        self.coerce_inner(v, &column.ty, &field_path, depth + 1)?,
                    );
                }
                if let Some(extra) = fields
                    .keys()
                    .find(|k| !columns.iter().any(|c| &c.name == *k))
                {
                    return Err(Error::new(
                        "E_FIELD",
                        format!("unknown field '{path}.{extra}'"),
                    ));
                }
                Value::Record(out)
            }
            (Value::Tuple(vs), ScalarType::Tuple(ts)) if vs.len() == ts.len() => Value::Tuple(
                vs.iter()
                    .zip(ts)
                    .enumerate()
                    .map(|(i, (v, t))| self.coerce_inner(v, t, &format!("{path}.{i}"), depth + 1))
                    .collect::<Result<_>>()?,
            ),
            (Value::List(vs), ScalarType::List(t)) => Value::List(
                vs.iter()
                    .enumerate()
                    .map(|(i, v)| self.coerce_inner(v, t, &format!("{path}[{i}]"), depth + 1))
                    .collect::<Result<_>>()?,
            ),
            (Value::Enum(v), ScalarType::Option(_)) if v.variant == "None" && v.args.is_empty() => {
                Value::Option(None)
            }
            (Value::Enum(v), ScalarType::Option(t)) if v.variant == "Some" && v.args.len() == 1 => {
                Value::Option(Some(Box::new(self.coerce_inner(
                    &v.args[0],
                    t,
                    path,
                    depth + 1,
                )?)))
            }
            (Value::Option(None), ScalarType::Option(_)) => Value::Option(None),
            (Value::Option(Some(v)), ScalarType::Option(t)) => {
                Value::Option(Some(Box::new(self.coerce_inner(v, t, path, depth + 1)?)))
            }
            (Value::Enum(v), ScalarType::Enum(def)) => {
                let (qualifier, variant_name) = v
                    .variant
                    .rsplit_once('.')
                    .map(|(q, n)| (Some(q), n))
                    .unwrap_or((None, &v.variant));
                if let Some(q) = qualifier {
                    let d = self.types.get(q).ok_or_else(bad)?;
                    let ScalarType::Enum(expected) = &d.ty else {
                        return Err(bad());
                    };
                    if !expected.variants.iter().any(|ev| {
                        def.variants
                            .iter()
                            .any(|actual| ev.id != 0 && ev.id == actual.id)
                    }) {
                        return Err(bad());
                    }
                }
                let variant = def
                    .variants
                    .iter()
                    .find(|d| d.name == variant_name)
                    .ok_or_else(|| {
                        Error::new("E_TYPE", format!("{path}: unknown variant '{}'", v.variant))
                    })?;
                if variant.args.len() != v.args.len() {
                    return Err(Error::new(
                        "E_TYPE",
                        format!(
                            "{path}: variant '{}' expects {} argument(s), got {}",
                            v.variant,
                            variant.args.len(),
                            v.args.len()
                        ),
                    ));
                }
                let args = v
                    .args
                    .iter()
                    .zip(&variant.args)
                    .enumerate()
                    .map(|(i, (v, t))| {
                        self.coerce_inner(v, t, &format!("{path}.{}[{i}]", variant.name), depth + 1)
                    })
                    .collect::<Result<_>>()?;
                Value::Enum(EnumValue {
                    variant: variant.name.clone(),
                    args,
                    id: variant.id,
                })
            }
            _ => return Err(bad()),
        })
    }

    pub fn describe(&self, ty: &ScalarType) -> String {
        match ty {
            ScalarType::Int => "int".into(),
            ScalarType::Float => "float".into(),
            ScalarType::Bool => "bool".into(),
            ScalarType::Text => "text".into(),
            ScalarType::Ref(id) => self
                .definition(*id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|_| format!("type#{id}")),
            ScalarType::Named(name) => name.clone(),
            ScalarType::Option(t) => format!("option {}", self.describe_argument(t)),
            ScalarType::List(t) => format!("list {}", self.describe_argument(t)),
            ScalarType::Tuple(ts) => format!(
                "({})",
                ts.iter()
                    .map(|t| self.describe(t))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarType::Record(cs) => format!(
                "{{{}}}",
                cs.iter()
                    .map(|c| format!("{} {}", c.name, self.describe(&c.ty)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarType::Enum(def) => format!(
                "enum({})",
                def.variants
                    .iter()
                    .map(|v| {
                        if v.args.is_empty() {
                            v.name.clone()
                        } else {
                            format!(
                                "{}({})",
                                v.name,
                                v.args
                                    .iter()
                                    .map(|t| self.describe(t))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn describe_argument(&self, ty: &ScalarType) -> String {
        let text = self.describe(ty);
        if matches!(ty, ScalarType::Option(_) | ScalarType::List(_)) {
            format!("({text})")
        } else {
            text
        }
    }

    pub fn describe_variant(&self, variant: &EnumVariantDef) -> String {
        match variant.args.as_slice() {
            [] => variant.name.clone(),
            [ty] if !matches!(ty, ScalarType::Tuple(_)) => {
                format!("{} {}", variant.name, self.describe(ty))
            }
            args => format!(
                "{} ({})",
                variant.name,
                args.iter()
                    .map(|t| self.describe(t))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        Err(Error::new(
            "E_LIMIT",
            format!("type/value depth exceeds {MAX_DEPTH}"),
        ))
    } else {
        Ok(())
    }
}
