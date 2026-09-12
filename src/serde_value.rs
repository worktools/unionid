//! Serde adapters for application-native structs and algebraic data types.

use std::collections::BTreeMap;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
};
use serde::{Serialize, Serializer};

use crate::error::{Error, Result};
use crate::model::{EnumValue, MAX_DEPTH, Value};

impl serde::ser::Error for Error {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self::new("E_SERDE", message.to_string())
    }
}

impl serde::de::Error for Error {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self::new("E_SERDE", message.to_string())
    }
}

impl Value {
    /// Encode a Rust serde value while preserving its algebraic shape.
    pub fn from_serde<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        value.serialize(ValueSerializer)
    }

    /// Decode a typed database value into an application serde type.
    pub fn to_serde<T: DeserializeOwned>(&self) -> Result<T> {
        T::deserialize(ValueDeserializer {
            value: self,
            depth: 0,
        })
        .map_err(|mut error| {
            error.message = format!("decode typed value: {}", error.message);
            error
        })
    }
}

struct ValueSerializer;

impl Serializer for ValueSerializer {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = Sequence;
    type SerializeTuple = Tuple;
    type SerializeTupleStruct = Tuple;
    type SerializeTupleVariant = TupleVariant;
    type SerializeMap = Record;
    type SerializeStruct = Record;
    type SerializeStructVariant = StructVariant;

    fn serialize_bool(self, value: bool) -> Result<Value> {
        Ok(Value::Bool(value))
    }

    fn serialize_i8(self, value: i8) -> Result<Value> {
        self.serialize_i64(value.into())
    }

    fn serialize_i16(self, value: i16) -> Result<Value> {
        self.serialize_i64(value.into())
    }

    fn serialize_i32(self, value: i32) -> Result<Value> {
        self.serialize_i64(value.into())
    }

    fn serialize_i64(self, value: i64) -> Result<Value> {
        Ok(Value::Int(value))
    }

    fn serialize_u8(self, value: u8) -> Result<Value> {
        self.serialize_u64(value.into())
    }

    fn serialize_u16(self, value: u16) -> Result<Value> {
        self.serialize_u64(value.into())
    }

    fn serialize_u32(self, value: u32) -> Result<Value> {
        self.serialize_u64(value.into())
    }

    fn serialize_u64(self, value: u64) -> Result<Value> {
        i64::try_from(value).map(Value::Int).map_err(|_| {
            Error::new(
                "E_SERDE",
                format!("unsigned integer {value} exceeds unionid int range"),
            )
        })
    }

    fn serialize_f32(self, value: f32) -> Result<Value> {
        self.serialize_f64(value.into())
    }

    fn serialize_f64(self, value: f64) -> Result<Value> {
        if value.is_finite() {
            Ok(Value::Float(value))
        } else {
            Err(Error::new("E_SERDE", "non-finite floats cannot be stored"))
        }
    }

    fn serialize_char(self, value: char) -> Result<Value> {
        Ok(Value::Text(value.to_string()))
    }

    fn serialize_str(self, value: &str) -> Result<Value> {
        Ok(Value::Text(value.into()))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Value> {
        Err(Error::new(
            "E_SERDE",
            "byte buffers have no unionid scalar type; use a list or text",
        ))
    }

    fn serialize_none(self) -> Result<Value> {
        Ok(Value::Option(None))
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value> {
        Ok(Value::Option(Some(Box::new(value.serialize(self)?))))
    }

    fn serialize_unit(self) -> Result<Value> {
        Ok(Value::Tuple(Vec::new()))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Value> {
        Ok(enum_value(variant, Vec::new()))
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Value> {
        if name.starts_with("unionid::scalar::") {
            let payload = serde_json::to_value(value)
                .map_err(|error| Error::new("E_SERDE", error.to_string()))?;
            return crate::scalars::decode_marker(name, payload);
        }
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value> {
        Ok(enum_value(variant, vec![value.serialize(self)?]))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Sequence> {
        Ok(Sequence(Vec::with_capacity(len.unwrap_or(0))))
    }

    fn serialize_tuple(self, len: usize) -> Result<Tuple> {
        Ok(Tuple(Vec::with_capacity(len)))
    }

    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<Tuple> {
        self.serialize_tuple(len)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<TupleVariant> {
        Ok(TupleVariant {
            variant,
            values: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Record> {
        Ok(Record::new(len.unwrap_or(0)))
    }

    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Record> {
        Ok(Record::new(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<StructVariant> {
        Ok(StructVariant {
            variant,
            fields: BTreeMap::new(),
            _expected: len,
        })
    }
}

struct Sequence(Vec<Value>);

impl SerializeSeq for Sequence {
    type Ok = Value;
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.0.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::List(self.0))
    }
}

struct Tuple(Vec<Value>);

impl SerializeTuple for Tuple {
    type Ok = Value;
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.0.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(Value::Tuple(self.0))
    }
}

impl SerializeTupleStruct for Tuple {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        SerializeTuple::serialize_element(self, value)
    }

    fn end(self) -> Result<Value> {
        SerializeTuple::end(self)
    }
}

struct TupleVariant {
    variant: &'static str,
    values: Vec<Value>,
}

impl SerializeTupleVariant for TupleVariant {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.values.push(value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(enum_value(self.variant, self.values))
    }
}

struct Record {
    fields: BTreeMap<String, Value>,
    next_key: Option<String>,
}

impl Record {
    fn new(_expected: usize) -> Self {
        Self {
            fields: BTreeMap::new(),
            next_key: None,
        }
    }

    fn insert<T: Serialize + ?Sized>(&mut self, key: String, value: &T) -> Result<()> {
        if self
            .fields
            .insert(key.clone(), value.serialize(ValueSerializer)?)
            .is_some()
        {
            return Err(Error::new(
                "E_SERDE",
                format!("duplicate record field '{key}'"),
            ));
        }
        Ok(())
    }
}

impl SerializeMap for Record {
    type Ok = Value;
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<()> {
        if self.next_key.is_some() {
            return Err(Error::new("E_SERDE", "map value is missing"));
        }
        self.next_key = Some(match serde_json::to_value(key) {
            Ok(serde_json::Value::String(key)) => key,
            Ok(_) => return Err(unsupported_key()),
            Err(error) => {
                return Err(Error::new("E_SERDE", format!("encode record key: {error}")));
            }
        });
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let key = self
            .next_key
            .take()
            .ok_or_else(|| Error::new("E_SERDE", "map key is missing"))?;
        self.insert(key, value)
    }

    fn end(self) -> Result<Value> {
        if self.next_key.is_some() {
            return Err(Error::new("E_SERDE", "map value is missing"));
        }
        Ok(Value::Record(self.fields))
    }
}

impl SerializeStruct for Record {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.insert(key.into(), value)
    }

    fn end(self) -> Result<Value> {
        Ok(Value::Record(self.fields))
    }
}

struct StructVariant {
    variant: &'static str,
    fields: BTreeMap<String, Value>,
    _expected: usize,
}

impl SerializeStructVariant for StructVariant {
    type Ok = Value;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        if self
            .fields
            .insert(key.into(), value.serialize(ValueSerializer)?)
            .is_some()
        {
            return Err(Error::new(
                "E_SERDE",
                format!("duplicate variant field '{key}'"),
            ));
        }
        Ok(())
    }

    fn end(self) -> Result<Value> {
        Ok(enum_value(self.variant, vec![Value::Record(self.fields)]))
    }
}

fn unsupported_key() -> Error {
    Error::new("E_SERDE", "unionid record maps require text keys")
}

fn enum_value(variant: &str, args: Vec<Value>) -> Value {
    Value::Enum(EnumValue {
        variant: variant.into(),
        args,
        id: 0,
    })
}

#[derive(Clone, Copy)]
struct ValueDeserializer<'a> {
    value: &'a Value,
    depth: usize,
}

impl ValueDeserializer<'_> {
    fn check(self) -> Result<Self> {
        if self.depth >= MAX_DEPTH {
            Err(Error::new(
                "E_LIMIT",
                format!("serde value depth exceeds {MAX_DEPTH}"),
            ))
        } else {
            Ok(self)
        }
    }

    fn child<'a>(self, value: &'a Value) -> ValueDeserializer<'a> {
        ValueDeserializer {
            value,
            depth: self.depth + 1,
        }
    }
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let this = self.check()?;
        match this.value {
            Value::Int(value) => visitor.visit_i64(*value),
            Value::Float(value) => visitor.visit_f64(*value),
            Value::Bool(value) => visitor.visit_bool(*value),
            Value::Text(value) => visitor.visit_borrowed_str(value),
            Value::Null => visitor.visit_unit(),
            Value::Option(None) => visitor.visit_none(),
            Value::Named { value, .. } | Value::Option(Some(value)) => {
                this.child(value).deserialize_any(visitor)
            }
            Value::Record(fields) => visitor.visit_map(ValueMapAccess::new(fields, this.depth)),
            Value::Tuple(items) | Value::List(items) => {
                visitor.visit_seq(ValueSeqAccess::new(items, this.depth))
            }
            Value::Enum(value) => visitor.visit_enum(ValueEnumAccess {
                value,
                depth: this.depth,
            }),
            Value::Uuid(value) => visitor.visit_string(value.to_string()),
            Value::Date(value) => visitor.visit_string(value.to_string()),
            Value::Timestamp(value) => visitor.visit_string(value.to_string()),
            Value::Duration(value) => visitor.visit_string(value.microseconds().to_string()),
            Value::Decimal(_) | Value::Bytes(_) => native_scalar_json(this.value)?
                .into_deserializer()
                .deserialize_any(visitor)
                .map_err(de::Error::custom),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let this = self.check()?;
        match this.value {
            Value::Null | Value::Option(None) => visitor.visit_none(),
            Value::Option(Some(value)) => visitor.visit_some(this.child(value)),
            Value::Named { value, .. } => this.child(value).deserialize_option(visitor),
            value => visitor.visit_some(this.child(value)),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        let this = self.check()?;
        if name.starts_with("unionid::scalar::") {
            let payload = native_scalar_json(this.value)?;
            return visitor
                .visit_newtype_struct(payload.into_deserializer())
                .map_err(de::Error::custom);
        }
        match this.value {
            Value::Named { value, .. } => visitor.visit_newtype_struct(this.child(value)),
            value => visitor.visit_newtype_struct(this.child(value)),
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let this = self.check()?;
        match this.value {
            Value::Named { value, .. } => this.child(value).deserialize_seq(visitor),
            Value::Tuple(items) | Value::List(items) => {
                visitor.visit_seq(ValueSeqAccess::new(items, this.depth))
            }
            _ => Err(Error::new("E_SERDE", "expected tuple or list")),
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let this = self.check()?;
        match this.value {
            Value::Named { value, .. } => this.child(value).deserialize_map(visitor),
            Value::Record(fields) => visitor.visit_map(ValueMapAccess::new(fields, this.depth)),
            _ => Err(Error::new("E_SERDE", "expected record")),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let this = self.check()?;
        match this.value {
            Value::Named { value, .. } => this
                .child(value)
                .deserialize_enum(_name, _variants, visitor),
            Value::Enum(value) => visitor.visit_enum(ValueEnumAccess {
                value,
                depth: this.depth,
            }),
            _ => Err(Error::new("E_SERDE", "expected sum value")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let this = self.check()?;
        match this.value {
            Value::Null => visitor.visit_unit(),
            Value::Tuple(items) if items.is_empty() => visitor.visit_unit(),
            Value::Named { value, .. } => this.child(value).deserialize_unit(visitor),
            _ => Err(Error::new("E_SERDE", "expected unit value")),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        identifier ignored_any
    }
}

struct ValueSeqAccess<'a> {
    items: std::slice::Iter<'a, Value>,
    depth: usize,
}

impl<'a> ValueSeqAccess<'a> {
    fn new(items: &'a [Value], depth: usize) -> Self {
        Self {
            items: items.iter(),
            depth,
        }
    }
}

impl<'de> SeqAccess<'de> for ValueSeqAccess<'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        self.items
            .next()
            .map(|value| {
                seed.deserialize(ValueDeserializer {
                    value,
                    depth: self.depth + 1,
                })
            })
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

struct ValueMapAccess<'a> {
    entries: std::collections::btree_map::Iter<'a, String, Value>,
    value: Option<&'a Value>,
    depth: usize,
}

impl<'a> ValueMapAccess<'a> {
    fn new(fields: &'a BTreeMap<String, Value>, depth: usize) -> Self {
        Self {
            entries: fields.iter(),
            value: None,
            depth,
        }
    }
}

impl<'de> MapAccess<'de> for ValueMapAccess<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        seed.deserialize(key.as_str().into_deserializer()).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let value = self
            .value
            .take()
            .ok_or_else(|| Error::new("E_SERDE", "record value is missing"))?;
        seed.deserialize(ValueDeserializer {
            value,
            depth: self.depth + 1,
        })
    }
}

struct ValueEnumAccess<'a> {
    value: &'a EnumValue,
    depth: usize,
}

impl<'de> EnumAccess<'de> for ValueEnumAccess<'de> {
    type Error = Error;
    type Variant = ValueVariantAccess<'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let variant = seed.deserialize(self.value.variant.as_str().into_deserializer())?;
        Ok((
            variant,
            ValueVariantAccess {
                args: &self.value.args,
                depth: self.depth,
            },
        ))
    }
}

struct ValueVariantAccess<'a> {
    args: &'a [Value],
    depth: usize,
}

impl<'de> VariantAccess<'de> for ValueVariantAccess<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        if self.args.is_empty() {
            Ok(())
        } else {
            Err(Error::new("E_SERDE", "unit variant has a payload"))
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        let [value] = self.args else {
            return Err(Error::new("E_SERDE", "expected one variant payload"));
        };
        seed.deserialize(ValueDeserializer {
            value,
            depth: self.depth + 1,
        })
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        visitor.visit_seq(ValueSeqAccess::new(self.args, self.depth))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let [Value::Record(fields)] = self.args else {
            return Err(Error::new("E_SERDE", "expected record variant payload"));
        };
        visitor.visit_map(ValueMapAccess::new(fields, self.depth + 1))
    }
}

fn native_scalar_json(value: &Value) -> Result<serde_json::Value> {
    match value {
        Value::Uuid(value) => serde_json::to_value(value),
        Value::Date(value) => serde_json::to_value(value),
        Value::Timestamp(value) => serde_json::to_value(value),
        Value::Duration(value) => serde_json::to_value(value),
        Value::Decimal(value) => serde_json::to_value(value),
        Value::Bytes(value) => serde_json::to_value(value),
        _ => return Err(Error::new("E_SERDE", "expected native scalar value")),
    }
    .map_err(|error| Error::new("E_SERDE", error.to_string()))
}
