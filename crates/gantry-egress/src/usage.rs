//! Anthropic 用量解析（`docs/modules/egress.md` §7.4；字段名以 OQ-E9 为准，需核实当前 API）。
//!
//! 只从上游响应里提取用量字段，其余字节原样透传；解析有界（单行 / 单个 JSON ≤ 1 MiB）。
//! 解析不了或流被截断时按保守估计计入并置 `incomplete`，使"让用量解析失败"不能绕过封顶。

use gantry_core::entities::TokenUsage;
use serde_json::Value;

/// 单行 SSE / 单个 JSON 响应的解析上限。
pub const PARSE_LIMIT: usize = 1024 * 1024;

/// 响应形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Json,
    EventStream,
    /// `count_tokens`：不计 token，只计请求数。
    CountTokens,
}

/// 流式喂入响应字节，结束时给出用量。
#[derive(Debug)]
pub struct UsageParser {
    kind: BodyKind,
    buf: Vec<u8>,
    overflow: bool,
    usage: Option<TokenUsage>,
    stream_done: bool,
    response_bytes: u64,
}

/// 解析结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedUsage {
    pub usage: TokenUsage,
    /// 用量是估计或下界。
    pub incomplete: bool,
}

impl UsageParser {
    pub fn new(kind: BodyKind) -> Self {
        Self {
            kind,
            buf: Vec::new(),
            overflow: false,
            usage: None,
            stream_done: false,
            response_bytes: 0,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.response_bytes += chunk.len() as u64;
        match self.kind {
            BodyKind::CountTokens => {}
            BodyKind::Json => {
                if self.buf.len() + chunk.len() > PARSE_LIMIT {
                    self.overflow = true;
                    self.buf.clear();
                } else if !self.overflow {
                    self.buf.extend_from_slice(chunk);
                }
            }
            BodyKind::EventStream => {
                for byte in chunk {
                    if *byte == b'\n' {
                        let line = std::mem::take(&mut self.buf);
                        if !self.overflow {
                            self.sse_line(&line);
                        }
                        self.overflow = false;
                    } else if self.buf.len() < PARSE_LIMIT {
                        self.buf.push(*byte);
                    } else {
                        self.overflow = true;
                    }
                }
            }
        }
    }

    fn sse_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let Ok(event) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = event.pointer("/message/usage") {
                    self.usage = Some(read_usage(usage));
                }
            }
            Some("message_delta") => {
                if let (Some(current), Some(output)) = (
                    self.usage.as_mut(),
                    event
                        .pointer("/usage/output_tokens")
                        .and_then(Value::as_u64),
                ) {
                    // 累计值，取最后一条。
                    current.output_tokens = output;
                }
            }
            Some("message_stop") => self.stream_done = true,
            _ => {}
        }
    }

    /// 结束解析。`completed`：上游正常结束（未中断）；`success`：2xx。
    /// `request_bytes` 用于保守估计。
    pub fn finish(self, completed: bool, success: bool, request_bytes: u64) -> ParsedUsage {
        if !success {
            // 失败请求只进 upstream_errors 与审计（§7.4）；截断的错误响应同样不计 token。
            return ParsedUsage {
                usage: TokenUsage::default(),
                incomplete: !completed,
            };
        }
        let parsed = match self.kind {
            BodyKind::CountTokens if completed => {
                return ParsedUsage {
                    usage: TokenUsage::new(0, 0, 0, 0, 1),
                    incomplete: false,
                }
            }
            BodyKind::CountTokens => None,
            BodyKind::Json if completed && !self.overflow => {
                serde_json::from_slice::<Value>(&self.buf)
                    .ok()
                    .and_then(|value| value.get("usage").map(read_usage))
            }
            BodyKind::Json => None,
            BodyKind::EventStream if completed && self.stream_done => self.usage,
            BodyKind::EventStream => None,
        };
        match parsed {
            Some(mut usage) => {
                usage.requests = 1;
                ParsedUsage {
                    usage,
                    incomplete: false,
                }
            }
            None => ParsedUsage {
                usage: estimate(request_bytes, self.response_bytes, self.usage),
                incomplete: true,
            },
        }
    }
}

fn read_usage(value: &Value) -> TokenUsage {
    let field = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
    TokenUsage::new(
        field("input_tokens"),
        field("output_tokens"),
        field("cache_read_input_tokens"),
        field("cache_creation_input_tokens"),
        0,
    )
}

/// 保守估计（§7.4 [提议]）：`ceil(请求字节/3)` 计为输入、`ceil(已转发响应字节/3)` 计为输出；
/// 已解析到的部分用量取较大者，使估计不低于已知下界。
fn estimate(request_bytes: u64, response_bytes: u64, partial: Option<TokenUsage>) -> TokenUsage {
    let partial = partial.unwrap_or_default();
    TokenUsage::new(
        request_bytes.div_ceil(3).max(partial.input_tokens),
        response_bytes.div_ceil(3).max(partial.output_tokens),
        partial.cache_read_tokens,
        partial.cache_write_tokens,
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(kind: BodyKind, chunks: &[&[u8]], completed: bool) -> ParsedUsage {
        let mut parser = UsageParser::new(kind);
        for chunk in chunks {
            parser.feed(chunk);
        }
        parser.finish(completed, true, 30)
    }

    #[test]
    fn json_usage_maps_cache_creation_to_cache_write() {
        let body = br#"{"id":"msg","usage":{"input_tokens":3201,"output_tokens":410,"cache_read_input_tokens":2800,"cache_creation_input_tokens":12}}"#;
        let parsed = parse(BodyKind::Json, &[&body[..20], &body[20..]], true);
        assert_eq!(parsed.usage, TokenUsage::new(3201, 410, 2800, 12, 1));
        assert!(!parsed.incomplete);
    }

    #[test]
    fn sse_takes_start_usage_and_the_last_delta() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":2}}}\n\n",
            "event: message_delta\r\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\r\n\r\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes();
        let chunks: Vec<&[u8]> = stream.chunks(13).collect();
        let parsed = parse(BodyKind::EventStream, &chunks, true);
        assert_eq!(parsed.usage, TokenUsage::new(10, 9, 5, 2, 1));
        assert!(!parsed.incomplete);
    }

    #[test]
    fn truncated_streams_are_estimated_and_incomplete() {
        let stream = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n";
        let parsed = parse(BodyKind::EventStream, &[stream], false);
        assert!(parsed.incomplete);
        assert_eq!(parsed.usage.input_tokens, 100, "known lower bound kept");
        assert!(parsed.usage.output_tokens >= stream.len() as u64 / 3);
        // 没有 message_stop 就结束，同样按截断处理。
        assert!(parse(BodyKind::EventStream, &[stream], true).incomplete);
    }

    #[test]
    fn unparseable_or_oversized_json_is_estimated() {
        let parsed = parse(BodyKind::Json, &[b"not json"], true);
        assert!(parsed.incomplete);
        assert_eq!(parsed.usage, TokenUsage::new(10, 3, 0, 0, 1));
        let big = vec![b' '; PARSE_LIMIT + 1];
        assert!(parse(BodyKind::Json, &[&big], true).incomplete);
    }

    #[test]
    fn count_tokens_counts_only_the_request() {
        let parsed = parse(BodyKind::CountTokens, &[br#"{"input_tokens":99}"#], true);
        assert_eq!(parsed.usage, TokenUsage::new(0, 0, 0, 0, 1));
    }

    #[test]
    fn failed_requests_carry_no_tokens() {
        let mut parser = UsageParser::new(BodyKind::Json);
        parser.feed(br#"{"type":"error","usage":{"input_tokens":5}}"#);
        let parsed = parser.finish(true, false, 100);
        assert_eq!(parsed.usage, TokenUsage::default());
        assert!(!parsed.incomplete);
    }

    #[test]
    fn overlong_sse_lines_are_skipped() {
        let mut stream = b"data: ".to_vec();
        stream.extend(vec![b'x'; PARSE_LIMIT + 10]);
        stream.extend_from_slice(b"\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\ndata: {\"type\":\"message_stop\"}\n");
        let parsed = parse(BodyKind::EventStream, &[&stream], true);
        assert_eq!(parsed.usage, TokenUsage::new(3, 0, 0, 0, 1));
    }
}
