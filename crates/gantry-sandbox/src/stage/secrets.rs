//! 形似密钥的值扫描（sandbox.md §12.B：`job.json` 与 `ExecSpec.env` 中没有任何形似密钥的值）。
//!
//! 两道：已知前缀（`sk-`、`ghp_`、`github_pat_`、`AKIA`、PEM 私钥头……）与高熵串。
//! 十六进制串（摘要、git oid、nonce）与 ULID 一类的 ID 不算高熵密钥；占位 key
//! `gantry-egress-managed` 豁免。这是纵深防御：真正的保证是沙盒里没有任何长期凭据（I1）。

use std::collections::BTreeMap;

use serde_json::Value;

use super::StageError;
use crate::invariants::PLACEHOLDER_ENV;

/// 已知密钥前缀（大小写敏感）。
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "AKIA",
    "ASIA",
    "AIza",
    "-----BEGIN",
];

/// 高熵判定的最短长度。
const ENTROPY_MIN_LEN: usize = 32;
/// 每字符香农熵阈值（比特）。
const ENTROPY_THRESHOLD: f64 = 4.0;

fn is_placeholder(value: &str) -> bool {
    PLACEHOLDER_ENV
        .iter()
        .any(|(_, placeholder)| *placeholder == value)
}

/// 把字符串切成"像令牌"的片段：字母数字与 `+=_-`。`/` 作分隔符，路径按段判断
/// （否则 `/gantry/inputs/upstream/nod_…/` 这类路径会被当成一个高熵串）。
fn tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-')))
        .filter(|token| !token.is_empty())
}

fn shannon_entropy(token: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in token.bytes() {
        counts[b as usize] += 1;
    }
    let len = token.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = f64::from(*count) / len;
            -p * p.log2()
        })
        .sum()
}

fn high_entropy(token: &str) -> bool {
    if token.len() < ENTROPY_MIN_LEN {
        return false;
    }
    // 摘要、oid、nonce 都是十六进制。
    if token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let has_upper = token.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = token.bytes().any(|b| b.is_ascii_lowercase());
    let has_digit = token.bytes().any(|b| b.is_ascii_digit());
    has_upper && has_lower && has_digit && shannon_entropy(token) >= ENTROPY_THRESHOLD
}

/// 单个值是否形似密钥。
pub fn looks_secret(value: &str) -> bool {
    if is_placeholder(value) {
        return false;
    }
    tokens(value).any(|token| {
        SECRET_PREFIXES
            .iter()
            .any(|prefix| token.starts_with(prefix) && token.len() >= prefix.len() + 16)
            || high_entropy(token)
    }) || value.contains("PRIVATE KEY-----")
}

fn walk(value: &Value, pointer: &mut String, hit: &mut Option<String>) {
    if hit.is_some() {
        return;
    }
    match value {
        Value::String(text) => {
            if looks_secret(text) {
                *hit = Some(pointer.clone());
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let len = pointer.len();
                pointer.push_str(&format!("/{index}"));
                walk(item, pointer, hit);
                pointer.truncate(len);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(key);
                walk(item, pointer, hit);
                pointer.truncate(len);
            }
        }
        _ => {}
    }
}

/// 扫描 `job.json` 的全部字符串值；命中是引擎缺陷（`InvalidSpec`），只报告位置不报告值。
pub fn assert_no_secret_like_values(job_bytes: &[u8]) -> Result<(), StageError> {
    let value: Value = serde_json::from_slice(job_bytes)
        .map_err(|err| StageError::InvalidSpec(format!("job.json is not JSON: {err}")))?;
    let mut hit = None;
    walk(&value, &mut String::new(), &mut hit);
    match hit {
        Some(pointer) => Err(StageError::InvalidSpec(format!(
            "job.json value at {pointer} looks like a secret"
        ))),
        None => Ok(()),
    }
}

/// 扫描 `ExecSpec.env`（签发 `job_token` 之前的那份，不含令牌）。
pub fn assert_env_clean(env: &BTreeMap<String, String>) -> Result<(), StageError> {
    for (name, value) in env {
        if looks_secret(value) {
            return Err(StageError::InvalidSpec(format!(
                "environment variable {name} looks like a secret"
            )));
        }
    }
    Ok(())
}
