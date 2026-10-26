//! 密钥引用与只写不读的密钥写入类型。core 不承载任何读取密钥值的路径。

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `^[a-z0-9][a-z0-9._-]{0,63}$`
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretName(String);

impl SecretName {
    pub fn parse(value: &str) -> Result<Self, SecretNameError> {
        let bytes = value.as_bytes();
        if bytes.is_empty() || bytes.len() > 64 {
            return Err(SecretNameError::Length);
        }
        let first = bytes[0];
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(SecretNameError::InvalidStart);
        }
        if !bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        }) {
            return Err(SecretNameError::InvalidChar);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretName").field(&self.0).finish()
    }
}

impl FromStr for SecretName {
    type Err = SecretNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for SecretName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SecretName {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretName".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretNameError {
    #[error("secret name must be 1..=64 characters")]
    Length,
    #[error("secret name must start with a lowercase letter or digit")]
    InvalidStart,
    #[error("secret name may only contain lowercase letters, digits, '.', '_' and '-'")]
    InvalidChar,
}

impl SecretNameError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Length => "secret_name_length",
            Self::InvalidStart => "secret_name_start",
            Self::InvalidChar => "secret_name_char",
        }
    }
}

/// 对外可见的密钥引用：只有名称与是否存在。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretRefView {
    pub secret_ref: SecretName,
    pub present: bool,
}

/// 密钥明文。不实现 `Serialize` / `Clone` / `Deserialize`；`Debug` 脱敏。
#[derive(PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// 唯一的取值入口；调用方必须是 `secrets` 提供方或 Egress 控制面实现。
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn into_secret(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

/// 密钥写入：缺省 = 保留；字符串 = 设置；`null` = 拒绝（`secret_clear_in_use`）。
#[derive(Default, PartialEq, Eq)]
pub enum SecretWrite {
    #[default]
    Keep,
    Set(SecretString),
}

impl fmt::Debug for SecretWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => f.write_str("Keep"),
            Self::Set(_) => f.write_str("Set(***)"),
        }
    }
}

impl<'de> Deserialize<'de> for SecretWrite {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SecretWrite;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a secret string; null is not allowed in a write request")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(SecretWrite::Set(SecretString::new(value.to_owned())))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(SecretWrite::Set(SecretString::new(value)))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Err(E::custom("secret_clear_in_use"))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Err(E::custom("secret_clear_in_use"))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// 密钥使用方（architecture §4 secrets 行）。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Consumer {
    Forge,
    Pki,
    Notify,
    Egress,
}

impl Consumer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::Pki => "pki",
            Self::Notify => "notify",
            Self::Egress => "egress",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_write_redacts_and_rejects_null() {
        let write: SecretWrite = serde_json::from_str("\"sk-live-xxx\"").unwrap();
        assert_eq!(format!("{write:?}"), "Set(***)");
        let err = serde_json::from_str::<SecretWrite>("null").unwrap_err();
        assert!(err.to_string().contains("secret_clear_in_use"));
        let keep: SecretWrite = serde_json::from_str("\"keep\"").unwrap();
        assert!(matches!(keep, SecretWrite::Set(_)));
    }

    #[test]
    fn secret_name_rules() {
        assert!("github.app.private_key".parse::<SecretName>().is_ok());
        assert!("Upper".parse::<SecretName>().is_err());
        assert!("-bad".parse::<SecretName>().is_err());
    }
}
