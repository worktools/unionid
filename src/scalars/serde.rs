//! Versioned newtype markers distinguish scalars from ordinary application text.
//! JSON intentionally erases the marker; a typed deserializer restores it while
//! validating the canonical payload. These are not protocol-v2 envelopes.

use std::{fmt, marker::PhantomData};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::{Bytes, Date, Decimal, Duration, Timestamp, Uuid, literal};
use crate::error::Result;

trait ScalarRepr: Sized {
    const MARKER: &'static str;
    type Payload: Serialize + de::DeserializeOwned;
    fn payload(&self) -> Self::Payload;
    fn from_payload(payload: Self::Payload) -> Result<Self>;
}

struct ScalarVisitor<T>(PhantomData<T>);

impl<'de, T: ScalarRepr> de::Visitor<'de> for ScalarVisitor<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "canonical {} payload", T::MARKER)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<T, D::Error> {
        T::from_payload(T::Payload::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

macro_rules! scalar_serde {
    ($($ty:ty),+ $(,)?) => {$(
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                serializer.serialize_newtype_struct(Self::MARKER, &self.payload())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                deserializer.deserialize_newtype_struct(Self::MARKER, ScalarVisitor(PhantomData))
            }
        }
    )+};
}

macro_rules! text_repr {
    ($ty:ty, $marker:literal) => {
        impl ScalarRepr for $ty {
            const MARKER: &'static str = $marker;
            type Payload = String;

            fn payload(&self) -> String {
                self.to_string()
            }

            fn from_payload(payload: String) -> Result<Self> {
                let value: Self = payload.parse()?;
                if value.to_string() != payload {
                    return Err(literal("expected canonical scalar text"));
                }
                Ok(value)
            }
        }
    };
}

text_repr!(Uuid, "unionid::scalar::v1::uuid");
text_repr!(Date, "unionid::scalar::v1::date");
text_repr!(Timestamp, "unionid::scalar::v1::timestamp");

impl ScalarRepr for Duration {
    const MARKER: &'static str = "unionid::scalar::v1::duration";
    type Payload = String;

    fn payload(&self) -> String {
        self.microseconds().to_string()
    }

    fn from_payload(payload: String) -> Result<Self> {
        let value: i64 = payload
            .parse()
            .map_err(|_| literal("invalid duration microseconds"))?;
        if value.to_string() != payload {
            return Err(literal("expected canonical duration microseconds"));
        }
        Ok(Self::from_microseconds(value))
    }
}

impl ScalarRepr for Bytes {
    const MARKER: &'static str = "unionid::scalar::v1::bytes";
    type Payload = String;

    fn payload(&self) -> String {
        self.to_base64url()
    }

    fn from_payload(payload: String) -> Result<Self> {
        Self::from_base64url(&payload)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecimalPayload {
    coefficient: String,
    scale: u8,
}

impl ScalarRepr for Decimal {
    const MARKER: &'static str = "unionid::scalar::v1::decimal";
    type Payload = DecimalPayload;

    fn payload(&self) -> DecimalPayload {
        DecimalPayload {
            coefficient: self.coefficient().to_string(),
            scale: self.scale(),
        }
    }

    fn from_payload(payload: DecimalPayload) -> Result<Self> {
        let coefficient: i128 = payload
            .coefficient
            .parse()
            .map_err(|_| literal("invalid decimal coefficient"))?;
        if coefficient.to_string() != payload.coefficient {
            return Err(literal("expected canonical decimal coefficient"));
        }
        Self::new(coefficient, 38, payload.scale)
    }
}

scalar_serde!(Uuid, Date, Timestamp, Duration, Decimal, Bytes);

pub(crate) fn decode_marker(name: &str, payload: serde_json::Value) -> Result<crate::Value> {
    use crate::{Error, Value};
    match name {
        Uuid::MARKER => serde_json::from_value::<Uuid>(payload).map(Value::Uuid),
        Date::MARKER => serde_json::from_value::<Date>(payload).map(Value::Date),
        Timestamp::MARKER => serde_json::from_value::<Timestamp>(payload).map(Value::Timestamp),
        Duration::MARKER => serde_json::from_value::<Duration>(payload).map(Value::Duration),
        Decimal::MARKER => serde_json::from_value::<Decimal>(payload).map(Value::Decimal),
        Bytes::MARKER => serde_json::from_value::<Bytes>(payload).map(Value::Bytes),
        _ => {
            return Err(Error::new(
                "E_SERDE",
                format!("unknown scalar marker '{name}'"),
            ));
        }
    }
    .map_err(|error| Error::new("E_SERDE", error.to_string()))
}
