use crate::error::{Error, Result};
use std::fmt;

/// Exact coefficient and scale. Precision belongs to the target schema type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    coefficient: i128,
    scale: u8,
}

fn range(message: &str) -> Error {
    Error::new("E_DECIMAL_RANGE", message)
}

pub fn validate_type(precision: u8, scale: u8) -> Result<()> {
    if !(1..=38).contains(&precision) || scale > precision {
        return Err(Error::new(
            "E_DECIMAL_TYPE",
            "decimal requires 1 <= precision <= 38 and scale <= precision",
        ));
    }
    Ok(())
}

impl Decimal {
    pub fn new(coefficient: i128, precision: u8, scale: u8) -> Result<Self> {
        validate_type(precision, scale)?;
        if coefficient.unsigned_abs() >= 10_u128.pow(precision.into()) {
            return Err(range("decimal coefficient exceeds precision"));
        }
        Ok(Self { coefficient, scale })
    }
    pub const fn coefficient(self) -> i128 {
        self.coefficient
    }
    pub const fn scale(self) -> u8 {
        self.scale
    }
    pub fn precision(self) -> u8 {
        let digits = self
            .coefficient
            .unsigned_abs()
            .checked_ilog10()
            .unwrap_or(0)
            + 1;
        (digits as u8).max(self.scale)
    }

    /// Parse without rounding using a declared target precision and scale.
    pub fn parse(source: &str, precision: u8, scale: u8) -> Result<Self> {
        validate_type(precision, scale)?;
        let negative = source.starts_with('-');
        let body = source.strip_prefix('-').unwrap_or(source);
        let (integer, fraction) = match body.split_once('.') {
            Some((integer, fraction)) if !fraction.is_empty() => (integer, fraction),
            Some(_) => return Err(range("decimal fraction requires digits")),
            None => (body, ""),
        };
        if integer.is_empty()
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return Err(range(
                "decimal requires plain digits without exponent, plus, or separators",
            ));
        }
        if fraction
            .as_bytes()
            .get(usize::from(scale)..)
            .is_some_and(|tail| tail.iter().any(|b| *b != b'0'))
        {
            return Err(range("decimal rescale would discard nonzero digits"));
        }
        let mut coefficient = 0_i128;
        for digit in integer.bytes().chain(fraction.bytes().take(scale.into())) {
            coefficient = coefficient
                .checked_mul(10)
                .and_then(|n| n.checked_add((digit - b'0').into()))
                .ok_or_else(|| range("decimal coefficient overflows"))?;
        }
        let padding = usize::from(scale).saturating_sub(fraction.len());
        coefficient = coefficient
            .checked_mul(10_i128.pow(padding as u32))
            .ok_or_else(|| range("decimal coefficient overflows"))?;
        if negative {
            coefficient = -coefficient;
        }
        Self::new(coefficient, precision, scale)
    }

    /// Infer the smallest precision for a context-free literal, retaining its scale.
    pub fn infer(source: &str) -> Result<Self> {
        let scale = source
            .split_once('.')
            .map_or(0, |(_, fraction)| fraction.len());
        if scale > 38 {
            return Err(range("decimal scale exceeds 38"));
        }
        Self::parse(source, 38, scale as u8)
    }

    pub fn rescale(self, precision: u8, scale: u8) -> Result<Self> {
        validate_type(precision, scale)?;
        let coefficient = if scale >= self.scale {
            self.coefficient
                .checked_mul(10_i128.pow((scale - self.scale).into()))
                .ok_or_else(|| range("decimal rescale overflows"))?
        } else {
            let divisor = 10_i128.pow((self.scale - scale).into());
            if self.coefficient % divisor != 0 {
                return Err(range("decimal rescale would discard nonzero digits"));
            }
            self.coefficient / divisor
        };
        Self::new(coefficient, precision, scale)
    }

    pub(crate) fn checked_add(self, other: Self) -> Result<Self> {
        if self.scale != other.scale {
            return Err(Error::new("E_TYPE", "decimal scales must match"));
        }
        let coefficient = self
            .coefficient
            .checked_add(other.coefficient)
            .ok_or_else(|| Error::new("E_ARITH", "decimal addition overflow"))?;
        Self::new(coefficient, 38, self.scale)
            .map_err(|_| Error::new("E_ARITH", "decimal addition exceeds 38 digits"))
    }

    pub(crate) fn checked_sub(self, other: Self) -> Result<Self> {
        if self.scale != other.scale {
            return Err(Error::new("E_TYPE", "decimal scales must match"));
        }
        let coefficient = self
            .coefficient
            .checked_sub(other.coefficient)
            .ok_or_else(|| Error::new("E_ARITH", "decimal subtraction overflow"))?;
        Self::new(coefficient, 38, self.scale)
            .map_err(|_| Error::new("E_ARITH", "decimal subtraction exceeds 38 digits"))
    }

    pub(crate) fn checked_neg(self) -> Result<Self> {
        let coefficient = self
            .coefficient
            .checked_neg()
            .ok_or_else(|| Error::new("E_ARITH", "decimal negation overflow"))?;
        Self::new(coefficient, 38, self.scale)
            .map_err(|_| Error::new("E_ARITH", "decimal negation exceeds 38 digits"))
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.coefficient < 0 {
            f.write_str("-")?;
        }
        let unsigned = self.coefficient.unsigned_abs();
        if self.scale == 0 {
            return write!(f, "{unsigned}");
        }
        let divisor = 10_u128.pow(self.scale.into());
        write!(
            f,
            "{}.{:0width$}",
            unsigned / divisor,
            unsigned % divisor,
            width = self.scale.into()
        )
    }
}
