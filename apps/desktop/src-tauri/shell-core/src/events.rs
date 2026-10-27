//! 事件流运行器与连接状态（desktop-shell.md §7.3、§9，api.md §5，P-67）。
//!
//! 整个客户端只有一条 `GET /v1/events`。运行器在后台任务里：连接 → 逐块解析 → 按 [`StreamState`]
//! 转发帧或采取动作 → 断线按退避重连（带已 ack 的游标）。连接状态的每次变化以 `state` 帧推给 UI。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::http::NetError;
use crate::session::Shell;
use crate::sse::{backoff_ms, Action, CloseReason, Parser, StreamState};

/// 帧的去处（Tauri `Channel<StreamFrame>`；测试里是收集器）。返回 `false` 表示对端已关闭。
pub trait FrameSink: Send + Sync {
    fn send(&self, frame: Value) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Unpaired,
    Connecting,
    Live,
    Reconnecting,
    HandshakeFailed,
    Revoked,
    CertExpired,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unpaired => "unpaired",
            Self::Connecting => "connecting",
            Self::Live => "live",
            Self::Reconnecting => "reconnecting",
            Self::HandshakeFailed => "handshake_failed",
            Self::Revoked => "revoked",
            Self::CertExpired => "cert_expired",
        }
    }
}

/// 合并后的连接健康（§6.2 `ConnectionState`）。
#[derive(Clone, Debug)]
pub struct ConnState {
    pub phase: Phase,
    /// 每次（重）连 +1。
    pub epoch: u64,
    pub last_bytes_at: Option<Instant>,
    pub cert_not_after: Option<String>,
    pub cert_expiring: bool,
    pub log_epoch: Option<u64>,
    pub clock_skew_ms: Option<i64>,
}

impl Default for ConnState {
    fn default() -> Self {
        Self {
            phase: Phase::Unpaired,
            epoch: 0,
            last_bytes_at: None,
            cert_not_after: None,
            cert_expiring: false,
            log_epoch: None,
            clock_skew_ms: None,
        }
    }
}

impl ConnState {
    pub fn to_json(&self) -> Value {
        json!({
            "phase": self.phase.as_str(),
            "epoch": self.epoch,
            "last_bytes_age_ms": self.last_bytes_at.map(|t| t.elapsed().as_millis() as u64),
            "cert_not_after": self.cert_not_after,
            "cert_expiring": self.cert_expiring,
            "log_epoch": self.log_epoch,
            "clock_skew_ms": self.clock_skew_ms,
        })
    }
}

/// §10 的事件流参数；测试可缩短。
#[derive(Clone, Debug)]
pub struct EventsConfig {
    /// `sse.idle_ms`：这么久没有任何字节就断开重连。
    pub idle: Duration,
    /// 退避时间乘以此系数（测试用 0.001 之类加速）。
    pub backoff_scale: f64,
    /// 连续 TLS 层失败达到此数 → `handshake_failed`。
    pub handshake_fail_limit: u32,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            idle: Duration::from_millis(45_000),
            backoff_scale: 1.0,
            handshake_fail_limit: 3,
        }
    }
}

/// 一条打开中的事件流。
pub struct StreamHandle {
    pub state: Arc<Mutex<StreamState>>,
    task: tokio::task::JoinHandle<()>,
}

impl StreamHandle {
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum Outcome {
    /// 按退避重连。
    Retry,
    /// 立即重连（`lagging`、协议违规）。
    RetryNow,
    /// 停止（等 UI 重新 `events_open` 或用户处理）。
    Stop,
}

pub(crate) fn spawn(
    shell: Arc<Shell>,
    state: Arc<Mutex<StreamState>>,
    sink: Arc<dyn FrameSink>,
    config: EventsConfig,
) -> StreamHandle {
    let task_state = state.clone();
    let task = tokio::spawn(async move { run(shell, task_state, sink, config).await });
    StreamHandle { state, task }
}

fn rand_unit() -> f64 {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    u32::from_le_bytes(b) as f64 / (u32::MAX as f64 + 1.0)
}

async fn run(
    shell: Arc<Shell>,
    state: Arc<Mutex<StreamState>>,
    sink: Arc<dyn FrameSink>,
    config: EventsConfig,
) {
    let mut attempt: u32 = 0;
    let mut tls_failures: u32 = 0;
    let mut retry_ms: Option<u64> = None;
    let mut first = true;
    loop {
        shell.set_phase(
            if first {
                Phase::Connecting
            } else {
                Phase::Reconnecting
            },
            &*sink,
        );
        first = false;
        let outcome = connect_once(
            &shell,
            &state,
            &*sink,
            &config,
            &mut tls_failures,
            &mut retry_ms,
            &mut attempt,
        )
        .await;
        match outcome {
            Outcome::Stop => return,
            Outcome::RetryNow => attempt = 0,
            Outcome::Retry => {
                attempt += 1;
                let ms = backoff_ms(attempt, retry_ms, rand_unit()) as f64 * config.backoff_scale;
                tokio::time::sleep(Duration::from_millis(ms as u64)).await;
            }
        }
        if !sink.send(json!({ "t": "state", "state": shell.connection_state() })) {
            return; // UI 已关闭通道。
        }
    }
}

async fn connect_once(
    shell: &Arc<Shell>,
    state: &Arc<Mutex<StreamState>>,
    sink: &dyn FrameSink,
    config: &EventsConfig,
    tls_failures: &mut u32,
    retry_ms: &mut Option<u64>,
    attempt: &mut u32,
) -> Outcome {
    let Ok(connected) = shell.connected() else {
        shell.set_phase(Phase::Unpaired, sink);
        return Outcome::Stop;
    };
    let cursor = state.lock().unwrap().cursor().map(|c| c.to_string());
    let mut headers = vec![("accept", "text/event-stream")];
    if let Some(c) = cursor.as_deref() {
        headers.push(("last-event-id", c));
    }
    let mut resp = match connected.client.open_stream("/v1/events", &headers).await {
        Ok(resp) => resp,
        Err(NetError::Tls) => {
            *tls_failures += 1;
            if *tls_failures >= config.handshake_fail_limit {
                // 可能已被吊销或证书过期；不自动清除密钥，由用户 identity_reset（api.md §2.1）。
                shell.set_phase(Phase::HandshakeFailed, sink);
                return Outcome::Stop;
            }
            return Outcome::Retry;
        }
        Err(_) => return Outcome::Retry,
    };
    *tls_failures = 0;
    if let Some(epoch) = resp
        .header("x-log-epoch")
        .and_then(|v| v.trim().parse().ok())
    {
        if epoch_changed(shell, state, sink, epoch) {
            return Outcome::Stop;
        }
    }
    match resp.status {
        200 => {}
        409 | 410 => {
            let problem: Value = serde_json::from_slice(&resp.collect().await).unwrap_or_default();
            let reason = match problem["code"].as_str() {
                Some("cursor_expired") => "cursor_expired",
                _ => "cursor_ahead",
            };
            // 不带旧游标重试：由 UI 清缓存、重新快照后再 `events_open`。
            sink.send(json!({ "t": "resync_required", "reason": reason }));
            return Outcome::Stop;
        }
        401 => {
            shell.mark_revoked(sink);
            return Outcome::Stop;
        }
        429 | 503 => {
            *retry_ms = resp
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| s * 1000);
            return Outcome::Retry;
        }
        _ => return Outcome::Retry,
    }

    shell.on_stream_open(sink);
    *attempt = 0;
    let mut parser = Parser::new();
    loop {
        let chunk = match tokio::time::timeout(config.idle, resp.next_chunk()).await {
            Err(_) => return Outcome::Retry, // 45 s 无字节
            Ok(Err(_)) | Ok(Ok(None)) => return Outcome::Retry,
            Ok(Ok(Some(chunk))) => chunk,
        };
        shell.touch_bytes();
        let Ok(frames) = parser.feed(&chunk) else {
            return Outcome::RetryNow;
        };
        if parser.retry_ms.is_some() {
            *retry_ms = parser.retry_ms;
        }
        for raw in frames {
            let action = state.lock().unwrap().handle(&raw);
            match action {
                Action::Forward(frame) => {
                    if !sink.send(frame) {
                        return Outcome::Stop;
                    }
                }
                Action::Drop => {}
                Action::Reconnect => return Outcome::RetryNow,
                Action::LogEpochChanged(epoch) => {
                    epoch_changed(shell, state, sink, epoch);
                    return Outcome::Stop;
                }
                Action::Close(reason) => {
                    return match reason {
                        CloseReason::DeviceRevoked => {
                            shell.mark_revoked(sink);
                            Outcome::Stop
                        }
                        CloseReason::CertExpired => {
                            shell.set_phase(Phase::CertExpired, sink);
                            Outcome::Stop
                        }
                        CloseReason::Lagging => Outcome::RetryNow,
                        CloseReason::ServerShutdown | CloseReason::Other(_) => Outcome::Retry,
                    };
                }
            }
        }
    }
}

/// 纪元号变化（P-67）：断流、清游标、丢弃待签内容、保存新值、发 `resync_required`。返回是否变化。
fn epoch_changed(
    shell: &Arc<Shell>,
    state: &Arc<Mutex<StreamState>>,
    sink: &dyn FrameSink,
    epoch: u64,
) -> bool {
    let changed = shell.observe_log_epoch(epoch).unwrap_or(false);
    let mut st = state.lock().unwrap();
    st.log_epoch = Some(epoch);
    if changed {
        st.reset();
        drop(st);
        sink.send(json!({ "t": "resync_required", "reason": "log_epoch_changed" }));
    }
    changed
}
