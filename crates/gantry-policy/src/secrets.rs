//! 密钥扫描器（worker-protocol §8.3 P9；policy.md §7.4 i 行、OQ-P3）。
//!
//! 补丁新增行与 `repo` / `forge` 的出站净化共用同一个扫描器（经
//! `PolicyPort::scan_text`）。命中只返回位置与模式名，**不返回、不记录明文**。
//! 模式是保守的内置集合，宁可多报；扩展须走规格。

use gantry_core::ports::SecretFinding;

/// 模式名；进入事件与日志的只有这些常量。
pub const PATTERN_OPENAI_KEY: &str = "openai_api_key";
pub const PATTERN_GITHUB_TOKEN: &str = "github_token";
pub const PATTERN_PRIVATE_KEY: &str = "private_key_block";
pub const PATTERN_AWS_ACCESS_KEY: &str = "aws_access_key_id";

/// 扫描一段文本，逐行报告命中（行号从 1 起）。
pub fn scan_text(text: &str) -> Vec<SecretFinding> {
    text.lines()
        .enumerate()
        .flat_map(|(index, line)| {
            scan_line(line)
                .into_iter()
                .map(move |pattern| SecretFinding {
                    path: None,
                    line: u32::try_from(index + 1).ok(),
                    pattern: pattern.to_owned(),
                })
        })
        .collect()
}

/// 扫描单行，返回命中的模式名（去重、按出现顺序）。
pub fn scan_line(line: &str) -> Vec<&'static str> {
    let mut hits = Vec::new();
    let mut push = |pattern: &'static str| {
        if !hits.contains(&pattern) {
            hits.push(pattern);
        }
    };
    if has_token(line, "sk-", 20, |b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
    }) {
        push(PATTERN_OPENAI_KEY);
    }
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        if has_token(line, prefix, 36, |b| b.is_ascii_alphanumeric()) {
            push(PATTERN_GITHUB_TOKEN);
        }
    }
    if has_token(line, "github_pat_", 22, |b| {
        b.is_ascii_alphanumeric() || b == b'_'
    }) {
        push(PATTERN_GITHUB_TOKEN);
    }
    if has_token(line, "AKIA", 16, |b| {
        b.is_ascii_uppercase() || b.is_ascii_digit()
    }) {
        push(PATTERN_AWS_ACCESS_KEY);
    }
    if let Some(start) = line.find("-----BEGIN ") {
        if line[start..].contains("PRIVATE KEY-----") {
            push(PATTERN_PRIVATE_KEY);
        }
    }
    hits
}

/// `prefix` 出现在词边界上，且其后至少有 `min_len` 个满足 `body` 的字节。
fn has_token(line: &str, prefix: &str, min_len: usize, body: impl Fn(u8) -> bool) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(offset) = line[from..].find(prefix) {
        let start = from + offset;
        let at_boundary = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let run = bytes[start + prefix.len()..]
            .iter()
            .take_while(|b| body(**b))
            .count();
        if at_boundary && run >= min_len {
            return true;
        }
        from = start + prefix.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // 测试用的伪令牌在运行时拼出，仓库里不出现形似真实密钥的字面量。
    fn fake(prefix: &str, ch: char, len: usize) -> String {
        format!("{prefix}{}", ch.to_string().repeat(len))
    }

    #[test]
    fn detects_builtin_patterns() {
        assert_eq!(
            scan_line(&fake("key = sk-", 'a', 32)),
            vec![PATTERN_OPENAI_KEY]
        );
        assert_eq!(
            scan_line(&fake("token: ghp_", 'B', 36)),
            vec![PATTERN_GITHUB_TOKEN]
        );
        assert_eq!(
            scan_line(&fake("github_pat_", 'c', 30)),
            vec![PATTERN_GITHUB_TOKEN]
        );
        assert_eq!(
            scan_line(&fake("AKIA", 'Z', 16)),
            vec![PATTERN_AWS_ACCESS_KEY]
        );
        let header = format!("-----BEGIN {} PRIVATE KEY-----", "RSA");
        assert_eq!(scan_line(&header), vec![PATTERN_PRIVATE_KEY]);
    }

    #[test]
    fn ignores_ordinary_text() {
        assert!(scan_line("conformance-probe").is_empty());
        assert!(scan_line("let task-runner = disk-usage;").is_empty());
        assert!(scan_line(&fake("ask-", 'a', 40)).is_empty());
        assert!(scan_line(&fake("ghp_", 'a', 10)).is_empty());
    }

    #[test]
    fn findings_carry_line_and_pattern_only() {
        let secret = fake("sk-", 'q', 24);
        let text = format!("first\n{secret}\n");
        let findings = scan_text(&text);
        assert_eq!(
            findings,
            vec![SecretFinding {
                path: None,
                line: Some(2),
                pattern: PATTERN_OPENAI_KEY.into(),
            }]
        );
        assert!(!format!("{findings:?}").contains(&secret));
    }
}
