//! Stable JSON Lines protocol types. These types intentionally do not expose
//! the serde representation used by the in-memory or storage models.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::{QueryPlan, QueryResponse, ResponseColumn, SchemaInfo, UpsertAction};
use crate::error::Error;
use crate::idempotency::{
    IdempotencyDurability, IdempotencyPruneOptions, IdempotencyPruneResult, IdempotencyStatus,
    IdempotentExecution, validate_key as validate_idempotency_key,
};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<ReceiptOperation>,
}

impl Request {
    /// Build a versioned query request for TCP, HTTP, or an embedded adapter.
    pub fn query(request_id: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            version: VERSION,
            request_id: request_id.into(),
            query: query.into(),
            introspect: None,
            params: BTreeMap::new(),
            schema: None,
            idempotency_key: None,
            receipts: None,
        }
    }

    /// Add an application-native serde value as a typed query parameter.
    ///
    /// The source language still declares where the value is used (`$name`).
    /// The prepared binder supplies nominal type identity and validates the
    /// complete value before any row is scanned or mutation is committed.
    pub fn with_serde_param<T: Serialize + ?Sized>(
        mut self,
        name: impl Into<String>,
        value: &T,
    ) -> Result<Self, Error> {
        let value = Value::from_serde(value)?;
        self.params.insert(name.into(), WireValue::from(&value));
        Ok(self)
    }

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

    /// Attach a bounded idempotency key to a mutation request. The canonical
    /// digest is computed from the complete wire request by the executor.
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Result<Self, Error> {
        let key = key.into();
        validate_idempotency_key(&key)?;
        self.idempotency_key = Some(key);
        Ok(self)
    }

    pub fn canonical_digest(&self) -> Result<String, Error> {
        let document = CanonicalRequest {
            params: &self.params,
            query: &self.query,
            schema: self.schema.as_ref().map(|schema| CanonicalSchema {
                hash: &schema.hash,
                revision: schema.revision.to_string(),
            }),
            version: self.version.to_string(),
        };
        let encoded = serde_json::to_vec(&document).map_err(|error| {
            Error::new(
                "E_PROTOCOL",
                format!("encode canonical idempotency request: {error}"),
            )
        })?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }

    pub fn introspection(request_id: impl Into<String>, kind: IntrospectionKind) -> Self {
        Self {
            version: VERSION,
            request_id: request_id.into(),
            query: String::new(),
            introspect: Some(kind),
            params: BTreeMap::new(),
            schema: None,
            idempotency_key: None,
            receipts: None,
        }
    }

    pub fn receipt_status(request_id: impl Into<String>) -> Self {
        let mut request = Self::query(request_id, "");
        request.receipts = Some(ReceiptOperation::Status);
        request
    }

    pub fn receipt_prune(
        request_id: impl Into<String>,
        options: IdempotencyPruneOptions,
        confirm: bool,
    ) -> Self {
        let mut request = Self::query(request_id, "");
        request.receipts = Some(ReceiptOperation::Prune { options, confirm });
        request
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiptOperation {
    Status,
    Prune {
        options: IdempotencyPruneOptions,
        #[serde(default)]
        confirm: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", content = "result", rename_all = "snake_case")]
pub enum ReceiptOperationResult {
    Status(IdempotencyStatus),
    Prune(IdempotencyPruneResult),
}

#[derive(Serialize)]
struct CanonicalRequest<'a> {
    params: &'a BTreeMap<String, WireValue>,
    query: &'a str,
    schema: Option<CanonicalSchema<'a>>,
    version: String,
}

#[derive(Serialize)]
struct CanonicalSchema<'a> {
    hash: &'a str,
    revision: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upsert_actions: Vec<UpsertAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<QueryPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introspection: Option<Introspection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<IdempotencyMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<ReceiptOperationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdempotencyMetadata {
    pub key: String,
    pub digest: String,
    pub replayed: bool,
    pub committed_sequence: String,
    pub durability: IdempotencyDurability,
}

impl Response {
    /// Decode lossless protocol rows into application-native serde records.
    ///
    /// This is the network equivalent of `QueryResponse::typed_rows`: stable
    /// nominal IDs stay on the wire while Rust code receives ordinary structs
    /// and enums shaped like the source-language records and constructors.
    pub fn typed_rows<T: serde::de::DeserializeOwned>(&self) -> Result<Vec<T>, Error> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if !self.ok {
            return Err(Error::new(
                "E_QUERY",
                "cannot decode rows from a failed protocol response",
            ));
        }
        self.rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let fields = row
                    .iter()
                    .map(|(name, value)| {
                        Value::try_from(value.clone()).map(|value| (name.clone(), value))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()?;
                Value::Record(fields).to_serde().map_err(|error| {
                    Error::new(
                        error.code.as_str(),
                        format!(
                            "decode protocol result row {}: {}",
                            index + 1,
                            error.message
                        ),
                    )
                })
            })
            .collect()
    }

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
            upsert_actions: response.upsert_actions,
            plan: response.plan,
            introspection: None,
            idempotency: None,
            receipts: None,
        }
    }

    pub fn from_idempotent(
        request_id: impl Into<String>,
        key: impl Into<String>,
        result: IdempotentExecution,
    ) -> Self {
        let metadata = IdempotencyMetadata {
            key: key.into(),
            digest: result.digest,
            replayed: result.replayed,
            committed_sequence: result.committed_sequence.to_string(),
            durability: result.durability,
        };
        let mut response = Self::from_query(request_id, result.response);
        response.idempotency = Some(metadata);
        response
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
            upsert_actions: Vec::new(),
            plan: None,
            introspection: Some(introspection),
            idempotency: None,
            receipts: None,
        }
    }

    pub fn from_receipt_operation(
        request_id: impl Into<String>,
        result: ReceiptOperationResult,
        schema: SchemaInfo,
    ) -> Self {
        let mut response = Self::from_query(
            request_id,
            QueryResponse::ok_message("idempotency receipt operation complete"),
        );
        response.schema = Some(schema);
        response.receipts = Some(result);
        response
    }

    pub fn failure(request_id: impl Into<String>, error: Error, schema: SchemaInfo) -> Self {
        let mut response = QueryResponse::failure(error);
        response.schema = Some(schema);
        Self::from_query(request_id, response)
    }
}
