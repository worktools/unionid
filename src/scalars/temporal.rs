use super::literal;
use crate::error::{Error, Result};
use std::{fmt, str::FromStr};

const EPOCH_DAYS: i32 = 719_162;
const DAY_MICROS: i64 = 86_400_000_000;

fn leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
fn year_days(year: i32) -> i32 {
    let previous = year - 1;
    previous * 365 + previous / 4 - previous / 100 + previous / 400
}
fn month_days(year: i32, month: u8) -> u8 {
    match month {
        2 => {
            if leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
fn digits(source: &[u8]) -> Result<i32> {
    if source.is_empty() || !source.iter().all(u8::is_ascii_digit) {
        return Err(literal("expected decimal digits"));
    }
    Ok(source.iter().fold(0, |n, b| n * 10 + i32::from(b - b'0')))
}

/// A Gregorian civil day, without a time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(i32);

impl Date {
    pub const MIN_DAYS: i32 = -EPOCH_DAYS;
    pub const MAX_DAYS: i32 = 2_932_896;
    pub fn from_epoch_days(days: i32) -> Result<Self> {
        if !(Self::MIN_DAYS..=Self::MAX_DAYS).contains(&days) {
            return Err(literal("date outside years 0001 through 9999"));
        }
        Ok(Self(days))
    }
    pub const fn epoch_days(self) -> i32 {
        self.0
    }
    fn components(self) -> (i32, u8, u8) {
        let days = self.0 + EPOCH_DAYS;
        let (mut low, mut high) = (1, 10_000);
        while low + 1 < high {
            let middle = (low + high) / 2;
            if year_days(middle) <= days {
                low = middle;
            } else {
                high = middle;
            }
        }
        let mut remaining = days - year_days(low);
        let mut month = 1;
        while remaining >= i32::from(month_days(low, month)) {
            remaining -= i32::from(month_days(low, month));
            month += 1;
        }
        (low, month, remaining as u8 + 1)
    }
}
impl FromStr for Date {
    type Err = Error;
    fn from_str(source: &str) -> Result<Self> {
        let bytes = source.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(literal("date requires YYYY-MM-DD"));
        }
        let year = digits(&bytes[..4])?;
        let month = digits(&bytes[5..7])? as u8;
        let day = digits(&bytes[8..])? as u8;
        if year == 0 || !(1..=12).contains(&month) || day == 0 || day > month_days(year, month) {
            return Err(literal("invalid Gregorian date"));
        }
        let days = year_days(year)
            + (1..month)
                .map(|m| i32::from(month_days(year, m)))
                .sum::<i32>()
            + i32::from(day)
            - 1
            - EPOCH_DAYS;
        Self::from_epoch_days(days)
    }
}
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.components();
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

/// An exact elapsed duration in signed microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(i64);
const UNITS: [(&str, i64); 7] = [
    ("week", 604_800_000_000),
    ("day", DAY_MICROS),
    ("hour", 3_600_000_000),
    ("minute", 60_000_000),
    ("second", 1_000_000),
    ("millisecond", 1_000),
    ("microsecond", 1),
];
impl Duration {
    pub const fn from_microseconds(value: i64) -> Self {
        Self(value)
    }
    pub const fn microseconds(self) -> i64 {
        self.0
    }
}
impl FromStr for Duration {
    type Err = Error;
    fn from_str(source: &str) -> Result<Self> {
        let numeric_start = usize::from(source.starts_with('-'));
        let end = numeric_start
            + source.as_bytes()[numeric_start..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        if end == numeric_start {
            return Err(literal("duration requires an integer and exact unit"));
        }
        let amount: i64 = source[..end]
            .parse()
            .map_err(|_| literal("duration integer overflow"))?;
        let unit = source[end..].strip_suffix('s').unwrap_or(&source[end..]);
        let multiplier = UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, size)| *size)
            .ok_or_else(|| literal("unknown exact duration unit"))?;
        amount
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| literal("duration overflows microseconds"))
    }
}
impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0microseconds");
        }
        let (unit, size) = UNITS
            .iter()
            .find(|(_, size)| self.0 % size == 0)
            .expect("microseconds divide every duration");
        let amount = self.0 / size;
        let suffix = if amount == 1 || amount == -1 { "" } else { "s" };
        write!(f, "{amount}{unit}{suffix}")
    }
}

/// A UTC instant with microsecond precision, in the Date range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);
impl Timestamp {
    pub fn from_epoch_microseconds(value: i64) -> Result<Self> {
        let days = value.div_euclid(DAY_MICROS);
        if !(i64::from(Date::MIN_DAYS)..=i64::from(Date::MAX_DAYS)).contains(&days) {
            return Err(literal("timestamp outside UTC date range"));
        }
        Ok(Self(value))
    }
    pub const fn epoch_microseconds(self) -> i64 {
        self.0
    }
}
impl FromStr for Timestamp {
    type Err = Error;
    fn from_str(source: &str) -> Result<Self> {
        let bytes = source.as_bytes();
        if bytes.len() < 20
            || !source.is_ascii()
            || !matches!(bytes[10], b'T' | b't')
            || bytes[13] != b':'
            || bytes[16] != b':'
        {
            return Err(literal(
                "timestamp requires RFC 3339 date, time, and offset",
            ));
        }
        let date: Date = source[..10].parse()?;
        let hour = digits(&bytes[11..13])?;
        let minute = digits(&bytes[14..16])?;
        let second = digits(&bytes[17..19])?;
        if hour > 23 || minute > 59 || second > 59 {
            return Err(literal("invalid timestamp time or leap second"));
        }
        let mut position = 19;
        let mut micros = 0_i64;
        if bytes[position] == b'.' {
            position += 1;
            let start = position;
            while position < bytes.len() && bytes[position].is_ascii_digit() {
                let digit = bytes[position] - b'0';
                if position - start < 6 {
                    micros = micros * 10 + i64::from(digit);
                } else if digit != 0 {
                    return Err(literal("timestamp exceeds microsecond precision"));
                }
                position += 1;
            }
            if position == start {
                return Err(literal("timestamp fraction requires digits"));
            }
            micros *= 10_i64.pow(6_usize.saturating_sub(position - start) as u32);
        }
        let offset = &source[position..];
        let offset_seconds = if matches!(offset, "Z" | "z") {
            0
        } else {
            let raw = offset.as_bytes();
            if raw.len() != 6 || !matches!(raw[0], b'+' | b'-') || raw[3] != b':' {
                return Err(literal("timestamp requires an explicit offset"));
            }
            let hours = digits(&raw[1..3])?;
            let minutes = digits(&raw[4..6])?;
            if hours > 23 || minutes > 59 || offset == "-00:00" {
                return Err(literal("invalid or unknown timestamp offset"));
            }
            (hours * 3600 + minutes * 60) * if raw[0] == b'-' { -1 } else { 1 }
        };
        Self::from_epoch_microseconds(
            i64::from(date.0) * DAY_MICROS
                + i64::from(hour * 3600 + minute * 60 + second - offset_seconds) * 1_000_000
                + micros,
        )
    }
}
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let date = Date(self.0.div_euclid(DAY_MICROS) as i32);
        let remainder = self.0.rem_euclid(DAY_MICROS);
        let seconds = remainder / 1_000_000;
        write!(
            f,
            "{date}T{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )?;
        let fraction = remainder % 1_000_000;
        if fraction != 0 {
            write!(f, ".{}", format!("{fraction:06}").trim_end_matches('0'))?;
        }
        f.write_str("Z")
    }
}
