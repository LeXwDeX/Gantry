//! 服务端推送事件（SSE）的解析与游标规则（desktop-shell.md §7.3，api.md §5.1–§5.2，P-67）。
//!
//! - [`Parser`]：按 WHATWG 规则增量解析（`id` / `event` / `data` 多行拼接 / `retry` / 注释行 /
//!   CRLF、LF、CR 三种行尾 / 开头的 BOM），字节可以任意切块喂入。
//! - [`StreamState`]：把原始帧变成转发给 UI 的 `StreamFrame` 或连接动作，维护游标：
//!   游标 = 已 ack 的最大 `seq`（事件与心跳取较大者），**先应用、后推进**。

use serde_json::{json, Value};

/// 一个完整的原始帧。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawEvent {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

/// 增量解析器。
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    started: bool,
    /// 上一个字节是 `\r`：紧随的 `\n` 属于同一个行尾（CRLF），可跨块。
    last_was_cr: bool,
    id: Option<String>,
    event: String,
    data: Vec<String>,
    has_data: bool,
    /// 最近一次 `retry:` 的毫秒数。
    pub retry_ms: Option<u64>,
}

/// 单行上限：防止服务端异常时无限占用内存（业务事件远小于此）。
const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct LineTooLong;

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回其中完成的帧。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<RawEvent>, LineTooLong> {
        let mut out = Vec::new();
        for &b in chunk {
            if std::mem::take(&mut self.last_was_cr) && b == b'\n' {
                continue;
            }
            if b == b'\r' || b == b'\n' {
                let line = std::mem::take(&mut self.buf);
                self.line(&line, &mut out);
                self.last_was_cr = b == b'\r';
            } else {
                self.buf.push(b);
                if self.buf.len() > MAX_LINE {
                    return Err(LineTooLong);
                }
            }
        }
        Ok(out)
    }

    fn line(&mut self, raw: &[u8], out: &mut Vec<RawEvent>) {
        let mut line = String::from_utf8_lossy(raw).into_owned();
        if !self.started {
            self.started = true;
            if let Some(rest) = line.strip_prefix('\u{feff}') {
                line = rest.to_string();
            }
        }
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return; // 注释
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f.to_string(), v.strip_prefix(' ').unwrap_or(v).to_string()),
            None => (line.clone(), String::new()),
        };
        match field.as_str() {
            "event" => self.event = value,
            "data" => {
                self.data.push(value);
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.id = Some(value),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                self.retry_ms = value.parse().ok();
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<RawEvent>) {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        if !std::mem::take(&mut self.has_data) {
            return; // 没有 data 的块不派发（WHATWG）。
        }
        out.push(RawEvent {
            id: self.id.clone(),
            event: if event.is_empty() {
                "message".into()
            } else {
                event
            },
            data: data.join("\n"),
        });
    }
}

// ---- 帧分类与游标 ----

/// 服务端 `close` 帧的原因（api.md §5.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseReason {
    DeviceRevoked,
    CertExpired,
    ServerShutdown,
    Lagging,
    Other(String),
}

/// 处理一帧后外壳要做的事。
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// 转发给 UI 的 `StreamFrame`。
    Forward(Value),
    /// 丢弃（重复或协议违规；违规会计数）。
    Drop,
    /// 连续协议违规达到上限：断开重连。
    Reconnect,
    /// 心跳带来的 `log_epoch` 与本地不同（P-67）：断流、清游标、全量重同步。
    LogEpochChanged(u64),
    Close(CloseReason),
}

/// 连续协议违规上限（§9"事件帧协议违规"）。
pub const MAX_VIOLATIONS: u32 = 3;

#[derive(Debug, Default)]
pub struct StreamState {
    /// 已转发的最大 `seq`（事件与心跳）。
    forwarded: Option<u64>,
    /// 已 ack 的最大 `seq`：重连游标。
    acked: Option<u64>,
    violations: u32,
    /// 本地保存的事件日志纪元号。
    pub log_epoch: Option<u64>,
}

impl StreamState {
    /// `after_seq`：UI 快照的最小 `X-Event-Seq`（api.md §5.3），作为初始游标。
    pub fn new(after_seq: Option<u64>, log_epoch: Option<u64>) -> Self {
        Self {
            forwarded: after_seq,
            acked: after_seq,
            violations: 0,
            log_epoch,
        }
    }

    /// 重连用的 `Last-Event-ID`。
    pub fn cursor(&self) -> Option<u64> {
        self.acked
    }

    /// `events_ack(seq)`：UI 已应用到 `seq`。只接受已转发过的、单调递增的值；其余忽略。
    pub fn ack(&mut self, seq: u64) -> bool {
        let within = self.forwarded.is_some_and(|f| seq <= f);
        let advances = self.acked.is_none_or(|a| seq > a);
        if within && advances {
            self.acked = Some(seq);
            true
        } else {
            false
        }
    }

    /// 清游标（`log_epoch` 变化、`cursor_ahead` 后由 UI 重新快照）。
    pub fn reset(&mut self) {
        self.forwarded = None;
        self.acked = None;
        self.violations = 0;
    }

    pub fn handle(&mut self, raw: &RawEvent) -> Action {
        match raw.event.as_str() {
            "heartbeat" => self.heartbeat(raw),
            "close" => {
                let reason: Value = serde_json::from_str(&raw.data).unwrap_or(Value::Null);
                Action::Close(match reason["reason"].as_str().unwrap_or_default() {
                    "device_revoked" => CloseReason::DeviceRevoked,
                    "cert_expired" => CloseReason::CertExpired,
                    "server_shutdown" => CloseReason::ServerShutdown,
                    "lagging" => CloseReason::Lagging,
                    other => CloseReason::Other(other.to_string()),
                })
            }
            _ => self.business(raw),
        }
    }

    fn business(&mut self, raw: &RawEvent) -> Action {
        let Ok(envelope) = serde_json::from_str::<Value>(&raw.data) else {
            return self.violation();
        };
        let seq = envelope["seq"].as_u64();
        let id = raw.id.as_deref().and_then(|i| i.parse::<u64>().ok());
        // `id:` 必须等于 `data.seq`，类型也要对上（api.md §5.1）。
        let (Some(seq), Some(id)) = (seq, id) else {
            return self.violation();
        };
        if seq != id || envelope["type"].as_str() != Some(raw.event.as_str()) {
            return self.violation();
        }
        self.violations = 0;
        // 至少一次语义：`seq` ≤ 已转发的直接丢弃（api.md §5.2）。
        if self.forwarded.is_some_and(|f| seq <= f) {
            return Action::Drop;
        }
        self.forwarded = Some(seq);
        Action::Forward(json!({ "t": "event", "envelope": envelope }))
    }

    fn heartbeat(&mut self, raw: &RawEvent) -> Action {
        let Ok(data) = serde_json::from_str::<Value>(&raw.data) else {
            return self.violation();
        };
        if let Some(epoch) = data["log_epoch"].as_u64() {
            if self.log_epoch.is_some_and(|e| e != epoch) {
                return Action::LogEpochChanged(epoch);
            }
            self.log_epoch = Some(epoch);
        }
        let Some(seq) = raw
            .id
            .as_deref()
            .and_then(|i| i.parse::<u64>().ok())
            .or_else(|| data["seq"].as_u64())
        else {
            return self.violation();
        };
        self.violations = 0;
        // 心跳的 `id` 是服务端已扫描到的最大 `seq`（含过滤掉的），用它推进游标。
        if self.forwarded.is_none_or(|f| seq > f) {
            self.forwarded = Some(seq);
        }
        Action::Forward(json!({ "t": "heartbeat", "seq": seq }))
    }

    fn violation(&mut self) -> Action {
        self.violations += 1;
        if self.violations >= MAX_VIOLATIONS {
            self.violations = 0;
            Action::Reconnect
        } else {
            Action::Drop
        }
    }
}

/// 重连退避（§10 `sse.backoff`）：第 `attempt` 次（从 1 起）在 `[0, min(30, 2^(attempt-1))]` 秒内全抖动；
/// 服务端的 `retry:` 若更大则取它。`unit` 是 `[0, 1)` 的随机数（可注入以便测试）。
pub fn backoff_ms(attempt: u32, retry_ms: Option<u64>, unit: f64) -> u64 {
    let cap_s = 2u64.saturating_pow(attempt.saturating_sub(1)).min(30);
    let jittered = (cap_s as f64 * 1000.0 * unit.clamp(0.0, 1.0)) as u64;
    jittered.max(retry_ms.unwrap_or(0).min(30_000))
}
