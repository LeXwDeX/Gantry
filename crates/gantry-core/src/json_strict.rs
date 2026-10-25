//! 严格 JSON 解析：拒绝重复键、限制深度、拒绝不安全整数与非有限浮点。

use std::collections::BTreeSet;

use serde_json::{Map, Number, Value};

/// 2^53 − 1（api.md §1.5）。
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// worker-protocol §8.2 的默认深度上限。
pub const DEFAULT_MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonLimits {
    pub max_depth: usize,
    pub reject_unsafe_integers: bool,
}

impl Default for JsonLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            reject_unsafe_integers: true,
        }
    }
}

impl JsonLimits {
    pub fn permissive() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            reject_unsafe_integers: false,
        }
    }
}

/// 解析 JSON 文本；所有畸形输入返回错误而不 panic。
pub fn parse_json_strict(bytes: &[u8], opts: JsonLimits) -> Result<Value, JsonError> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(JsonError::new(
            "invalid_utf8",
            "UTF-8 BOM is not allowed",
            0,
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| JsonError::new("invalid_utf8", "input is not valid UTF-8", e.valid_up_to()))?;
    let mut parser = Parser {
        input: text,
        pos: 0,
        limits: opts,
    };
    let value = parser.parse_value(0)?;
    parser.skip_whitespace();
    if parser.pos != text.len() {
        return Err(JsonError::new(
            "trailing_data",
            "unexpected data after the top-level value",
            parser.pos,
        ));
    }
    Ok(value)
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    limits: JsonLimits,
}

impl<'a> Parser<'a> {
    fn parse_value(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'"') => self.parse_string().map(Value::String),
            Some(b't') => {
                self.expect_keyword("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.expect_keyword("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.expect_keyword("null")?;
                Ok(Value::Null)
            }
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("invalid_json", "unexpected character")),
            None => Err(self.error("invalid_json", "unexpected end of input")),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.check_depth(depth)?;
        self.pos += 1; // {
        let mut map = Map::new();
        let mut keys = BTreeSet::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("invalid_json", "object key must be a string"));
            }
            let key_start = self.pos;
            let key = self.parse_string()?;
            if !keys.insert(key.clone()) {
                return Err(JsonError::new(
                    "duplicate_key",
                    format!("duplicate object key: {key}"),
                    key_start,
                ));
            }
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("invalid_json", "missing ':' after object key"));
            }
            self.pos += 1;
            let value = self.parse_value(depth)?;
            map.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("invalid_json", "expected ',' or '}' in object")),
            }
        }
        Ok(Value::Object(map))
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.check_depth(depth)?;
        self.pos += 1; // [
        let mut values = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(values));
        }
        loop {
            let value = self.parse_value(depth)?;
            values.push(value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("invalid_json", "expected ',' or ']' in array")),
            }
        }
        Ok(Value::Array(values))
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        let start = self.pos;
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            match self.next_char() {
                Some('"') => return Ok(out),
                Some('\\') => {
                    let escape = self
                        .next_char()
                        .ok_or_else(|| self.error("invalid_json", "unterminated escape"))?;
                    match escape {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let first = self.read_hex4()?;
                            if (0xD800..=0xDBFF).contains(&first) {
                                if self.next_char() != Some('\\') || self.next_char() != Some('u') {
                                    return Err(
                                        self.error("invalid_json", "unpaired high surrogate")
                                    );
                                }
                                let second = self.read_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&second) {
                                    return Err(self.error("invalid_json", "invalid low surrogate"));
                                }
                                let code = 0x10000
                                    + (((first - 0xD800) as u32) << 10)
                                    + (second - 0xDC00) as u32;
                                let ch = char::from_u32(code).ok_or_else(|| {
                                    self.error("invalid_json", "invalid unicode escape")
                                })?;
                                out.push(ch);
                            } else if (0xDC00..=0xDFFF).contains(&first) {
                                return Err(self.error("invalid_json", "unpaired low surrogate"));
                            } else {
                                let ch = char::from_u32(first as u32).ok_or_else(|| {
                                    self.error("invalid_json", "invalid unicode escape")
                                })?;
                                out.push(ch);
                            }
                        }
                        _ => return Err(self.error("invalid_json", "invalid escape sequence")),
                    }
                }
                Some(ch) if (ch as u32) < 0x20 => {
                    return Err(JsonError::new(
                        "invalid_json",
                        "unescaped control character in string",
                        start,
                    ));
                }
                Some(ch) => out.push(ch),
                None => return Err(JsonError::new("invalid_json", "unterminated string", start)),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("invalid_json", "leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("invalid_json", "invalid number")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid_json", "fraction requires digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid_json", "exponent requires digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let lexeme = &self.input[start..self.pos];
        if is_float {
            let value: f64 = lexeme
                .parse()
                .map_err(|_| self.error("invalid_json", "invalid floating-point number"))?;
            if !value.is_finite() {
                return Err(JsonError::new(
                    "nan_or_infinity",
                    "non-finite numbers are not allowed",
                    start,
                ));
            }
            Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| self.error("nan_or_infinity", "non-finite number"))
        } else if self.limits.reject_unsafe_integers {
            let magnitude = lexeme.trim_start_matches('-');
            let safe = magnitude
                .parse::<u64>()
                .map(|v| v <= MAX_SAFE_INTEGER)
                .unwrap_or(false);
            if !safe {
                return Err(JsonError::new(
                    "unsafe_integer",
                    "integer magnitude exceeds 2^53-1",
                    start,
                ));
            }
            let number: i64 = lexeme
                .parse()
                .map_err(|_| self.error("invalid_json", "integer is out of range"))?;
            Ok(Value::Number(Number::from(number)))
        } else {
            let number: i64 = lexeme
                .parse()
                .map_err(|_| self.error("invalid_json", "integer is out of range"))?;
            Ok(Value::Number(Number::from(number)))
        }
    }

    fn check_depth(&self, depth: usize) -> Result<(), JsonError> {
        if depth > self.limits.max_depth {
            return Err(JsonError::new(
                "depth_exceeded",
                format!("JSON depth exceeds {}", self.limits.max_depth),
                self.pos,
            ));
        }
        Ok(())
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<(), JsonError> {
        if self.input[self.pos..].starts_with(keyword) {
            self.pos += keyword.len();
            Ok(())
        } else {
            Err(self.error("invalid_json", "invalid literal"))
        }
    }

    fn read_hex4(&mut self) -> Result<u16, JsonError> {
        let start = self.pos;
        let mut value = 0u16;
        for _ in 0..4 {
            let ch = self
                .next_char()
                .ok_or_else(|| JsonError::new("invalid_json", "truncated unicode escape", start))?;
            let digit = ch
                .to_digit(16)
                .ok_or_else(|| JsonError::new("invalid_json", "invalid unicode escape", start))?;
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.input[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn error(&self, code: &'static str, message: &'static str) -> JsonError {
        JsonError::new(code, message, self.pos)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct JsonError {
    pub code: &'static str,
    pub message: String,
    pub position: usize,
    pub pointer: Option<String>,
}

impl JsonError {
    pub fn new(code: &'static str, message: impl Into<String>, position: usize) -> Self {
        Self {
            code,
            message: message.into(),
            position,
            pointer: None,
        }
    }
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at byte {}: {}",
            self.code, self.position, self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_keys() {
        let err = parse_json_strict(br#"{"a":1,"a":2}"#, JsonLimits::default()).unwrap_err();
        assert_eq!(err.code, "duplicate_key");
    }

    #[test]
    fn rejects_depth_and_unsafe_integer() {
        let deep = "[".repeat(DEFAULT_MAX_DEPTH + 1) + &"]".repeat(DEFAULT_MAX_DEPTH + 1);
        assert_eq!(
            parse_json_strict(deep.as_bytes(), JsonLimits::default())
                .unwrap_err()
                .code,
            "depth_exceeded"
        );
        assert_eq!(
            parse_json_strict(b"9007199254740992", JsonLimits::default())
                .unwrap_err()
                .code,
            "unsafe_integer"
        );
        assert!(parse_json_strict(b"9007199254740991", JsonLimits::default()).is_ok());
    }

    #[test]
    fn rejects_nan_like_tokens_and_bad_utf8() {
        assert_eq!(
            parse_json_strict(b"NaN", JsonLimits::default())
                .unwrap_err()
                .code,
            "invalid_json"
        );
        assert_eq!(
            parse_json_strict(b"1e9999", JsonLimits::default())
                .unwrap_err()
                .code,
            "nan_or_infinity"
        );
        assert_eq!(
            parse_json_strict(&[0xff], JsonLimits::default())
                .unwrap_err()
                .code,
            "invalid_utf8"
        );
    }

    #[test]
    fn rejects_non_hex_after_unicode_escape_without_panicking() {
        assert_eq!(
            parse_json_strict("\"\\u😀\"".as_bytes(), JsonLimits::default())
                .unwrap_err()
                .code,
            "invalid_json"
        );
    }

    #[test]
    fn roundtrips_unicode_escape_and_surrogate_pair() {
        let value = parse_json_strict(br#""\u00e9\ud83d\ude00""#, JsonLimits::default()).unwrap();
        assert_eq!(value, Value::String("é😀".into()));
    }
}
