//! Stable JSON Lines protocol types. These types intentionally do not expose
//! the serde representation used by the in-memory or storage models.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::db::{QueryPlan, QueryResponse, ResponseColumn, SchemaInfo, UpsertAction};
use crate::error::Error;
use crate::introspection::{Introspection, IntrospectionKind};
use crate::model::{EnumValue, Value};

pub const VERSION: u32 = 1;
pub const MAX_INTROSPECTION_BYTES: usize = 1024 * 1024;
pub const MAX_REQUEST_ID_BYTES: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introspect: Option<IntrospectionKind>,
    #[serde(default)]
    pub params: BTreeMap<String, WireValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaInfo>,
}

impl Request {
    pub fn decode_params(&self) -> Result<BTreeMap<String, Value>, Error> {
        self.params
            .iter()
            .map(|(name, value)| {
                Value::try_from(value.clone())
                    .map(|value| (name.clone(), value))
                    .map_err(|error| {
                        Error::new(
                            "E_PARAM_TYPE",
                            format!("invalid parameter '${name}': {}", error.message),
                        )
                    })
            })
            .collect()
    }

    pub fn introspection(request_id: impl Into<String>, kind: IntrospectionKind) -> Self {
        Self {
            version: VERSION,
            request_id: request_id.into(),
            query: String::new(),
            introspect: Some(kind),
            params: BTreeMap::new(),
            schema: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireValue {
    Int {
        value: String,
    },
    Float {
        value: String,
    },
    Bool {
        value: bool,
    },
    Text {
        value: String,
    },
    Null,
    Variant {
        name: String,
        variant_id: String,
        args: Vec<WireValue>,
    },
    Record {
        fields: BTreeMap<String, WireValue>,
    },
    Tuple {
        items: Vec<WireValue>,
    },
    List {
        items: Vec<WireValue>,
    },
    Option {
        value: Option<Box<WireValue>>,
    },
    Named {
        type_id: String,
        value: Box<WireValue>,
    },
}

impl From<&Value> for WireValue {
    fn from(value: &Value) -> Self {
        match value {
            Value::Int(value) => Self::Int {
                value: value.to_string(),
            },
            Value::Float(value) => Self::Float {
                value: value.to_string(),
            },
            Value::Bool(value) => Self::Bool { value: *value },
            Value::Text(value) => Self::Text {
                value: value.clone(),
            },
            Value::Null => Self::Null,
            Value::Enum(value) => Self::Variant {
                name: value.variant.clone(),
                variant_id: value.id.to_string(),
                args: value.args.iter().map(Self::from).collect(),
            },
            Value::Record(fields) => Self::Record {
                fields: fields
                    .iter()
                    .map(|(name, value)| (name.clone(), Self::from(value)))
                    .collect(),
            },
            Value::Tuple(items) => Self::Tuple {
                items: items.iter().map(Self::from).collect(),
            },
            Value::List(items) => Self::List {
                items: items.iter().map(Self::from).collect(),
            },
            Value::Option(value) => Self::Option {
                value: value.as_deref().map(Self::from).map(Box::new),
            },
            Value::Named { type_id, value } => Self::Named {
                type_id: type_id.to_string(),
                value: Box::new(Self::from(value.as_ref())),
            },
        }
    }
}

impl TryFrom<WireValue> for Value {
    type Error = Error;

    fn try_from(value: WireValue) -> Result<Self, Self::Error> {
        Ok(match value {
            WireValue::Int { value } => Value::Int(value.parse().map_err(|_| {
                Error::new("E_PARAM_TYPE", "int value must be a base-10 i64 string")
            })?),
            WireValue::Float { value } => {
                let value: f64 = value.parse().map_err(|_| {
                    Error::new("E_PARAM_TYPE", "float value must be a decimal f64 string")
                })?;
                if !value.is_finite() {
                    return Err(Error::new("E_PARAM_TYPE", "float value must be finite"));
                }
                Value::Float(if value == 0.0 { 0.0 } else { value })
            }
            WireValue::Bool { value } => Value::Bool(value),
            WireValue::Text { value } => Value::Text(value),
            WireValue::Null => Value::Null,
            WireValue::Variant {
                name,
                variant_id,
                args,
            } => Value::Enum(EnumValue {
                variant: name,
                id: parse_id(&variant_id, "variant_id")?,
                args: decode_values(args)?,
            }),
            WireValue::Record { fields } => Value::Record(
                fields
                    .into_iter()
                    .map(|(name, value)| Value::try_from(value).map(|value| (name, value)))
                    .collect::<Result<_, _>>()?,
            ),
            WireValue::Tuple { items } => Value::Tuple(decode_values(items)?),
            WireValue::List { items } => Value::List(decode_values(items)?),
            WireValue::Option { value } => Value::Option(
                value
                    .map(|value| Value::try_from(*value))
                    .transpose()?
                    .map(Box::new),
            ),
            WireValue::Named { type_id, value } => Value::Named {
                type_id: parse_id(&type_id, "type_id")?,
                value: Box::new(Value::try_from(*value)?),
            },
        })
    }
}

fn decode_values(values: Vec<WireValue>) -> Result<Vec<Value>, Error> {
    values.into_iter().map(Value::try_from).collect()
}

fn parse_id(value: &str, field: &str) -> Result<u64, Error> {
    value.parse().map_err(|_| {
        Error::new(
            "E_PARAM_TYPE",
            format!("{field} must be a base-10 u64 string"),
        )
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub request_id: String,
    pub ok: bool,
    pub message: String,
    pub columns: Vec<ResponseColumn>,
    pub rows: Vec<BTreeMap<String, WireValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upsert_action: Option<UpsertAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<QueryPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introspection: Option<Introspection>,
}

impl Response {
    pub fn from_query(request_id: impl Into<String>, response: QueryResponse) -> Self {
        Self {
            version: VERSION,
            request_id: request_id.into(),
            ok: response.ok,
            message: response.message,
            columns: response.columns,
            rows: response
                .rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|(name, value)| (name, WireValue::from(&value)))
                        .collect()
                })
                .collect(),
            error: response.error,
            warnings: response.warnings,
            schema: response.schema,
            affected_rows: response.affected_rows,
            upsert_action: response.upsert_action,
            plan: response.plan,
            introspection: None,
        }
    }

    pub fn from_introspection(request_id: impl Into<String>, introspection: Introspection) -> Self {
        Self {
            version: VERSION,
            request_id: request_id.into(),
            ok: true,
            message: "introspection complete".into(),
            columns: Vec::new(),
            rows: Vec::new(),
            error: None,
            warnings: Vec::new(),
            schema: Some(introspection.schema.clone()),
            affected_rows: None,
            upsert_action: None,
            plan: None,
            introspection: Some(introspection),
        }
    }

    pub fn failure(request_id: impl Into<String>, error: Error, schema: SchemaInfo) -> Self {
        let mut response = QueryResponse::failure(error);
        response.schema = Some(schema);
        Self::from_query(request_id, response)
    }
}
