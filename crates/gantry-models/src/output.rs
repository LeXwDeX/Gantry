//! 直接角色与输出 schema（models.md §7.3 第 1、6 步，worker-protocol §6）。
//!
//! 输出按 `core::worker::StructuredOutput` 严格解析（`kind` 判别 + 字段类型），
//! 再跑一致性规则（如 `ACCEPT` ⇒ 无 blocker、`ci_classification` 必带分类）。
//! 校验失败只给出稳定的错误码；重试时回喂错误码，不回喂模型原输出。

use gantry_core::entities::{Role, Tier};
use gantry_core::worker::{StructuredOutput, StructuredOutputKind};
use serde_json::Value;

use crate::error::ModelsError;

/// 一个直接调用角色的静态描述。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectRole {
    pub kind: StructuredOutputKind,
    /// 由角色决定的层级（architecture §9）；直接角色不会是 worker。
    pub tier: Tier,
    /// `gantry.worker.output.<kind>/1`。
    pub schema_id: &'static str,
    /// 角色的输出上限（内部常量，OQ-5）。
    pub max_output_tokens: u32,
    schema_text: &'static str,
}

impl DirectRole {
    /// 只接受 `intent`、`admission`、`summarize`、`classify`；其余 → `InvalidRole`。
    pub fn for_role(role: &Role) -> Result<Self, ModelsError> {
        let role = match role {
            Role::Intent => DirectRole {
                kind: StructuredOutputKind::Intent,
                tier: Tier::Planner,
                schema_id: "gantry.worker.output.intent/1",
                max_output_tokens: 1024,
                schema_text: include_str!("../../../schemas/worker/output.intent.schema.json"),
            },
            Role::Admission => DirectRole {
                kind: StructuredOutputKind::Admission,
                tier: Tier::Planner,
                schema_id: "gantry.worker.output.admission/1",
                max_output_tokens: 8192,
                schema_text: include_str!("../../../schemas/worker/output.admission.schema.json"),
            },
            Role::Summarize => DirectRole {
                kind: StructuredOutputKind::Summarize,
                tier: Tier::Planner,
                schema_id: "gantry.worker.output.summarize/1",
                max_output_tokens: 8192,
                schema_text: include_str!("../../../schemas/worker/output.summarize.schema.json"),
            },
            Role::Classify => DirectRole {
                kind: StructuredOutputKind::Classify,
                tier: Tier::Utility,
                schema_id: "gantry.worker.output.classify/1",
                max_output_tokens: 2048,
                schema_text: include_str!("../../../schemas/worker/output.classify.schema.json"),
            },
            other => {
                return Err(ModelsError::InvalidRole {
                    role: other.as_str(),
                })
            }
        };
        Ok(role)
    }

    /// `schema` 必须是该角色的 schema id。
    pub fn check_schema(&self, role: &Role, schema: &str) -> Result<(), ModelsError> {
        if schema != self.schema_id {
            return Err(ModelsError::InvalidSchema {
                role: role.as_str(),
                schema: schema.to_owned(),
            });
        }
        Ok(())
    }

    /// 发给提供方的 JSON Schema（`schemas/worker/output.<kind>.schema.json`）。
    pub fn json_schema(&self) -> Value {
        serde_json::from_str(self.schema_text).expect("generated worker output schema is JSON")
    }

    /// 解析并校验模型输出。成功返回规范化后的 JSON。
    pub fn validate(&self, text: &str) -> Result<Value, OutputError> {
        let value: Value =
            serde_json::from_str(text.trim()).map_err(|_| OutputError::InvalidJson)?;
        let parsed: StructuredOutput =
            serde_json::from_value(value).map_err(|_| OutputError::SchemaMismatch)?;
        if parsed.kind() != self.kind {
            return Err(OutputError::WrongKind);
        }
        parsed
            .validate()
            .map_err(|error| OutputError::Inconsistent(format!("{error:?}")))?;
        serde_json::to_value(&parsed).map_err(|_| OutputError::SchemaMismatch)
    }
}

/// 输出校验失败的类别；`code()` 是唯一回喂给模型的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputError {
    InvalidJson,
    SchemaMismatch,
    WrongKind,
    Inconsistent(String),
}

impl OutputError {
    pub fn code(&self) -> String {
        match self {
            Self::InvalidJson => "invalid_json".into(),
            Self::SchemaMismatch => "schema_mismatch".into(),
            Self::WrongKind => "wrong_kind".into(),
            Self::Inconsistent(rule) => format!("inconsistent:{}", rule_code(rule)),
        }
    }
}

/// `WorkerError` 的变体名转为 snake_case，作为稳定的规则码。
fn rule_code(debug: &str) -> String {
    let name = debug.split(['(', ' ', '{']).next().unwrap_or(debug);
    let mut out = String::with_capacity(name.len() + 4);
    for (index, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
