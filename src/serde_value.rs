//! Serde adapters for application-native structs and algebraic data types.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
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

impl Value {
    /// Encode a Rust serde value while preserving its algebraic shape.
    pub fn from_serde<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        value.serialize(ValueSerializer)
    }

    /// Decode a typed database value into an application serde type.
    pub fn to_serde<T: DeserializeOwned>(&self) -> Result<T> {
        let json = json_value(self, 0)?;
        serde_json::from_value(json)
            .map_err(|error| Error::new("E_SERDE", format!("decode typed value: {error}")))
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
        _name: &'static str,
        value: &T,
    ) -> Result<Value> {
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

fn json_value(value: &Value, depth: usize) -> Result<serde_json::Value> {
    if depth >= MAX_DEPTH {
        return Err(Error::new(
            "E_LIMIT",
            format!("serde value depth exceeds {MAX_DEPTH}"),
        ));
    }
    Ok(match value {
        Value::Int(value) => serde_json::Value::Number((*value).into()),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| Error::new("E_SERDE", "cannot decode a non-finite float"))?,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Text(value) => serde_json::Value::String(value.clone()),
        Value::Null | Value::Option(None) => serde_json::Value::Null,
        Value::Named { value, .. } => json_value(value, depth + 1)?,
        Value::Option(Some(value)) => json_value(value, depth + 1)?,
        Value::Record(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(name, value)| Ok((name.clone(), json_value(value, depth + 1)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Tuple(items) | Value::List(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|value| json_value(value, depth + 1))
                .collect::<Result<_>>()?,
        ),
        Value::Enum(value) if value.args.is_empty() => {
            serde_json::Value::String(value.variant.clone())
        }
        Value::Enum(value) => {
            let payload = if value.args.len() == 1 {
                json_value(&value.args[0], depth + 1)?
            } else {
                serde_json::Value::Array(
                    value
                        .args
                        .iter()
                        .map(|value| json_value(value, depth + 1))
                        .collect::<Result<_>>()?,
                )
            };
            serde_json::Value::Object([(value.variant.clone(), payload)].into_iter().collect())
        }
    })
}
