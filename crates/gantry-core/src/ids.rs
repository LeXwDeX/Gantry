//! 带前缀的 ULID 标识符、时钟与熵源注入、单调生成器。

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::time::Timestamp;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const MAX_MS: u64 = (1u64 << 48) - 1;
const MAX_RANDOM: u128 = (1u128 << 80) - 1;

/// 标识符种类：只提供线上前缀。
pub trait IdKind: Copy + 'static {
    const PREFIX: &'static str;
}

macro_rules! id_kinds {
    ($(($name:ident, $ty:ident, $prefix:literal)),* $(,)?) => {
        $(#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)] pub struct $ty;
          impl IdKind for $ty { const PREFIX: &'static str = $prefix; }
          pub type $name = Id<$ty>;)*
    };
}

id_kinds!(
    (ProjectId, Prj, "prj"),
    (DeliveryId, Dlv, "dlv"),
    (WorkflowId, Wfl, "wfl"),
    (NodeId, Nod, "nod"),
    (AttemptId, Att, "att"),
    (WorktreeId, Wtr, "wtr"),
    (SandboxId, Sbx, "sbx"),
    (ConversationId, Cnv, "cnv"),
    (MessageId, Msg, "msg"),
    (InboxId, Inb, "inb"),
    (ApprovalId, Apr, "apr"),
    (ActionId, Act, "act"),
    (DeviceId, Dev, "dev"),
    (EventId, Evt, "evt"),
    (BudgetId, Bdg, "bdg"),
);

/// 128 位标识符；文本形式为 `<prefix>_<26 位 Crockford base32>`。
#[derive(Clone, Copy)]
pub struct Id<K: IdKind> {
    value: u128,
    _kind: PhantomData<K>,
}

impl<K: IdKind> Id<K> {
    /// 从毫秒时间戳与 80 位随机部分构造。
    pub fn from_parts(timestamp_ms: u64, random80: u128) -> Result<Self, IdError> {
        if timestamp_ms > MAX_MS {
            return Err(IdError::Overflow);
        }
        if random80 > MAX_RANDOM {
            return Err(IdError::Overflow);
        }
        Ok(Self {
            value: ((timestamp_ms as u128) << 80) | random80,
            _kind: PhantomData,
        })
    }

    /// 构造前 48 位为毫秒时间戳。
    pub fn timestamp_ms(&self) -> u64 {
        (self.value >> 80) as u64
    }

    /// 80 位随机部分。
    pub fn random80(&self) -> u128 {
        self.value & MAX_RANDOM
    }

    /// 128 位原始值。
    pub fn as_u128(self) -> u128 {
        self.value
    }

    fn encode(self) -> String {
        format!("{}_{}", K::PREFIX, encode_ulid(self.value))
    }
}

impl<K: IdKind> PartialEq for Id<K> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<K: IdKind> Eq for Id<K> {}

impl<K: IdKind> std::hash::Hash for Id<K> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<K: IdKind> PartialOrd for Id<K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: IdKind> Ord for Id<K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<K: IdKind> fmt::Display for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl<K: IdKind> fmt::Debug for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl<K: IdKind> FromStr for Id<K> {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix(K::PREFIX)
            .and_then(|r| r.strip_prefix('_'))
            .ok_or(IdError::BadPrefix)?;
        let value = decode_ulid(rest)?;
        Ok(Self {
            value,
            _kind: PhantomData,
        })
    }
}

impl<K: IdKind> Serialize for Id<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encode())
    }
}

impl<'de, K: IdKind> Deserialize<'de> for Id<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl<K: IdKind> JsonSchema for Id<K> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("Id_{}", K::PREFIX).into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": format!("^{}_[0-7][0-9A-HJKMNP-TV-Z]{{25}}$", K::PREFIX),
        })
    }
}

/// 无前缀 ULID，仅用于 `request_id` 等透明标识。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ulid(u128);

impl Ulid {
    pub fn from_parts(timestamp_ms: u64, random80: u128) -> Result<Self, IdError> {
        if timestamp_ms > MAX_MS || random80 > MAX_RANDOM {
            return Err(IdError::Overflow);
        }
        Ok(Self(((timestamp_ms as u128) << 80) | random80))
    }

    pub fn timestamp_ms(self) -> u64 {
        (self.0 >> 80) as u64
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_ulid(self.0))
    }
}

impl fmt::Debug for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_ulid(self.0))
    }
}

impl FromStr for Ulid {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        decode_ulid(s).map(Self)
    }
}

impl Serialize for Ulid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode_ulid(self.0))
    }
}

impl<'de> Deserialize<'de> for Ulid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Ulid {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Ulid".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-7][0-9A-HJKMNP-TV-Z]{25}$",
        })
    }
}

fn encode_ulid(value: u128) -> String {
    let mut buf = [b'0'; 26];
    for (index, slot) in buf.iter_mut().enumerate() {
        let shift = 5 * (25 - index);
        *slot = CROCKFORD[((value >> shift) & 0x1f) as usize];
    }
    // 128 位值的高 2 位使首字符自然落在 0..=7；其余算法保证。
    String::from_utf8(buf.to_vec()).unwrap_or_default()
}

fn decode_ulid(text: &str) -> Result<u128, IdError> {
    if text.len() != 26 {
        return Err(IdError::Length);
    }
    if text.chars().any(|c| c.is_ascii_lowercase()) {
        return Err(IdError::Lowercase);
    }
    let bytes = text.as_bytes();
    let mut value: u128 = 0;
    for (index, byte) in bytes.iter().enumerate() {
        let digit = decode_digit(*byte).ok_or(IdError::InvalidChar)?;
        if index == 0 && digit > 7 {
            return Err(IdError::Overflow);
        }
        value = (value << 5) | digit as u128;
    }
    Ok(value)
}

fn decode_digit(byte: u8) -> Option<u8> {
    CROCKFORD.iter().position(|c| *c == byte).map(|p| p as u8)
}

/// ID 解析与生成错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("identifier prefix does not match")]
    BadPrefix,
    #[error("identifier body must be 26 characters")]
    Length,
    #[error("identifier uses lowercase characters")]
    Lowercase,
    #[error("identifier contains a character outside Crockford base32")]
    InvalidChar,
    #[error("identifier exceeds 128 bits or generator exhausted")]
    Overflow,
    #[error("clock returned a timestamp before the Unix epoch")]
    NegativeTimestamp,
}

impl IdError {
    pub fn code(self) -> &'static str {
        match self {
            Self::BadPrefix => "bad_prefix",
            Self::Length => "bad_length",
            Self::Lowercase => "lowercase",
            Self::InvalidChar => "invalid_char",
            Self::Overflow => "overflow",
            Self::NegativeTimestamp => "negative_timestamp",
        }
    }
}

/// 可信时钟，由调用方注入。
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// 可信熵源，由调用方注入。
pub trait Entropy: Send + Sync {
    fn fill(&self, buf: &mut [u8]);
}

/// 单调 ULID 生成器。同一毫秒内随机部分递增；时钟回拨沿用上次毫秒数。
pub struct IdGen {
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    state: Mutex<GenState>,
}

#[derive(Default)]
struct GenState {
    last_ms: Option<u64>,
    last_random: u128,
}

impl IdGen {
    pub fn new(clock: Arc<dyn Clock>, entropy: Arc<dyn Entropy>) -> Self {
        Self {
            clock,
            entropy,
            state: Mutex::new(GenState::default()),
        }
    }

    pub fn next<K: IdKind>(&self) -> Result<Id<K>, IdError> {
        let value = self.next_u128()?;
        Ok(Id {
            value,
            _kind: PhantomData,
        })
    }

    pub fn next_ulid(&self) -> Result<Ulid, IdError> {
        self.next_u128().map(Ulid)
    }

    fn next_u128(&self) -> Result<u128, IdError> {
        let now = self.clock.now().as_millis();
        if now < 0 {
            return Err(IdError::NegativeTimestamp);
        }
        let now = now as u64;
        if now > MAX_MS {
            return Err(IdError::Overflow);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (ms, random) = match state.last_ms {
            Some(last_ms) if now <= last_ms => (last_ms, state.last_random + 1),
            _ => {
                let mut bytes = [0u8; 10];
                self.entropy.fill(&mut bytes);
                let mut random = 0u128;
                for byte in bytes {
                    random = (random << 8) | byte as u128;
                }
                (now, random & MAX_RANDOM)
            }
        };
        if random > MAX_RANDOM {
            return Err(IdError::Overflow);
        }
        state.last_ms = Some(ms);
        state.last_random = random;
        Ok(((ms as u128) << 80) | random)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_text_roundtrip() {
        let id = ProjectId::from_parts(1_700_000_000_000, 1).unwrap();
        let text = id.to_string();
        assert!(text.starts_with("prj_"));
        assert_eq!(text.len(), 30);
        assert_eq!(text.parse::<ProjectId>().unwrap(), id);
    }

    #[test]
    fn id_rejects_bad_input() {
        assert_eq!("prj_".parse::<ProjectId>().unwrap_err(), IdError::Length);
        assert_eq!(
            "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"
                .parse::<ProjectId>()
                .unwrap_err(),
            IdError::BadPrefix
        );
        assert_eq!(
            "prj_01jb7m4p8r2x6v9n3q5t7w1yzb"
                .parse::<ProjectId>()
                .unwrap_err(),
            IdError::Lowercase
        );
        assert_eq!(
            "prj_01JB7M4P8R2X6V9N3Q5T7W1YZI"
                .parse::<ProjectId>()
                .unwrap_err(),
            IdError::InvalidChar
        );
        assert_eq!(
            "prj_81JB7M4P8R2X6V9N3Q5T7W1YZB"
                .parse::<ProjectId>()
                .unwrap_err(),
            IdError::Overflow
        );
    }
}
