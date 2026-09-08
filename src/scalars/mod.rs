//! Exact scalar value domains for the production-scalar compatibility upgrade.
//!
//! These wrappers validate logical values. Database and protocol integration is
//! delivered separately; constructing a wrapper does not upgrade a database.
mod decimal;
mod serde;
pub(crate) use serde::decode_marker;
mod temporal;

pub use decimal::Decimal;
pub(crate) use decimal::validate_type as validate_decimal_type;
pub use temporal::{Date, Duration, Timestamp};

use crate::error::{Error, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::fmt;
use std::str::FromStr;

pub const MAX_BYTES: usize = 16 * 1024 * 1024;

fn literal(message: &str) -> Error {
    Error::new("E_SCALAR_LITERAL", message)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(literal("expected hexadecimal digits")),
    }
}

fn decode_hex(source: &str) -> Result<Vec<u8>> {
    if source.len() > MAX_BYTES * 2 || !source.len().is_multiple_of(2) {
        return Err(literal(
            "hex bytes must have even length and at most 16 MiB",
        ));
    }
    source
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok(hex_digit(pair[0])? * 16 + hex_digit(pair[1])?))
        .collect()
}

/// An opaque 128-bit identifier in network byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; 16]);

impl Uuid {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl FromStr for Uuid {
    type Err = Error;
    fn from_str(source: &str) -> Result<Self> {
        if source.len() != 36 {
            return Err(literal("UUID must use 36-character hyphenated form"));
        }
        let mut bytes = [0; 16];
        let mut offset = 0;
        for (index, byte) in source.bytes().enumerate() {
            if [8, 13, 18, 23].contains(&index) {
                if byte != b'-' {
                    return Err(literal("invalid UUID hyphen position"));
                }
            } else {
                let digit = hex_digit(byte)?;
                bytes[offset / 2] = bytes[offset / 2] * 16 + digit;
                offset += 1;
            }
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if [4, 6, 8, 10].contains(&index) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Bounded binary data. Text conversion uses hexadecimal, without a prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(Vec<u8>);

impl Bytes {
    /// Decode canonical unpadded base64url, as used by the scalar serde payload.
    /// The encoded bound is checked before allocating the decoded buffer.
    pub fn from_base64url(source: &str) -> Result<Self> {
        if source.len() > (MAX_BYTES * 4).div_ceil(3) {
            return Err(literal("bytes exceed 16 MiB"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(source)
            .map_err(|_| literal("expected canonical unpadded base64url"))?;
        Self::new(bytes)
    }

    pub fn to_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.0)
    }

    pub fn new(value: Vec<u8>) -> Result<Self> {
        if value.len() > MAX_BYTES {
            return Err(literal("bytes exceed 16 MiB"));
        }
        Ok(Self(value))
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl FromStr for Bytes {
    type Err = Error;
    fn from_str(source: &str) -> Result<Self> {
        Ok(Self(decode_hex(source)?))
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

macro_rules! scalar_value {
    ($($ty:ident),+ $(,)?) => {$(
        impl From<$ty> for crate::Value {
            fn from(value: $ty) -> Self { Self::$ty(value) }
        }
    )+};
}
scalar_value!(Uuid, Date, Timestamp, Duration, Decimal, Bytes);
