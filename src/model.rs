use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid};

pub const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScalarType {
    Int,
    Float,
    Bool,
    Text,
    Uuid,
    Date,
    Timestamp,
    Duration,
    Decimal { precision: u8, scale: u8 },
    Bytes,
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
    Uuid(Uuid),
    Date(Date),
    Timestamp(Timestamp),
    Duration(Duration),
    Decimal(Decimal),
    Bytes(Bytes),
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
            (Self::Uuid(a), Self::Uuid(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::Timestamp(a), Self::Timestamp(b)) => a == b,
            (Self::Duration(a), Self::Duration(b)) => a == b,
            (Self::Decimal(a), Self::Decimal(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
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
            (Self::Uuid(a), Self::Uuid(b)) => Some(a.cmp(b)),
            (Self::Date(a), Self::Date(b)) => Some(a.cmp(b)),
            (Self::Timestamp(a), Self::Timestamp(b)) => Some(a.cmp(b)),
            (Self::Duration(a), Self::Duration(b)) => Some(a.cmp(b)),
            (Self::Bytes(a), Self::Bytes(b)) => Some(a.cmp(b)),
            (Self::Decimal(a), Self::Decimal(b)) if a.scale() == b.scale() => {
                Some(a.coefficient().cmp(&b.coefficient()))
            }
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

    /// Whether this value requires the production-scalar protocol boundary.
    pub fn requires_protocol_v2(&self) -> bool {
        match self {
            Self::Uuid(_)
            | Self::Date(_)
            | Self::Timestamp(_)
            | Self::Duration(_)
            | Self::Decimal(_)
            | Self::Bytes(_) => true,
            Self::Named { value, .. } | Self::Option(Some(value)) => value.requires_protocol_v2(),
            Self::Enum(value) => value.args.iter().any(Self::requires_protocol_v2),
            Self::Record(fields) => fields.values().any(Self::requires_protocol_v2),
            Self::Tuple(items) | Self::List(items) => items.iter().any(Self::requires_protocol_v2),
            _ => false,
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
            Self::Uuid(v) => serde_json::json!(["uuid", v]),
            Self::Date(v) => serde_json::json!(["date", v]),
            Self::Timestamp(v) => serde_json::json!(["timestamp", v]),
            Self::Duration(v) => serde_json::json!(["duration", v]),
            Self::Decimal(v) => serde_json::json!(["decimal", v]),
            Self::Bytes(v) => serde_json::json!(["bytes", v]),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default)]
    pub id: u64,
}

pub type RowId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    /// Stable identity inside a table. Deleting a row never permits this ID to
    /// identify a later row.
    #[serde(default)]
    pub id: RowId,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    /// Stable catalog identity. Names may change in a later schema revision.
    #[serde(default)]
    pub id: u64,
    pub name: String,
    pub schema: Vec<Column>,
    pub rows: Vec<Row>,
    /// The next stable row identity. This is persisted even when the row with
    /// the greatest allocated ID has been deleted.
    #[serde(default)]
    pub next_row_id: RowId,
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
    pub(crate) fn next_id(&self) -> u64 {
        self.next_id
    }

    pub(crate) fn restore_next_id(&mut self, next_id: u64) -> Result<()> {
        if next_id == 0 {
            return Err(Error::new("E_STORAGE", "catalog next ID must be positive"));
        }
        self.next_id = next_id;
        Ok(())
    }

    pub(crate) fn allocate(&mut self) -> Result<u64> {
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
        // Reserve the nominal identity before resolving the body so a direct
        // self-reference becomes the same stable Ref. Predicting the ID after
        // the body's field/variant allocations preserves the v1 allocation
        // order for every non-recursive schema. The complete catalog is
        // restored on failure because resolving also allocates those IDs.
        let previous = self.clone();
        let result = (|| {
            let body_ids = structural_id_count(&ty, 0)?;
            let id = self
                .next_id
                .checked_add(body_ids)
                .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))?;
            self.types.insert(
                name.clone(),
                TypeDefinition {
                    id,
                    name: name.clone(),
                    ty: ScalarType::Bool,
                },
            );
            let ty = self.resolve_inner(ty, 0)?;
            let allocated = self.allocate()?;
            debug_assert_eq!(allocated, id);
            self.types.get_mut(&name).expect("reserved type").ty = ty;
            // Defaults that construct the type currently being declared need
            // the complete definition, so normalize every default only after
            // the nominal body has replaced the temporary reservation.
            let ty = self.types.get(&name).expect("reserved type").ty.clone();
            let ty = self.normalize_defaults(ty, 0)?;
            self.types.get_mut(&name).expect("reserved type").ty = ty;
            self.validate_finite_types()
        })();
        if let Err(error) = result {
            *self = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn definition(&self, id: u64) -> Result<&TypeDefinition> {
        self.types
            .values()
            .find(|d| d.id == id)
            .ok_or_else(|| Error::new("E_SCHEMA", format!("unknown type ID {id}")))
    }

    pub fn resolve(&mut self, ty: ScalarType, depth: usize) -> Result<ScalarType> {
        let ty = self.resolve_inner(ty, depth)?;
        self.normalize_defaults(ty, depth)
    }

    fn resolve_inner(&mut self, ty: ScalarType, depth: usize) -> Result<ScalarType> {
        check_depth(depth)?;
        Ok(match ty {
            ScalarType::Decimal { precision, scale } => {
                crate::scalars::validate_decimal_type(precision, scale)?;
                ScalarType::Decimal { precision, scale }
            }
            ScalarType::Named(name) => ScalarType::Ref(
                self.types
                    .get(&name)
                    .ok_or_else(|| {
                        Error::new(
                            "E_SCHEMA",
                            format!("unknown type '{name}'; declare other referenced types first"),
                        )
                    })?
                    .id,
            ),
            ScalarType::Record(columns) => {
                let mut seen = BTreeSet::new();
                let mut resolved = Vec::new();
                for column in columns {
                    if !seen.insert(column.name.clone()) {
                        return Err(Error::new(
                            "E_SCHEMA",
                            format!("duplicate field '{}'", column.name),
                        ));
                    }
                    let ty = self.resolve_inner(column.ty, depth + 1)?;
                    resolved.push(Column {
                        name: column.name,
                        ty,
                        default: column.default,
                        id: self.allocate()?,
                    });
                }
                ScalarType::Record(resolved)
            }
            ScalarType::Enum(def) => {
                let mut seen = BTreeSet::new();
                let mut variants = Vec::new();
                for v in def.variants {
                    if !seen.insert(v.name.clone()) {
                        return Err(Error::new(
                            "E_SCHEMA",
                            format!("duplicate variant '{}'", v.name),
                        ));
                    }
                    variants.push(EnumVariantDef {
                        name: v.name,
                        args: v
                            .args
                            .into_iter()
                            .map(|t| self.resolve_inner(t, depth + 1))
                            .collect::<Result<_>>()?,
                        id: self.allocate()?,
                    });
                }
                ScalarType::Enum(EnumType { variants })
            }
            ScalarType::Option(t) => {
                ScalarType::Option(Box::new(self.resolve_inner(*t, depth + 1)?))
            }
            ScalarType::List(t) => ScalarType::List(Box::new(self.resolve_inner(*t, depth + 1)?)),
            ScalarType::Tuple(ts) => ScalarType::Tuple(
                ts.into_iter()
                    .map(|t| self.resolve_inner(t, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            other => other,
        })
    }

    fn normalize_defaults(&self, ty: ScalarType, depth: usize) -> Result<ScalarType> {
        check_depth(depth)?;
        Ok(match ty {
            ScalarType::Record(columns) => ScalarType::Record(
                columns
                    .into_iter()
                    .map(|column| {
                        let ty = self.normalize_defaults(column.ty, depth + 1)?;
                        let default = column
                            .default
                            .map(|value| {
                                self.coerce_inner(
                                    &value,
                                    &ty,
                                    &format!("default for field '{}'", column.name),
                                    depth + 1,
                                )
                            })
                            .transpose()?;
                        Ok(Column {
                            name: column.name,
                            ty,
                            default,
                            id: column.id,
                        })
                    })
                    .collect::<Result<_>>()?,
            ),
            ScalarType::Enum(sum) => ScalarType::Enum(EnumType {
                variants: sum
                    .variants
                    .into_iter()
                    .map(|variant| {
                        Ok(EnumVariantDef {
                            name: variant.name,
                            args: variant
                                .args
                                .into_iter()
                                .map(|argument| self.normalize_defaults(argument, depth + 1))
                                .collect::<Result<_>>()?,
                            id: variant.id,
                        })
                    })
                    .collect::<Result<_>>()?,
            }),
            ScalarType::Tuple(items) => ScalarType::Tuple(
                items
                    .into_iter()
                    .map(|item| self.normalize_defaults(item, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            ScalarType::Option(inner) => {
                ScalarType::Option(Box::new(self.normalize_defaults(*inner, depth + 1)?))
            }
            ScalarType::List(inner) => {
                ScalarType::List(Box::new(self.normalize_defaults(*inner, depth + 1)?))
            }
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

    /// Reject named-type cycles that cannot construct any finite value.
    /// Option and list are immediately inhabited by None and [], while sums
    /// need one finite branch and products need every component to be finite.
    pub(crate) fn validate_finite_types(&self) -> Result<()> {
        for definition in self.types.values() {
            validate_type_references(self, &definition.ty, 0)?;
        }
        validate_no_mutual_cycles(self)?;
        let mut finite = BTreeSet::new();
        loop {
            let mut changed = false;
            for definition in self.types.values() {
                if !finite.contains(&definition.id) && type_is_finite(&definition.ty, &finite) {
                    finite.insert(definition.id);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let uninhabited = self
            .types
            .values()
            .filter(|definition| !finite.contains(&definition.id))
            .map(|definition| format!("'{}'", definition.name))
            .collect::<Vec<_>>();
        if uninhabited.is_empty() {
            Ok(())
        } else {
            Err(Error::new(
                "E_SCHEMA",
                format!(
                    "recursive type {} has no finite value; add a terminating sum variant, option, or list path",
                    uninhabited.join(", ")
                ),
            ))
        }
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

    pub fn field_path_ids(&self, fields: &[Column], path: &str) -> Result<Vec<u64>> {
        let mut columns = fields;
        let mut ids = Vec::new();
        let mut parts = path.split('.').peekable();
        while let Some(part) = parts.next() {
            let field = columns
                .iter()
                .find(|field| field.name == part)
                .ok_or_else(|| Error::new("E_FIELD", format!("unknown field '{path}'")))?;
            ids.push(field.id);
            if parts.peek().is_some() {
                let ScalarType::Record(nested) = self.underlying(&field.ty)? else {
                    return Err(Error::new(
                        "E_TYPE",
                        format!(
                            "'{path}' traverses a non-record value; optional and variant values require explicit handling"
                        ),
                    ));
                };
                columns = nested;
            }
        }
        Ok(ids)
    }

    /// Check the complete type, including empty containers and recursive references.
    pub fn requires_protocol_v2(&self, ty: &ScalarType) -> Result<bool> {
        fn visit(
            catalog: &Catalog,
            ty: &ScalarType,
            seen: &mut BTreeSet<u64>,
            depth: usize,
        ) -> Result<bool> {
            check_depth(depth)?;
            Ok(match ty {
                ScalarType::Uuid
                | ScalarType::Date
                | ScalarType::Timestamp
                | ScalarType::Duration
                | ScalarType::Bytes => true,
                ScalarType::Decimal { precision, scale } => {
                    crate::scalars::validate_decimal_type(*precision, *scale)?;
                    true
                }
                ScalarType::Ref(id) if seen.insert(*id) => {
                    visit(catalog, &catalog.definition(*id)?.ty, seen, depth + 1)?
                }
                ScalarType::Record(fields) => {
                    let mut result = false;
                    for field in fields {
                        result |= visit(catalog, &field.ty, seen, depth + 1)?;
                    }
                    result
                }
                ScalarType::Enum(sum) => {
                    let mut result = false;
                    for variant in &sum.variants {
                        for ty in &variant.args {
                            result |= visit(catalog, ty, seen, depth + 1)?;
                        }
                    }
                    result
                }
                ScalarType::Tuple(items) => {
                    let mut result = false;
                    for ty in items {
                        result |= visit(catalog, ty, seen, depth + 1)?;
                    }
                    result
                }
                ScalarType::Option(inner) | ScalarType::List(inner) => {
                    visit(catalog, inner, seen, depth + 1)?
                }
                ScalarType::Named(name) => {
                    return Err(Error::new("E_SCHEMA", format!("unresolved type '{name}'")));
                }
                _ => false,
            })
        }
        visit(self, ty, &mut BTreeSet::new(), 0)
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
            (Value::Uuid(v), ScalarType::Uuid) => Value::Uuid(*v),
            (Value::Date(v), ScalarType::Date) => Value::Date(*v),
            (Value::Timestamp(v), ScalarType::Timestamp) => Value::Timestamp(*v),
            (Value::Duration(v), ScalarType::Duration) => Value::Duration(*v),
            (Value::Bytes(v), ScalarType::Bytes) => Value::Bytes(v.clone()),
            (Value::Decimal(v), ScalarType::Decimal { precision, scale }) => {
                Value::Decimal(v.rescale(*precision, *scale)?)
            }
            (Value::Text(v), ScalarType::Text) => Value::Text(v.clone()),
            (Value::Record(fields), ScalarType::Record(columns)) => {
                let mut out = BTreeMap::new();
                for column in columns {
                    let field_path = format!("{path}.{}", column.name);
                    let value = match fields.get(&column.name) {
                        Some(value) => {
                            self.coerce_inner(value, &column.ty, &field_path, depth + 1)?
                        }
                        None => {
                            let default = column.default.as_ref().ok_or_else(|| {
                                Error::new(
                                    "E_FIELD",
                                    format!("missing required field '{field_path}'; declare a default or provide the field explicitly"),
                                )
                            })?;
                            self.coerce_inner(default, &column.ty, &field_path, depth + 1)?
                        }
                    };
                    out.insert(column.name.clone(), value);
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
                let variant = if v.id == 0 {
                    def.variants.iter().find(|d| d.name == variant_name)
                } else {
                    def.variants.iter().find(|d| d.id == v.id)
                }
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
            ScalarType::Uuid => "uuid".into(),
            ScalarType::Date => "date".into(),
            ScalarType::Timestamp => "timestamp".into(),
            ScalarType::Duration => "duration".into(),
            ScalarType::Bytes => "bytes".into(),
            ScalarType::Decimal { precision, scale } => format!("decimal {precision} {scale}"),

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
                    .map(|c| self.describe_column(c))
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

    pub fn describe_column(&self, column: &Column) -> String {
        let mut text = format!("{} {}", column.name, self.describe(&column.ty));
        if let Some(default) = &column.default {
            text.push_str(" = ");
            text.push_str(&default.source_text());
        }
        text
    }
}

fn type_is_finite(ty: &ScalarType, finite: &BTreeSet<u64>) -> bool {
    match ty {
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Uuid
        | ScalarType::Date
        | ScalarType::Timestamp
        | ScalarType::Duration
        | ScalarType::Decimal { .. }
        | ScalarType::Bytes => true,
        ScalarType::Ref(id) => finite.contains(id),
        ScalarType::Option(_) | ScalarType::List(_) => true,
        ScalarType::Tuple(items) => items.iter().all(|item| type_is_finite(item, finite)),
        ScalarType::Record(fields) => fields.iter().all(|field| type_is_finite(&field.ty, finite)),
        ScalarType::Enum(sum) => sum.variants.iter().any(|variant| {
            variant
                .args
                .iter()
                .all(|argument| type_is_finite(argument, finite))
        }),
        ScalarType::Named(_) => false,
    }
}

fn validate_type_references(catalog: &Catalog, ty: &ScalarType, depth: usize) -> Result<()> {
    check_depth(depth)?;
    if let ScalarType::Decimal { precision, scale } = ty {
        crate::scalars::validate_decimal_type(*precision, *scale)?;
    }
    match ty {
        ScalarType::Ref(id) => {
            catalog.definition(*id)?;
        }
        ScalarType::Record(fields) => {
            for field in fields {
                validate_type_references(catalog, &field.ty, depth + 1)?;
            }
        }
        ScalarType::Enum(sum) => {
            for variant in &sum.variants {
                for argument in &variant.args {
                    validate_type_references(catalog, argument, depth + 1)?;
                }
            }
        }
        ScalarType::Tuple(items) => {
            for item in items {
                validate_type_references(catalog, item, depth + 1)?;
            }
        }
        ScalarType::Option(inner) | ScalarType::List(inner) => {
            validate_type_references(catalog, inner, depth + 1)?;
        }
        ScalarType::Named(name) => {
            return Err(Error::new(
                "E_SCHEMA",
                format!("unresolved type reference '{name}'"),
            ));
        }
        ScalarType::Int
        | ScalarType::Float
        | ScalarType::Bool
        | ScalarType::Text
        | ScalarType::Uuid
        | ScalarType::Date
        | ScalarType::Timestamp
        | ScalarType::Duration
        | ScalarType::Decimal { .. }
        | ScalarType::Bytes => {}
    }
    Ok(())
}

fn validate_no_mutual_cycles(catalog: &Catalog) -> Result<()> {
    fn visit(
        catalog: &Catalog,
        id: u64,
        visiting: &mut BTreeSet<u64>,
        visited: &mut BTreeSet<u64>,
    ) -> Result<()> {
        if visited.contains(&id) {
            return Ok(());
        }
        visiting.insert(id);
        let definition = catalog.definition(id)?;
        let mut references = BTreeSet::new();
        collect_type_references(&definition.ty, &mut references);
        for reference in references {
            if reference == id {
                continue;
            }
            if visiting.contains(&reference) {
                let other = &catalog.definition(reference)?.name;
                return Err(Error::new(
                    "E_SCHEMA",
                    format!(
                        "mutually recursive types '{}' and '{other}' are not supported; use direct self-reference or separate the values",
                        definition.name
                    ),
                ));
            }
            visit(catalog, reference, visiting, visited)?;
        }
        visiting.remove(&id);
        visited.insert(id);
        Ok(())
    }

    let mut visited = BTreeSet::new();
    for definition in catalog.types.values() {
        visit(catalog, definition.id, &mut BTreeSet::new(), &mut visited)?;
    }
    Ok(())
}

fn collect_type_references(ty: &ScalarType, references: &mut BTreeSet<u64>) {
    match ty {
        ScalarType::Ref(id) => {
            references.insert(*id);
        }
        ScalarType::Record(fields) => {
            for field in fields {
                collect_type_references(&field.ty, references);
            }
        }
        ScalarType::Enum(sum) => {
            for variant in &sum.variants {
                for argument in &variant.args {
                    collect_type_references(argument, references);
                }
            }
        }
        ScalarType::Tuple(items) => {
            for item in items {
                collect_type_references(item, references);
            }
        }
        ScalarType::Option(inner) | ScalarType::List(inner) => {
            collect_type_references(inner, references);
        }
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
        | ScalarType::Named(_) => {}
    }
}

fn structural_id_count(ty: &ScalarType, depth: usize) -> Result<u64> {
    check_depth(depth)?;
    let nested = match ty {
        ScalarType::Record(fields) => fields.iter().try_fold(0_u64, |count, field| {
            let count = count
                .checked_add(1)
                .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))?;
            count
                .checked_add(structural_id_count(&field.ty, depth + 1)?)
                .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))
        })?,
        ScalarType::Enum(sum) => sum.variants.iter().try_fold(0_u64, |count, variant| {
            let count = count
                .checked_add(1)
                .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))?;
            variant.args.iter().try_fold(count, |count, argument| {
                count
                    .checked_add(structural_id_count(argument, depth + 1)?)
                    .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))
            })
        })?,
        ScalarType::Tuple(items) => items.iter().try_fold(0_u64, |count, item| {
            count
                .checked_add(structural_id_count(item, depth + 1)?)
                .ok_or_else(|| Error::new("E_SCHEMA", "catalog ID space exhausted"))
        })?,
        ScalarType::Option(inner) | ScalarType::List(inner) => {
            structural_id_count(inner, depth + 1)?
        }
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
        | ScalarType::Named(_)
        | ScalarType::Ref(_) => 0,
    };
    Ok(nested)
}

impl Value {
    pub fn source_text(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Float(value) => format!("{value:?}"),
            Self::Bool(value) => value.to_string(),
            Self::Text(value) => serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
            Self::Uuid(value) => format!("uuid \"{value}\""),
            Self::Decimal(value) => format!("decimal \"{value}\""),
            Self::Bytes(value) => format!("bytes \"{value}\""),
            Self::Date(value) => format!("@{value}"),
            Self::Timestamp(value) => format!("@{value}"),
            Self::Duration(value) => value.to_string(),
            Self::Null => "null".into(),
            Self::Named { value, .. } => value.source_text(),
            Self::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(name, value)| format!("{name} = {}", value.source_text()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Tuple(values) => format!(
                "({})",
                values
                    .iter()
                    .map(Self::source_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::List(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(Self::source_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Option(None) => "None".into(),
            Self::Option(Some(value)) => format!("Some ({})", value.source_text()),
            Self::Enum(value) if value.args.is_empty() => value.variant.clone(),
            Self::Enum(value)
                if value.args.len() == 1 && matches!(value.args[0], Self::Record(_)) =>
            {
                format!("{} {}", value.variant, value.args[0].source_text())
            }
            Self::Enum(value) => format!(
                "{}({})",
                value.variant,
                value
                    .args
                    .iter()
                    .map(Self::source_text)
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
