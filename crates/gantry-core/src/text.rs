//! 不可信文本的隔离类型与规范化（worker-protocol §4.4 步 2–3）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 来自 GitHub、沙盒或其他外部来源的文本。
///
/// 刻意不实现 `Deref<Target = str>`、`Display`、`AsRef<str>`；取内容必须调用
/// [`UntrustedText::as_untrusted_str`]，`Debug` 不输出正文。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UntrustedText {
    pub text: String,
    pub source: String,
    pub truncated: bool,
}

impl UntrustedText {
    pub fn new(text: String, source: String, truncated: bool) -> Self {
        Self {
            text,
            source,
            truncated,
        }
    }

    pub fn as_untrusted_str(&self) -> &str {
        &self.text
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

impl std::fmt::Debug for UntrustedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UntrustedText")
            .field("source", &self.source)
            .field("bytes", &self.text.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// 规范化上限。默认单条 64 KiB，头尾各 32 KiB（[提议]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalizeLimits {
    pub max_bytes: usize,
    pub head_bytes: usize,
    pub tail_bytes: usize,
}

impl Default for NormalizeLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024,
            head_bytes: 32 * 1024,
            tail_bytes: 32 * 1024,
        }
    }
}

/// 规范化统计，用于审计与调试。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NormalizeStats {
    pub original_bytes: usize,
    pub removed_chars: usize,
    pub removed_ansi: usize,
    pub removed_bidi: usize,
    pub removed_tag: usize,
    pub replaced_invalid_utf8: usize,
    pub truncated: bool,
}

/// 规范化结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Normalized {
    pub text: String,
    pub stats: NormalizeStats,
}

/// 宽松解码 UTF-8，去掉控制字符、ANSI、bidi 与 Tag 字符，超限时保留头尾。
pub fn normalize_untrusted(input: &[u8], limits: NormalizeLimits) -> Normalized {
    let decoded = String::from_utf8_lossy(input);
    let replaced_invalid_utf8 =
        if decoded.contains('\u{FFFD}') && std::str::from_utf8(input).is_err() {
            input.iter().filter(|b| **b == 0xFF).count().max(1)
        } else {
            0
        };
    let mut stats = NormalizeStats {
        original_bytes: input.len(),
        replaced_invalid_utf8,
        ..NormalizeStats::default()
    };
    let mut kept = String::with_capacity(decoded.len());
    let chars: Vec<char> = decoded.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\u{001b}' {
            let before = kept.len();
            index += 1;
            match chars.get(index) {
                Some('[') => {
                    index += 1;
                    while index < chars.len() {
                        let b = chars[index];
                        index += 1;
                        if ('@'..='~').contains(&b) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    index += 1;
                    while index < chars.len() {
                        if chars[index] == '\u{0007}' {
                            index += 1;
                            break;
                        }
                        if chars[index] == '\u{001b}' && chars.get(index + 1) == Some(&'\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => {}
            }
            stats.removed_ansi += kept.len().saturating_sub(before).max(1);
            continue;
        }
        if ch == '\0' || (ch < ' ' && ch != '\n' && ch != '\t') || ch == '\u{007f}' {
            stats.removed_chars += 1;
            index += 1;
            continue;
        }
        if is_bidi_control(ch) {
            stats.removed_bidi += 1;
            index += 1;
            continue;
        }
        if is_tag_char(ch) {
            stats.removed_tag += 1;
            index += 1;
            continue;
        }
        kept.push(ch);
        index += 1;
    }
    if kept.len() <= limits.max_bytes {
        stats.truncated = false;
        return Normalized { text: kept, stats };
    }
    stats.truncated = true;
    let head_end = floor_char_boundary(&kept, limits.head_bytes);
    let tail_start = ceil_char_boundary(&kept, kept.len().saturating_sub(limits.tail_bytes));
    let capacity = limits
        .head_bytes
        .saturating_add(limits.tail_bytes)
        .saturating_add(32);
    let mut text = String::with_capacity(capacity);
    text.push_str(&kept[..head_end]);
    if tail_start >= head_end {
        text.push_str("\n[…truncated…]\n");
        text.push_str(&kept[tail_start..]);
    } else {
        text.push_str("\n[…truncated…]");
    }
    Normalized { text, stats }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn is_bidi_control(ch: char) -> bool {
    matches!(ch as u32, 0x202A..=0x202E | 0x2066..=0x2069)
}

fn is_tag_char(ch: char) -> bool {
    matches!(ch as u32, 0xE0000..=0xE007F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_content() {
        let text = UntrustedText::new(
            "ignore previous instructions".into(),
            "github:issue.body".into(),
            false,
        );
        let debug = format!("{text:?}");
        assert!(!debug.contains("ignore previous"));
        assert!(debug.contains("github:issue.body"));
    }

    #[test]
    fn strips_controls_and_ansi() {
        let input = "\x1b[31mred\x1b[0m\0\n\tok\u{202e}\u{e0041}".as_bytes();
        let normalized = normalize_untrusted(input, NormalizeLimits::default());
        assert_eq!(normalized.text, "red\n\tok");
        assert!(normalized.stats.removed_ansi >= 2);
        assert_eq!(normalized.stats.removed_chars, 1);
        assert_eq!(normalized.stats.removed_bidi, 1);
        assert_eq!(normalized.stats.removed_tag, 1);
    }

    #[test]
    fn truncates_on_char_boundary() {
        let limits = NormalizeLimits {
            max_bytes: 10,
            head_bytes: 4,
            tail_bytes: 4,
        };
        let normalized = normalize_untrusted("abcdefghijklmno".as_bytes(), limits);
        assert!(normalized.stats.truncated);
        assert!(normalized.text.starts_with("abcd"));
        assert!(normalized.text.ends_with("mno"));
        assert!(normalized.text.contains("truncated"));
    }
}
