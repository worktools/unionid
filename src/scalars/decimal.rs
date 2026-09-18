use crate::error::{Error, Result};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};
use std::fmt;
use std::str::FromStr;

/// Explicit rounding used by decimal operations which can discard digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecimalRounding {
    Exact,
    TowardZero,
    AwayFromZero,
    Floor,
    Ceil,
    HalfUp,
    HalfEven,
}

impl DecimalRounding {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::TowardZero => "toward_zero",
            Self::AwayFromZero => "away_from_zero",
            Self::Floor => "floor",
            Self::Ceil => "ceil",
            Self::HalfUp => "half_up",
            Self::HalfEven => "half_even",
        }
    }
}

impl FromStr for DecimalRounding {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "exact" => Ok(Self::Exact),
            "toward_zero" => Ok(Self::TowardZero),
            "away_from_zero" => Ok(Self::AwayFromZero),
            "floor" => Ok(Self::Floor),
            "ceil" => Ok(Self::Ceil),
            "half_up" => Ok(Self::HalfUp),
            "half_even" => Ok(Self::HalfEven),
            _ => Err(Error::new(
                "E_DECIMAL_ROUNDING",
                format!(
                    "unknown decimal rounding mode '{value}'; expected exact, toward_zero, away_from_zero, floor, ceil, half_up, or half_even"
                ),
            )),
        }
    }
}

/// Exact coefficient and scale. Precision belongs to the target schema type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    coefficient: i128,
    scale: u8,
}

fn range(message: &str) -> Error {
    Error::new("E_DECIMAL_RANGE", message)
}

fn arithmetic(message: impl Into<String>) -> Error {
    Error::new("E_ARITH", message)
}

fn power_of_ten(exponent: u32) -> BigInt {
    BigInt::from(10_u8).pow(exponent)
}

fn round_ratio(
    numerator: BigInt,
    denominator: BigInt,
    rounding: DecimalRounding,
) -> Result<BigInt> {
    if denominator.is_zero() {
        return Err(arithmetic("decimal division by zero"));
    }
    let negative = numerator.is_negative() != denominator.is_negative();
    let numerator = numerator.abs();
    let denominator = denominator.abs();
    let quotient = &numerator / &denominator;
    let remainder = numerator % &denominator;
    if remainder.is_zero() {
        return Ok(if negative { -quotient } else { quotient });
    }
    if rounding == DecimalRounding::Exact {
        return Err(arithmetic(
            "decimal operation requires rounding but mode is exact",
        ));
    }
    let increment = match rounding {
        DecimalRounding::Exact | DecimalRounding::TowardZero => false,
        DecimalRounding::AwayFromZero => true,
        DecimalRounding::Floor => negative,
        DecimalRounding::Ceil => !negative,
        DecimalRounding::HalfUp => remainder * 2 >= denominator,
        DecimalRounding::HalfEven => {
            let doubled = &remainder * 2;
            doubled > denominator
                || (doubled == denominator && (&quotient % 2_u8) != BigInt::zero())
        }
    };
    let rounded = quotient + u8::from(increment);
    Ok(if negative { -rounded } else { rounded })
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

    pub fn rescale_with_rounding(
        self,
        precision: u8,
        scale: u8,
        rounding: DecimalRounding,
    ) -> Result<Self> {
        validate_type(precision, scale)?;
        let coefficient = if scale >= self.scale {
            BigInt::from(self.coefficient) * power_of_ten((scale - self.scale).into())
        } else {
            round_ratio(
                BigInt::from(self.coefficient),
                power_of_ten((self.scale - scale).into()),
                rounding,
            )?
        };
        Self::from_bigint(coefficient, precision, scale, "decimal rounding")
    }

    pub fn checked_mul_to(
        self,
        other: Self,
        precision: u8,
        scale: u8,
        rounding: DecimalRounding,
    ) -> Result<Self> {
        validate_type(precision, scale)?;
        let product = BigInt::from(self.coefficient) * BigInt::from(other.coefficient);
        let source_scale = u32::from(self.scale) + u32::from(other.scale);
        let coefficient = if u32::from(scale) >= source_scale {
            product * power_of_ten(u32::from(scale) - source_scale)
        } else {
            round_ratio(
                product,
                power_of_ten(source_scale - u32::from(scale)),
                rounding,
            )?
        };
        Self::from_bigint(coefficient, precision, scale, "decimal multiplication")
    }

    pub fn checked_div_to(
        self,
        other: Self,
        precision: u8,
        scale: u8,
        rounding: DecimalRounding,
    ) -> Result<Self> {
        validate_type(precision, scale)?;
        if other.coefficient == 0 {
            return Err(arithmetic("decimal division by zero"));
        }
        let numerator_exponent = u32::from(scale) + u32::from(other.scale);
        let denominator_exponent = u32::from(self.scale);
        let mut numerator = BigInt::from(self.coefficient);
        let mut denominator = BigInt::from(other.coefficient);
        if numerator_exponent >= denominator_exponent {
            numerator *= power_of_ten(numerator_exponent - denominator_exponent);
        } else {
            denominator *= power_of_ten(denominator_exponent - numerator_exponent);
        }
        let coefficient = round_ratio(numerator, denominator, rounding)?;
        Self::from_bigint(coefficient, precision, scale, "decimal division")
    }

    pub(crate) fn average_coefficients(
        coefficient: BigInt,
        source_scale: u8,
        count: i64,
        precision: u8,
        scale: u8,
        rounding: DecimalRounding,
    ) -> Result<Self> {
        validate_type(precision, scale)?;
        let mut numerator = coefficient;
        let mut denominator = BigInt::from(count);
        if scale >= source_scale {
            numerator *= power_of_ten((scale - source_scale).into());
        } else {
            denominator *= power_of_ten((source_scale - scale).into());
        }
        let coefficient = round_ratio(numerator, denominator, rounding)?;
        Self::from_bigint(coefficient, precision, scale, "decimal average")
    }

    fn from_bigint(coefficient: BigInt, precision: u8, scale: u8, operation: &str) -> Result<Self> {
        let coefficient = coefficient
            .to_i128()
            .ok_or_else(|| arithmetic(format!("{operation} exceeds 38 digits")))?;
        Self::new(coefficient, precision, scale).map_err(|error| {
            if error.code == "E_DECIMAL_RANGE" {
                arithmetic(format!("{operation} exceeds decimal {precision} {scale}"))
            } else {
                error
            }
        })
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
