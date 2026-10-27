//! 事件流 `GET /v1/events`（#41，api.md §5，本文 §7.9）。
//!
//! **入口检查**（返回 `200` 之前，失败按普通 problem）：每设备并发上限 → 参数
//! （`project_id` 可重复，`types` 逗号分隔，精确类型或 `前缀.*`）→ 游标
//! （`Last-Event-ID` 优先于 `after`，十进制 `u64`；大于 head → `409 cursor_ahead`）。
//!
//! **流**：一个连接一个后台任务。回放与实时的衔接用 `EventLog::scan`：每次都从
//! 已扫描位置 `scanned` 继续扫，因此天然不丢不重；store 的 `tail`（store.md §7.5）
//! 落地后替换轮询，接口不变。过滤分两层：`project_id` 交给 store；类型、手机白名单
//! 与 `settings.*` 门禁在**发送时刻**判定，角色在每次心跳重新读取。心跳携带已扫描
//! 到的 `seq`（含被过滤的事件）与 `log_epoch`，同时检查设备吊销、证书到期与关停。

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use gantry_core::entities::{DeviceRole, DeviceStatus, Seq};
use gantry_core::events::{Event, EventType};
use gantry_core::ids::{DeviceId, ProjectId};
use gantry_core::ports::ClientProfile;
use gantry_core::time::Timestamp;
use gantry_store::{EventFilter, StoreError, TypePattern};
use http::header::{HeaderName, HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use http::StatusCode;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use crate::auth::role_satisfies;
use crate::consts::{SSE_HEARTBEAT, SSE_MAX_LAG_EVENTS, SSE_MAX_STREAMS_PER_DEVICE, SSE_RETRY_MS};
use crate::problem::{Problem, ProblemCode};
use crate::server::{ApiCore, HandlerRequest, Reply, Services};

/// 回放 / 实时的轮询间隔（store 尚无 `tail` 推送）。
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 每次扫描的批量上限（store 的 `scan` 上限）。
const SCAN_BATCH: u32 = 200;
/// 发送端有界缓冲（帧数）与写超时（本文 §7.9 第 5 步；字节上限按平均帧大小折算）。
pub const SEND_BUFFER_FRAMES: usize = 1024;
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);

const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");
const X_ACCEL_BUFFERING: HeaderName = HeaderName::from_static("x-accel-buffering");

/// 手机档案的事件类型白名单（api.md §2.3）。
const MOBILE_PREFIXES: [&str; 4] = ["project.", "conversation.", "message.", "inbox."];

/// 事件流的响应体：后台任务产出的 SSE 帧。
pub struct EventStream {
    rx: mpsc::Receiver<Bytes>,
}

impl EventStream {
    /// 下一帧；流结束（含 `close` 帧之后）返回 `None`。
    pub async fn next_frame(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }

    /// 供传输层（hyper 响应体）轮询下一帧。
    pub fn poll_frame(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Bytes>> {
        self.rx.poll_recv(cx)
    }
}

/// 每设备并发流计数；许可在任务结束时释放。
#[derive(Default)]
pub struct StreamRegistry {
    counts: Mutex<HashMap<DeviceId, usize>>,
}

pub struct StreamPermit {
    registry: Arc<StreamRegistry>,
    device: DeviceId,
}

impl StreamRegistry {
    fn acquire(self: &Arc<Self>, device: DeviceId) -> Option<StreamPermit> {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        let count = counts.entry(device).or_insert(0);
        if *count >= SSE_MAX_STREAMS_PER_DEVICE {
            return None;
        }
        *count += 1;
        Some(StreamPermit {
            registry: self.clone(),
            device,
        })
    }
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        let mut counts = self
            .registry
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(count) = counts.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.device);
            }
        }
    }
}

/// 发送时刻的类型判定。
#[derive(Clone, Debug)]
struct TypeGate {
    requested: Vec<TypePattern>,
    profile: ClientProfile,
    role: DeviceRole,
}

impl TypeGate {
    fn allows(&self, ty: EventType) -> bool {
        let name = ty.as_str();
        if !self.requested.is_empty() && !self.requested.iter().any(|p| p.matches(ty)) {
            return false;
        }
        if name.starts_with("settings.") {
            // settings.* 只对桌面档案且角色 ≥ operator 的流发送（api.md §5.4）
            return self.profile == ClientProfile::Desktop
                && role_satisfies(self.role, DeviceRole::Operator);
        }
        match self.profile {
            ClientProfile::Desktop => true,
            ClientProfile::Mobile => {
                ty == EventType::DeviceRevoked
                    || MOBILE_PREFIXES.iter().any(|p| name.starts_with(p))
            }
        }
    }
}

fn invalid_cursor() -> Problem {
    Problem::new(
        ProblemCode::InvalidCursor,
        "cursor must be a decimal sequence number",
    )
}

fn unavailable() -> Problem {
    Problem::retry_after(ProblemCode::Unavailable, "store unavailable", 1)
}

/// 游标：`Last-Event-ID` 头优先于 `after=`（api.md §5.2）。
fn cursor(req: &HandlerRequest<'_>) -> Result<Option<Seq>, Problem> {
    if let Some(value) = req.headers.get(LAST_EVENT_ID) {
        let text = value.to_str().map_err(|_| invalid_cursor())?.trim();
        let valid = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        let n: u64 = if valid { text.parse().ok() } else { None }.ok_or_else(invalid_cursor)?;
        return Seq::from_wire(n).map(Some).map_err(|_| invalid_cursor());
    }
    match req.query.int("after") {
        Some(n) => Seq::from_wire(n).map(Some).map_err(|_| invalid_cursor()),
        None => Ok(None),
    }
}

fn type_patterns(req: &HandlerRequest<'_>) -> Result<Vec<TypePattern>, Problem> {
    let Some(raw) = req.query.str("types") else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            TypePattern::parse(s)
                .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "types is invalid"))
        })
        .collect()
}

/// #41 的入口：检查通过后启动后台任务并返回 `200 text/event-stream`。
pub async fn stream_events(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let device = req.auth.device.clone();
    let Some(permit) = core.streams().acquire(device.id) else {
        return Err(Problem::retry_after(
            ProblemCode::RateLimited,
            "too many concurrent event streams for this device",
            SSE_HEARTBEAT.as_secs(),
        ));
    };
    let requested = type_patterns(req)?;
    let projects: BTreeSet<ProjectId> = req
        .query
        .strs("project_id")
        .into_iter()
        .map(|p| {
            p.parse()
                .map_err(|_| Problem::new(ProblemCode::InvalidRequest, "project_id is invalid"))
        })
        .collect::<Result<_, _>>()?;
    let after = cursor(req)?;

    let svc = core.services().clone();
    let events = svc.store.events();
    let head = events.head().await.map_err(|_| unavailable())?;
    if let Some(after) = after {
        match events.check_cursor(after).await {
            Ok(()) => {}
            Err(StoreError::CursorAhead { .. }) => {
                return Err(Problem::new(
                    ProblemCode::CursorAhead,
                    "cursor is ahead of the event log",
                ))
            }
            Err(_) => return Err(unavailable()),
        }
    }

    let (tx, rx) = mpsc::channel(SEND_BUFFER_FRAMES);
    let task = StreamTask {
        svc,
        device: device.id,
        gate: TypeGate {
            requested,
            profile: req.auth.profile,
            role: device.role,
        },
        filter: EventFilter {
            project_ids: (!projects.is_empty()).then_some(projects),
            include: Vec::new(),
            exclude: Vec::new(),
        },
        cert_not_after: req.auth.cert.not_after,
        shutdown: core.shutdown_signal(),
        tx,
    };
    tokio::spawn(task.run(after, head, permit));

    let mut reply = Reply::events(EventStream { rx }, head);
    reply.headers.extend([
        (
            CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        ),
        (CACHE_CONTROL, HeaderValue::from_static("no-store")),
        (X_ACCEL_BUFFERING, HeaderValue::from_static("no")),
    ]);
    reply.status = StatusCode::OK;
    Ok(reply)
}

/// 关闭原因（api.md §5.5，`close` 帧）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    DeviceRevoked,
    CertExpired,
    Lagging,
    ServerShutdown,
}

impl CloseReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::DeviceRevoked => "device_revoked",
            Self::CertExpired => "cert_expired",
            Self::Lagging => "lagging",
            Self::ServerShutdown => "server_shutdown",
        }
    }
}

struct StreamTask {
    svc: Services,
    device: DeviceId,
    gate: TypeGate,
    filter: EventFilter,
    cert_not_after: Timestamp,
    shutdown: watch::Receiver<bool>,
    tx: mpsc::Sender<Bytes>,
}

/// 发送失败：客户端已断开，或缓冲写满且超时（慢消费者）。
enum SendError {
    Gone,
    Lagging,
}

impl StreamTask {
    async fn run(mut self, after: Option<Seq>, head: Seq, _permit: StreamPermit) {
        if self
            .send_raw(format!("retry: {SSE_RETRY_MS}\n\n"))
            .await
            .is_err()
        {
            return;
        }
        // 无游标：不回放，首帧是带当前 head 的心跳。
        let mut scanned = match after {
            Some(after) => after,
            None => {
                if self.heartbeat(head).await.is_err() {
                    return;
                }
                head
            }
        };
        let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT);
        heartbeat.tick().await;
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        loop {
            tokio::select! {
                changed = self.shutdown.changed() => {
                    if changed.is_err() || *self.shutdown.borrow() {
                        self.close(CloseReason::ServerShutdown).await;
                        return;
                    }
                }
                _ = heartbeat.tick() => {
                    if let Some(reason) = self.check_device().await {
                        self.close(reason).await;
                        return;
                    }
                    match self.heartbeat(scanned).await {
                        Ok(()) => {}
                        Err(SendError::Gone) => return,
                        Err(SendError::Lagging) => {
                            self.close(CloseReason::Lagging).await;
                            return;
                        }
                    }
                }
                _ = poll.tick() => {
                    if self.svc.clock.now() >= self.cert_not_after {
                        self.close(CloseReason::CertExpired).await;
                        return;
                    }
                    match self.pump(&mut scanned).await {
                        Ok(()) => {}
                        Err(Some(reason)) => {
                            self.close(reason).await;
                            return;
                        }
                        Err(None) => return,
                    }
                }
            }
        }
    }

    /// 从 `scanned` 继续扫到 head，逐个发送通过判定的事件。
    async fn pump(&mut self, scanned: &mut Seq) -> Result<(), Option<CloseReason>> {
        let events = self.svc.store.events();
        loop {
            // 事件日志读失败：关闭该流，客户端按退避重连（本文 §9）。
            let page = events
                .scan(*scanned, &self.filter, SCAN_BATCH)
                .await
                .map_err(|_| None)?;
            if page.head.as_u64().saturating_sub(scanned.as_u64()) > SSE_MAX_LAG_EVENTS {
                return Err(Some(CloseReason::Lagging));
            }
            for event in &page.events {
                if self.gate.allows(event.ty) {
                    match self.send_event(event).await {
                        Ok(()) => {}
                        Err(SendError::Gone) => return Err(None),
                        Err(SendError::Lagging) => return Err(Some(CloseReason::Lagging)),
                    }
                }
                *scanned = event.seq;
            }
            *scanned = (*scanned).max(page.scanned_to);
            if page.scanned_to >= page.head {
                return Ok(());
            }
        }
    }

    /// 心跳时实时读设备：吊销 → 关闭；角色变化 → 之后的发送按新角色判定。
    async fn check_device(&mut self) -> Option<CloseReason> {
        let status = match self.svc.devices.get(&self.device).await {
            Ok(Some(device)) if device.status == DeviceStatus::Active => {
                self.gate.role = device.role;
                None
            }
            Ok(_) => Some(CloseReason::DeviceRevoked),
            // 目录暂不可用：本次跳过，下一次心跳再查（握手仍会拒绝已吊销设备）。
            Err(_) => None,
        };
        status.or_else(|| {
            (self.svc.clock.now() >= self.cert_not_after).then_some(CloseReason::CertExpired)
        })
    }

    async fn heartbeat(&self, scanned: Seq) -> Result<(), SendError> {
        let data = json!({
            "seq": scanned.as_u64(),
            "ts": self.svc.clock.now().to_canonical_string(),
            "log_epoch": self.svc.store.log_identity().log_epoch,
        });
        self.send_raw(format!(
            "event: heartbeat\nid: {}\ndata: {}\n\n",
            scanned.as_u64(),
            data
        ))
        .await
    }

    async fn send_event(&self, event: &Event<Value>) -> Result<(), SendError> {
        let data = serde_json::to_string(event).map_err(|_| SendError::Gone)?;
        self.send_raw(format!(
            "id: {}\nevent: {}\ndata: {data}\n\n",
            event.seq.as_u64(),
            event.ty.as_str()
        ))
        .await
    }

    async fn close(&self, reason: CloseReason) {
        // close 帧尽力发送，不阻塞关闭（本文 §7.9 第 7 步）。
        let frame = format!(
            "event: close\ndata: {}\n\n",
            json!({ "reason": reason.as_str() })
        );
        let _ = self.tx.try_send(Bytes::from(frame));
    }

    /// 普通帧始终给 `close` 帧留一格：缓冲只剩一格时等待，写超时即判定慢消费者。
    async fn send_raw(&self, frame: String) -> Result<(), SendError> {
        let wait_for_room = async {
            loop {
                if self.tx.is_closed() {
                    return Err(SendError::Gone);
                }
                if self.tx.capacity() > 1 {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        match tokio::time::timeout(SEND_TIMEOUT, wait_for_room).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(SendError::Lagging),
        }
        self.tx
            .try_send(Bytes::from(frame))
            .map_err(|_| SendError::Gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(profile: ClientProfile, role: DeviceRole, requested: &[&str]) -> TypeGate {
        TypeGate {
            requested: requested
                .iter()
                .map(|s| TypePattern::parse(s).unwrap())
                .collect(),
            profile,
            role,
        }
    }

    #[test]
    fn settings_gate_and_mobile_whitelist() {
        let desktop_op = gate(ClientProfile::Desktop, DeviceRole::Operator, &[]);
        let desktop_viewer = gate(ClientProfile::Desktop, DeviceRole::Viewer, &[]);
        let mobile_op = gate(ClientProfile::Mobile, DeviceRole::Operator, &[]);
        assert!(desktop_op.allows(EventType::SettingsUpdated));
        assert!(!desktop_viewer.allows(EventType::SettingsUpdated));
        assert!(!mobile_op.allows(EventType::SettingsUpdated));
        assert!(mobile_op.allows(EventType::MessagePosted));
        assert!(mobile_op.allows(EventType::DeviceRevoked));
        assert!(!mobile_op.allows(EventType::DeliveryCreated));
        // 请求了白名单外的类型：不报错，只是收不到
        let mobile_delivery = gate(ClientProfile::Mobile, DeviceRole::Operator, &["delivery.*"]);
        assert!(!mobile_delivery.allows(EventType::DeliveryCreated));
        assert!(!mobile_delivery.allows(EventType::MessagePosted));
        let desktop_inbox = gate(
            ClientProfile::Desktop,
            DeviceRole::Viewer,
            &["inbox.*", "message.posted"],
        );
        assert!(desktop_inbox.allows(EventType::MessagePosted));
        assert!(!desktop_inbox.allows(EventType::DeliveryCreated));
    }
}
