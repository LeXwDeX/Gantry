//! UTC 毫秒时间戳、规范 RFC 3339 输出与 UTC 日期。

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Unix epoch 毫秒（UTC）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    pub fn as_millis(self) -> i64 {
        self.0
    }

    /// 宽松 RFC 3339：0–9 位小数、`Z` 或数值偏移；归一为 UTC 毫秒并截断多余精度。
    pub fn parse_rfc3339(input: &str) -> Result<Self, TimeError> {
        parse_rfc3339(input, false)
    }

    /// 严格规范形：恰好三位毫秒并以 `Z` 结尾，用于审批签名载荷。
    pub fn parse_canonical(input: &str) -> Result<Self, TimeError> {
        parse_rfc3339(input, true)
    }

    /// 规范文本：`YYYY-MM-DDTHH:MM:SS.mmmZ`。
    pub fn to_canonical_string(self) -> String {
        format_timestamp(self.0)
    }

    pub fn checked_add_secs(self, secs: u64) -> Option<Self> {
        let delta = i64::try_from(secs).ok()?.checked_mul(1000)?;
        self.0.checked_add(delta).map(Self)
    }

    pub fn to_date(self) -> Date {
        let days = self.0.div_euclid(86_400_000);
        let (year, month, day) = civil_from_days(days);
        Date { year, month, day }
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_canonical_string())
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_canonical_string())
    }
}

impl FromStr for Timestamp {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_rfc3339(s)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_canonical_string())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Timestamp {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Timestamp".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\\.[0-9]{3}Z$",
        })
    }
}

/// UTC 日历日，文本形式 `YYYY-MM-DD`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
}

impl Date {
    pub fn from_ymd(year: i32, month: u8, day: u8) -> Result<Self, TimeError> {
        if month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
            return Err(TimeError::InvalidDate);
        }
        Ok(Self { year, month, day })
    }

    pub fn year(self) -> i32 {
        self.year
    }

    pub fn month(self) -> u8 {
        self.month
    }

    pub fn day(self) -> u8 {
        self.day
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Debug for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl FromStr for Date {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(TimeError::InvalidDate);
        }
        let year = parse_digits(&bytes[0..4])? as i32;
        let month = parse_digits(&bytes[5..7])? as u8;
        let day = parse_digits(&bytes[8..10])? as u8;
        Self::from_ymd(year, month, day)
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Date {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Date".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}$",
        })
    }
}

/// 时间解析错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    #[error("timestamp does not follow RFC 3339")]
    InvalidFormat,
    #[error("timestamp must use the canonical three-digit millisecond form")]
    NotCanonical,
    #[error("timestamp contains an invalid calendar date or time")]
    InvalidDate,
    #[error("timestamp is out of the supported range")]
    OutOfRange,
}

impl TimeError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidFormat => "invalid_format",
            Self::NotCanonical => "not_canonical",
            Self::InvalidDate => "invalid_date",
            Self::OutOfRange => "out_of_range",
        }
    }
}

fn parse_rfc3339(input: &str, canonical: bool) -> Result<Timestamp, TimeError> {
    let bytes = input.as_bytes();
    if bytes.len() < 20 {
        return Err(if canonical {
            TimeError::NotCanonical
        } else {
            TimeError::InvalidFormat
        });
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return Err(bad_format(canonical));
    }
    if canonical {
        if bytes[10] != b'T' {
            return Err(TimeError::NotCanonical);
        }
    } else if bytes[10] != b'T' && bytes[10] != b't' {
        return Err(TimeError::InvalidFormat);
    }
    let year = parse_digits(&bytes[0..4])? as i32;
    let month = parse_digits(&bytes[5..7])? as u8;
    let day = parse_digits(&bytes[8..10])? as u8;
    let hour = parse_digits(&bytes[11..13])? as u8;
    let minute = parse_digits(&bytes[14..16])? as u8;
    let second = parse_digits(&bytes[17..19])? as u8;
    if month == 0
        || month > 12
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(TimeError::InvalidDate);
    }
    // 闰秒按下一毫秒归属：只接受 60 并截断为 59.999 的下一整秒。
    let mut millis = 0i64;
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let digits = index - start;
        if digits == 0 || digits > 9 {
            return Err(bad_format(canonical));
        }
        if canonical && digits != 3 {
            return Err(TimeError::NotCanonical);
        }
        let fraction = parse_digits(&bytes[start..index])?;
        millis = match digits {
            1 => fraction as i64 * 100,
            2 => fraction as i64 * 10,
            3 => fraction as i64,
            _ => {
                let divisor = 10u32.pow((digits - 3) as u32);
                (fraction / divisor) as i64
            }
        };
    } else if canonical {
        return Err(TimeError::NotCanonical);
    }
    let offset_minutes = match bytes.get(index) {
        Some(b'Z') => {
            index += 1;
            0i64
        }
        Some(b'z') if !canonical => {
            index += 1;
            0i64
        }
        Some(sign @ (b'+' | b'-')) => {
            if canonical {
                return Err(TimeError::NotCanonical);
            }
            let s = *sign;
            if index + 6 > bytes.len() || bytes[index + 3] != b':' {
                return Err(bad_format(canonical));
            }
            let oh = parse_digits(&bytes[index + 1..index + 3])? as i64;
            let om = parse_digits(&bytes[index + 4..index + 6])? as i64;
            if oh > 23 || om > 59 {
                return Err(TimeError::InvalidDate);
            }
            index += 6;
            let total = oh * 60 + om;
            if s == b'-' {
                -total
            } else {
                total
            }
        }
        _ => return Err(bad_format(canonical)),
    };
    if index != bytes.len() {
        return Err(bad_format(canonical));
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|v| v.checked_add((hour as i64) * 3600 + (minute as i64) * 60 + second as i64))
        .ok_or(TimeError::OutOfRange)?;
    let utc_ms = seconds
        .checked_mul(1000)
        .and_then(|v| v.checked_add(millis))
        .and_then(|v| v.checked_sub(offset_minutes.checked_mul(60_000)?))
        .ok_or(TimeError::OutOfRange)?;
    Ok(Timestamp(utc_ms))
}

fn bad_format(canonical: bool) -> TimeError {
    if canonical {
        TimeError::NotCanonical
    } else {
        TimeError::InvalidFormat
    }
}

fn parse_digits(bytes: &[u8]) -> Result<u32, TimeError> {
    let mut value = 0u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(TimeError::InvalidFormat);
        }
        value = value * 10 + (byte - b'0') as u32;
    }
    Ok(value)
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant 的 civil 日历算法。
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year as i32, month as u8, day as u8)
}

fn format_timestamp(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3_600_000;
    let minute = (rem / 60_000) % 60;
    let second = (rem / 1000) % 60;
    let millis = rem % 1000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_roundtrip() {
        let text = "2026-10-24T12:41:03.120Z";
        let ts = Timestamp::parse_canonical(text).unwrap();
        assert_eq!(ts.to_canonical_string(), text);
        assert!(Timestamp::parse_canonical("2026-10-24T12:41:03Z").is_err());
        assert!(Timestamp::parse_canonical("2026-10-24T12:41:03.120+00:00").is_err());
    }

    #[test]
    fn lenient_offsets_and_fraction() {
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-24T12:41:02Z")
                .unwrap()
                .to_canonical_string(),
            "2026-10-24T12:41:02.000Z"
        );
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-24T20:41:03.12+08:00")
                .unwrap()
                .to_canonical_string(),
            "2026-10-24T12:41:03.120Z"
        );
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-24T12:41:03.123456789Z")
                .unwrap()
                .as_millis(),
            Timestamp::parse_canonical("2026-10-24T12:41:03.123Z")
                .unwrap()
                .as_millis()
        );
    }

    #[test]
    fn date_roundtrip() {
        let ts = Timestamp::parse_canonical("2026-10-24T12:41:03.120Z").unwrap();
        assert_eq!(ts.to_date().to_string(), "2026-10-24");
        assert_eq!("2026-10-24".parse::<Date>().unwrap(), ts.to_date());
    }
}
