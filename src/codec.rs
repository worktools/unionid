//! Versioned, type-directed encoding for durable algebraic values.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::model::{Catalog, MAX_DEPTH, ScalarType, Value};

const MAGIC: &[u8; 4] = b"UIDV";
pub const VALUE_CODEC_VERSION: u16 = 1;
pub const PRODUCTION_VALUE_CODEC_VERSION: u16 = 2;
pub const MAX_VALUE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_COLLECTION_ITEMS: usize = 1_000_000;

pub fn encode_value(catalog: &Catalog, ty: &ScalarType, value: &Value) -> Result<Vec<u8>> {
    encode_value_version(catalog, ty, value, VALUE_CODEC_VERSION)
}

/// Encode a production-scalar value for storage-format-4 integration.
/// Existing storage callers continue to use the explicit legacy entry point.
pub fn encode_value_v2(catalog: &Catalog, ty: &ScalarType, value: &Value) -> Result<Vec<u8>> {
    encode_value_version(catalog, ty, value, PRODUCTION_VALUE_CODEC_VERSION)
}

fn encode_value_version(
    catalog: &Catalog,
    ty: &ScalarType,
    value: &Value,
    version: u16,
) -> Result<Vec<u8>> {
    if catalog.requires_protocol_v2(ty)? && version < PRODUCTION_VALUE_CODEC_VERSION {
        return Err(codec_error("production scalars require value codec 2"));
    }
    let value = catalog.coerce(value, ty, "value")?;
    let mut encoder = Encoder {
        catalog,
        bytes: Vec::new(),
    };
    encoder.bytes(MAGIC)?;
    encoder.u16(version)?;
    encoder.value(ty, &value, "value", 0)?;
    Ok(encoder.bytes)
}

pub fn decode_value(catalog: &Catalog, ty: &ScalarType, bytes: &[u8]) -> Result<Value> {
    if bytes.len() > MAX_VALUE_BYTES {
        return Err(codec_error(format!(
            "encoded value exceeds {MAX_VALUE_BYTES} byte limit"
        )));
    }
    let mut decoder = Decoder {
        catalog,
        bytes,
        pos: 0,
    };
    if decoder.take(MAGIC.len())? != MAGIC {
        return Err(codec_error("invalid value codec magic"));
    }
    let version = decoder.u16()?;
    if version != VALUE_CODEC_VERSION && version != PRODUCTION_VALUE_CODEC_VERSION {
        return Err(codec_error(format!(
            "unsupported value codec version {version}"
        )));
    }
    if catalog.requires_protocol_v2(ty)? && version < PRODUCTION_VALUE_CODEC_VERSION {
        return Err(codec_error("production scalars require value codec 2"));
    }
    let value = decoder.value(ty, "value", 0)?;
    if decoder.pos != bytes.len() {
        return Err(codec_error(format!(
            "{} trailing byte(s) after value",
            bytes.len() - decoder.pos
        )));
    }
    Ok(value)
}

fn codec_error(message: impl Into<String>) -> Error {
    Error::new("E_CODEC", message)
}

struct Encoder<'a> {
    catalog: &'a Catalog,
    bytes: Vec<u8>,
}

impl Encoder<'_> {
    fn value(&mut self, ty: &ScalarType, value: &Value, path: &str, depth: usize) -> Result<()> {
        check_depth(depth)?;
        match ty {
            ScalarType::Int => match value {
                Value::Int(value) => self.bytes(&value.to_be_bytes()),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Float => match value {
                Value::Float(value) if value.is_finite() => {
                    let value = if *value == 0.0 { 0.0 } else { *value };
                    self.bytes(&value.to_bits().to_be_bytes())
                }
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Bool => match value {
                Value::Bool(false) => self.u8(0),
                Value::Bool(true) => self.u8(1),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Uuid => match value {
                Value::Uuid(v) => self.bytes(v.as_bytes()),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Date => match value {
                Value::Date(v) => self.bytes(&v.epoch_days().to_be_bytes()),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Timestamp => match value {
                Value::Timestamp(v) => self.bytes(&v.epoch_microseconds().to_be_bytes()),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Duration => match value {
                Value::Duration(v) => self.bytes(&v.microseconds().to_be_bytes()),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Decimal { .. } => match value {
                Value::Decimal(v) => self.bytes(&v.coefficient().to_be_bytes()),
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Bytes => match value {
                Value::Bytes(v) => {
                    self.len(v.as_slice().len(), path)?;
                    self.bytes(v.as_slice())
                }
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Text => match value {
                Value::Text(value) => {
                    self.len(value.len(), path)?;
                    self.bytes(value.as_bytes())
                }
                _ => Err(type_mismatch(path)),
            },
            ScalarType::Ref(type_id) => {
                let Value::Named {
                    type_id: actual,
                    value,
                } = value
                else {
                    return Err(type_mismatch(path));
                };
                if actual != type_id {
                    return Err(codec_error(format!(
                        "{path}: expected type ID {type_id}, got {actual}"
                    )));
                }
                self.u64(*type_id)?;
                let definition = self.catalog.definition(*type_id)?;
                self.value(&definition.ty, value, path, depth + 1)
            }
            ScalarType::Record(columns) => {
                let Value::Record(fields) = value else {
                    return Err(type_mismatch(path));
                };
                self.len(columns.len(), path)?;
                let mut ordered = columns.iter().collect::<Vec<_>>();
                ordered.sort_by_key(|column| column.id);
                for column in ordered {
                    self.u64(column.id)?;
                    let field = fields.get(&column.name).ok_or_else(|| {
                        codec_error(format!("{path}: missing field '{}'", column.name))
                    })?;
                    self.value(
                        &column.ty,
                        field,
                        &format!("{path}.{}", column.name),
                        depth + 1,
                    )?;
                }
                Ok(())
            }
            ScalarType::Tuple(types) => {
                let Value::Tuple(values) = value else {
                    return Err(type_mismatch(path));
                };
                if values.len() != types.len() {
                    return Err(codec_error(format!(
                        "{path}: expected {} tuple item(s), got {}",
                        types.len(),
                        values.len()
                    )));
                }
                self.len(types.len(), path)?;
                for (index, (ty, value)) in types.iter().zip(values).enumerate() {
                    self.value(ty, value, &format!("{path}.{index}"), depth + 1)?;
                }
                Ok(())
            }
            ScalarType::Option(inner) => match value {
                Value::Option(None) => self.u8(0),
                Value::Option(Some(value)) => {
                    self.u8(1)?;
                    self.value(inner, value, path, depth + 1)
                }
                _ => Err(type_mismatch(path)),
            },
            ScalarType::List(inner) => {
                let Value::List(values) = value else {
                    return Err(type_mismatch(path));
                };
                check_items(values.len(), path)?;
                self.len(values.len(), path)?;
                for (index, value) in values.iter().enumerate() {
                    self.value(inner, value, &format!("{path}[{index}]"), depth + 1)?;
                }
                Ok(())
            }
            ScalarType::Enum(enum_type) => {
                let Value::Enum(value) = value else {
                    return Err(type_mismatch(path));
                };
                let variant = enum_type
                    .variants
                    .iter()
                    .find(|variant| variant.id == value.id)
                    .ok_or_else(|| {
                        codec_error(format!("{path}: unknown variant ID {}", value.id))
                    })?;
                if value.args.len() != variant.args.len() {
                    return Err(codec_error(format!(
                        "{path}: variant ID {} expects {} argument(s), got {}",
                        value.id,
                        variant.args.len(),
                        value.args.len()
                    )));
                }
                self.u64(value.id)?;
                self.len(value.args.len(), path)?;
                for (index, (ty, value)) in variant.args.iter().zip(&value.args).enumerate() {
                    self.value(
                        ty,
                        value,
                        &format!("{path}.{}[{index}]", variant.name),
                        depth + 1,
                    )?;
                }
                Ok(())
            }
            ScalarType::Named(name) => Err(codec_error(format!(
                "{path}: unresolved type name '{name}'"
            ))),
        }
    }

    fn len(&mut self, len: usize, path: &str) -> Result<()> {
        let len = u32::try_from(len)
            .map_err(|_| codec_error(format!("{path}: item or byte length exceeds u32")))?;
        self.bytes(&len.to_be_bytes())
    }

    fn u8(&mut self, value: u8) -> Result<()> {
        self.bytes(&[value])
    }

    fn u16(&mut self, value: u16) -> Result<()> {
        self.bytes(&value.to_be_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<()> {
        self.bytes(&value.to_be_bytes())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_VALUE_BYTES {
            return Err(codec_error(format!(
                "encoded value exceeds {MAX_VALUE_BYTES} byte limit"
            )));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

struct Decoder<'a> {
    catalog: &'a Catalog,
    bytes: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn value(&mut self, ty: &ScalarType, path: &str, depth: usize) -> Result<Value> {
        check_depth(depth)?;
        Ok(match ty {
            ScalarType::Int => Value::Int(i64::from_be_bytes(self.array()?)),
            ScalarType::Float => {
                let value = f64::from_bits(u64::from_be_bytes(self.array()?));
                if !value.is_finite() {
                    return Err(codec_error(format!("{path}: float must be finite")));
                }
                Value::Float(if value == 0.0 { 0.0 } else { value })
            }
            ScalarType::Bool => match self.u8()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                tag => return Err(codec_error(format!("{path}: invalid bool tag {tag}"))),
            },
            ScalarType::Uuid => Value::Uuid(crate::scalars::Uuid::from_bytes(self.array()?)),
            ScalarType::Date => Value::Date(
                crate::scalars::Date::from_epoch_days(i32::from_be_bytes(self.array()?))
                    .map_err(|e| codec_error(e.to_string()))?,
            ),
            ScalarType::Timestamp => Value::Timestamp(
                crate::scalars::Timestamp::from_epoch_microseconds(i64::from_be_bytes(
                    self.array()?,
                ))
                .map_err(|e| codec_error(e.to_string()))?,
            ),
            ScalarType::Duration => Value::Duration(crate::scalars::Duration::from_microseconds(
                i64::from_be_bytes(self.array()?),
            )),
            ScalarType::Decimal { precision, scale } => Value::Decimal(
                crate::scalars::Decimal::new(
                    i128::from_be_bytes(self.array()?),
                    *precision,
                    *scale,
                )
                .map_err(|e| codec_error(e.to_string()))?,
            ),
            ScalarType::Bytes => {
                let len = self.length()?;
                Value::Bytes(
                    crate::scalars::Bytes::new(self.take(len)?.to_vec())
                        .map_err(|e| codec_error(e.to_string()))?,
                )
            }
            ScalarType::Text => {
                let len = self.length()?;
                let text = std::str::from_utf8(self.take(len)?)
                    .map_err(|error| codec_error(format!("{path}: invalid UTF-8 text: {error}")))?;
                Value::Text(text.into())
            }
            ScalarType::Ref(type_id) => {
                let actual = self.u64()?;
                if actual != *type_id {
                    return Err(codec_error(format!(
                        "{path}: expected type ID {type_id}, got {actual}"
                    )));
                }
                let definition = self.catalog.definition(*type_id)?;
                Value::Named {
                    type_id: *type_id,
                    value: Box::new(self.value(&definition.ty, path, depth + 1)?),
                }
            }
            ScalarType::Record(columns) => {
                let count = self.length()?;
                if count != columns.len() {
                    return Err(codec_error(format!(
                        "{path}: encoded record has {count} field(s), schema expects {}",
                        columns.len()
                    )));
                }
                let mut fields = BTreeMap::new();
                let mut seen = BTreeSet::new();
                for _ in 0..count {
                    let field_id = self.u64()?;
                    if !seen.insert(field_id) {
                        return Err(codec_error(format!(
                            "{path}: duplicate field ID {field_id}"
                        )));
                    }
                    let column = columns
                        .iter()
                        .find(|column| column.id == field_id)
                        .ok_or_else(|| {
                            codec_error(format!("{path}: unknown field ID {field_id}"))
                        })?;
                    fields.insert(
                        column.name.clone(),
                        self.value(&column.ty, &format!("{path}.{}", column.name), depth + 1)?,
                    );
                }
                Value::Record(fields)
            }
            ScalarType::Tuple(types) => {
                let count = self.length()?;
                if count != types.len() {
                    return Err(codec_error(format!(
                        "{path}: encoded tuple has {count} item(s), schema expects {}",
                        types.len()
                    )));
                }
                Value::Tuple(
                    types
                        .iter()
                        .enumerate()
                        .map(|(index, ty)| self.value(ty, &format!("{path}.{index}"), depth + 1))
                        .collect::<Result<_>>()?,
                )
            }
            ScalarType::Option(inner) => match self.u8()? {
                0 => Value::Option(None),
                1 => Value::Option(Some(Box::new(self.value(inner, path, depth + 1)?))),
                tag => return Err(codec_error(format!("{path}: invalid option tag {tag}"))),
            },
            ScalarType::List(inner) => {
                let count = self.length()?;
                check_items(count, path)?;
                if count > self.remaining() {
                    return Err(codec_error(format!(
                        "{path}: list count exceeds remaining encoded bytes"
                    )));
                }
                let mut values = Vec::with_capacity(count.min(1024));
                for index in 0..count {
                    values.push(self.value(inner, &format!("{path}[{index}]"), depth + 1)?);
                }
                Value::List(values)
            }
            ScalarType::Enum(enum_type) => {
                let variant_id = self.u64()?;
                let variant = enum_type
                    .variants
                    .iter()
                    .find(|variant| variant.id == variant_id)
                    .ok_or_else(|| {
                        codec_error(format!("{path}: unknown variant ID {variant_id}"))
                    })?;
                let count = self.length()?;
                if count != variant.args.len() {
                    return Err(codec_error(format!(
                        "{path}: variant ID {variant_id} has {count} argument(s), schema expects {}",
                        variant.args.len()
                    )));
                }
                let args = variant
                    .args
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| {
                        self.value(ty, &format!("{path}.{}[{index}]", variant.name), depth + 1)
                    })
                    .collect::<Result<_>>()?;
                Value::Enum(crate::model::EnumValue {
                    variant: variant.name.clone(),
                    args,
                    id: variant_id,
                })
            }
            ScalarType::Named(name) => {
                return Err(codec_error(format!(
                    "{path}: unresolved type name '{name}'"
                )));
            }
        })
    }

    fn length(&mut self) -> Result<usize> {
        Ok(u32::from_be_bytes(self.array()?) as usize)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| codec_error("truncated encoded value"))
    }

    fn take(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| codec_error("truncated encoded value"))?;
        let value = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(value)
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        Err(codec_error(format!(
            "value nesting exceeds {MAX_DEPTH} levels"
        )))
    } else {
        Ok(())
    }
}

fn check_items(items: usize, path: &str) -> Result<()> {
    if items > MAX_COLLECTION_ITEMS {
        Err(codec_error(format!(
            "{path}: collection exceeds {MAX_COLLECTION_ITEMS} item limit"
        )))
    } else {
        Ok(())
    }
}

fn type_mismatch(path: &str) -> Error {
    codec_error(format!("{path}: typed value does not match its schema"))
}
