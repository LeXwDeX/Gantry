//! 设备名校验与终端输出净化（pki.md §7.4.3 第 4 步、§7.5 重命名、§8 终端注入）。

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("device name must not be empty")]
    Empty,
    #[error("device name exceeds {limit} characters")]
    TooLong { limit: usize },
    #[error("device name contains control characters")]
    Control,
}

/// 设备名：1–`max_chars` 个字符（按 Unicode 标量计），不含控制字符与换行。
///
/// 配对与重命名共用（`pki.device_name_max_chars`，默认 64）。
pub fn validate_device_name(name: &str, max_chars: usize) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > max_chars {
        return Err(NameError::TooLong { limit: max_chars });
    }
    if name.chars().any(is_unsafe_char) {
        return Err(NameError::Control);
    }
    Ok(())
}

/// 打印到管理员终端前的净化：剥离 ANSI / VT 转义序列（CSI、OSC 等）与所有控制字符。
///
/// 输入来自未认证方（配对请求里的设备名），也用于 `gantry device recover list`。
pub fn sanitize_for_terminal(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            // C1 的 CSI / OSC / DCS 等单字节引导符，与 ESC 序列同样处理。
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars),
            c if is_unsafe_char(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// 控制字符（C0、DEL、C1）以及会改变显示方向或行结构的格式字符。
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn skip_escape(chars: &mut Chars<'_>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        Some(']' | 'P' | 'X' | '^' | '_') => skip_string(chars),
        // 两字节序列（如 ESC c、ESC 7）或中间字节序列（ESC ( B）：吞掉中间字节与终止字节。
        Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
            while let Some(&next) = chars.peek() {
                chars.next();
                if !('\u{20}'..='\u{2f}').contains(&next) {
                    break;
                }
            }
        }
        _ => {}
    }
}

/// CSI：参数与中间字节之后，以 0x40–0x7e 结束。
fn skip_csi(chars: &mut Chars<'_>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

/// OSC / DCS / SOS / PM / APC：到 BEL、ST（ESC \）或 C1 ST 为止。
fn skip_string(chars: &mut Chars<'_>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{7}' | '\u{9c}' => break,
            '\u{1b}' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        assert_eq!(validate_device_name("", 64), Err(NameError::Empty));
        assert_eq!(validate_device_name(&"a".repeat(64), 64), Ok(()));
        assert_eq!(
            validate_device_name(&"a".repeat(65), 64),
            Err(NameError::TooLong { limit: 64 })
        );
        // 按字符计：64 个汉字合法。
        assert_eq!(validate_device_name(&"机".repeat(64), 64), Ok(()));
        for bad in [
            "a\nb",
            "a\rb",
            "a\tb",
            "a\u{1b}[31mb",
            "a\u{7f}",
            "a\u{85}",
            "a\u{202e}b",
        ] {
            assert_eq!(
                validate_device_name(bad, 64),
                Err(NameError::Control),
                "{bad:?}"
            );
        }
        assert_eq!(validate_device_name("Lex 的 MacBook Pro", 64), Ok(()));
    }

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        let cases = [
            ("plain name", "plain name"),
            ("red\u{1b}[31;1mALERT\u{1b}[0m", "redALERT"),
            ("title\u{1b}]0;pwned\u{7}x", "titlex"),
            (
                "link\u{1b}]8;;http://x\u{1b}\\text\u{1b}]8;;\u{1b}\\",
                "linktext",
            ),
            ("line1\nline2\r\n", "line1line2"),
            ("c1\u{9b}2Jcsi", "c1csi"),
            ("reset\u{1b}cdone", "resetdone"),
            ("charset\u{1b}(Bok", "charsetok"),
            ("bidi\u{202e}txt", "biditxt"),
            ("trailing\u{1b}", "trailing"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_for_terminal(input), expected, "{input:?}");
        }
    }
}
