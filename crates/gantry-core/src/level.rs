//! 档位、档位配置键、推理强度与档位变更守卫。

use std::fmt;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 线上档位值：`"None"`…`"Ultimate"`。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum Level {
    None,
    Low,
    Medium,
    High,
    Max,
    Ultimate,
}

impl Level {
    pub fn capped_by(self, cap: Level) -> Level {
        if self > cap {
            cap
        } else {
            self
        }
    }

    pub fn key(self) -> LevelKey {
        self.into()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Max => "Max",
            Self::Ultimate => "Ultimate",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 配置对象里的档位键，一律小写（P-71）。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum LevelKey {
    None,
    Low,
    Medium,
    High,
    Max,
    Ultimate,
}

impl LevelKey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::Ultimate => "ultimate",
        }
    }
}

impl From<Level> for LevelKey {
    fn from(value: Level) -> Self {
        match value {
            Level::None => Self::None,
            Level::Low => Self::Low,
            Level::Medium => Self::Medium,
            Level::High => Self::High,
            Level::Max => Self::Max,
            Level::Ultimate => Self::Ultimate,
        }
    }
}

impl fmt::Display for LevelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 推理强度（`JobSpec.execution.reasoning_effort`）。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 配置中的 `reasoning_effort`：固定值或按档位查表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortSetting {
    Fixed(Effort),
    ByLevel,
}

impl EffortSetting {
    pub fn resolve(self, by_level: impl FnOnce() -> Effort) -> Effort {
        match self {
            Self::Fixed(effort) => effort,
            Self::ByLevel => by_level(),
        }
    }
}

impl Serialize for EffortSetting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fixed(effort) => effort.serialize(serializer),
            Self::ByLevel => serializer.serialize_str("by_level"),
        }
    }
}

impl<'de> Deserialize<'de> for EffortSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "by_level" => Ok(Self::ByLevel),
            "none" => Ok(Self::Fixed(Effort::None)),
            "low" => Ok(Self::Fixed(Effort::Low)),
            "medium" => Ok(Self::Fixed(Effort::Medium)),
            "high" => Ok(Self::Fixed(Effort::High)),
            "max" => Ok(Self::Fixed(Effort::Max)),
            other => Err(serde::de::Error::custom(format!(
                "unknown reasoning_effort: {other}"
            ))),
        }
    }
}

impl JsonSchema for EffortSetting {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EffortSetting".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                { "enum": ["none", "low", "medium", "high", "max"] },
                { "const": "by_level" }
            ]
        })
    }
}

/// 档位变更错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LevelError {
    #[error("level_effective must not decrease")]
    Downgrade,
    #[error("level_effective must not exceed level_cap")]
    AboveCap,
    #[error("current level_effective already exceeds level_cap")]
    InconsistentCap,
}

impl LevelError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Downgrade => "level_downgrade",
            Self::AboveCap => "level_above_cap",
            Self::InconsistentCap => "level_cap_inconsistent",
        }
    }
}

/// P-57：只能在 cap 内上升，不自动降级。
pub fn check_level_change(cap: Level, from: Level, to: Level) -> Result<(), LevelError> {
    if from > cap {
        return Err(LevelError::InconsistentCap);
    }
    if to > cap {
        return Err(LevelError::AboveCap);
    }
    if to < from {
        return Err(LevelError::Downgrade);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_wire_case() {
        assert_eq!(serde_json::to_string(&Level::Medium).unwrap(), "\"Medium\"");
        assert!(serde_json::from_str::<Level>("\"none\"")
            .unwrap_err()
            .to_string()
            .contains("unknown variant"));
        assert_eq!(LevelKey::from(Level::Ultimate).as_str(), "ultimate");
    }

    #[test]
    fn level_change_guard() {
        assert!(check_level_change(Level::High, Level::Low, Level::High).is_ok());
        assert_eq!(
            check_level_change(Level::High, Level::High, Level::Low),
            Err(LevelError::Downgrade)
        );
        assert_eq!(
            check_level_change(Level::Medium, Level::Low, Level::High),
            Err(LevelError::AboveCap)
        );
    }

    #[test]
    fn effort_setting_shape() {
        assert_eq!(
            serde_json::to_string(&EffortSetting::ByLevel).unwrap(),
            "\"by_level\""
        );
        assert_eq!(
            serde_json::to_string(&EffortSetting::Fixed(Effort::High)).unwrap(),
            "\"high\""
        );
    }
}
