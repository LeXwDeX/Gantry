//! 事件流 #41（api.md §5，本文 §12.2「事件流」的 M1 条目）。
//!
//! 心跳与轮询用 tokio 的暂停时钟（`start_paused`）推进；证书到期用 `FakeClock`。

mod support;

use std::time::Duration;

use gantry_api::sse::EventStream;
use gantry_core::entities::{Device, DevicePlatform, DeviceRole, DeviceStatus, Seq};
use gantry_core::events::EventType;
use gantry_core::ids::{MessageId, ProjectId};
use http::Method;
use serde_json::{json, Value};
use support::doubles::{fixture_data, record};
use support::*;

#[derive(Debug, Default, Clone, PartialEq)]
struct Frame {
    event: Option<String>,
    id: Option<String>,
    data: Option<String>,
    retry: Option<String>,
}

impl Frame {
    fn json(&self) -> Value {
        serde_json::from_str(self.data.as_deref().unwrap()).unwrap()
    }
}

fn parse(raw: &[u8]) -> Frame {
    let text = std::str::from_utf8(raw).unwrap();
    assert!(
        text.ends_with("\n\n"),
        "frame must end with a blank line: {text:?}"
    );
    let mut f = Frame::default();
    for line in text.trim_end().lines() {
        let (k, v) = line.split_once(": ").unwrap();
        let slot = match k {
            "event" => &mut f.event,
            "id" => &mut f.id,
            "data" => &mut f.data,
            "retry" => &mut f.retry,
            other => panic!("unexpected field {other}"),
        };
        assert!(slot.is_none(), "duplicate field {k}");
        *slot = Some(v.to_owned());
    }
    f
}

struct Client {
    stream: EventStream,
}

impl Client {
    async fn next(&mut self) -> Option<Frame> {
        tokio::time::timeout(Duration::from_secs(120), self.stream.next_frame())
            .await
            .expect("no frame within 120 s of virtual time")
            .map(|b| parse(&b))
    }

    /// 下一个非心跳帧。
    async fn next_event(&mut self) -> Option<Frame> {
        loop {
            let f = self.next().await?;
            if f.event.as_deref() != Some("heartbeat") {
                return Some(f);
            }
        }
    }

    async fn next_heartbeat(&mut self) -> Frame {
        loop {
            let f = self.next().await.expect("stream ended");
            if f.event.as_deref() == Some("heartbeat") {
                return f;
            }
        }
    }
}

async fn open(h: &Harness, head: gantry_api::RequestHead) -> Client {
    let resp = h.core.handle(head, bytes::Bytes::new()).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.body());
    assert_eq!(
        header(&resp, "content-type"),
        Some("text/event-stream; charset=utf-8")
    );
    assert_eq!(header(&resp, "cache-control"), Some("no-store"));
    assert_eq!(header(&resp, "x-accel-buffering"), Some("no"));
    assert!(header(&resp, "x-log-epoch").is_some());
    let mut client = Client {
        stream: resp.into_body().into_events().expect("event stream body"),
    };
    let first = client.next().await.unwrap();
    assert_eq!(
        first.retry.as_deref(),
        Some("3000"),
        "retry frame comes first"
    );
    client
}

fn viewer(h: &Harness, n: u128, platform: DevicePlatform, role: DeviceRole) -> Device {
    let d = device(n, platform, role);
    h.devices.insert(d.clone());
    d
}

/// 追加一条 `message.posted`（可指定项目）。
async fn message(h: &Harness, n: u128, project: Option<ProjectId>) -> Seq {
    let id = MessageId::from_parts(T0 as u64, 1000 + n).unwrap();
    record(
        &h.store,
        EventType::MessagePosted,
        &id.to_string(),
        project,
        fixture_data("message.posted"),
        None,
    )
    .await
}

/// 追加一次全局 `levels` 写入；`base_rev` 取上一次写入的 seq（从未写过为 0）。
async fn settings(h: &Harness, base_rev: Seq) -> Seq {
    let mut data = fixture_data("settings.updated");
    data["base_rev"] = json!(base_rev.as_u64());
    record(
        &h.store,
        EventType::SettingsUpdated,
        "settings",
        None,
        data,
        None,
    )
    .await
}

async fn envelope(h: &Harness, seq: Seq) -> String {
    let event = h.store.events().get_by_seq(seq).await.unwrap().unwrap();
    serde_json::to_string(&event).unwrap()
}

#[tokio::test(start_paused = true)]
async fn without_cursor_starts_with_head_heartbeat_then_streams_live_events() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let old = message(&h, 1, None).await;
    let mut c = open(&h, req(Method::GET, "/v1/events", &d)).await;
    let hb = c.next().await.unwrap();
    assert_eq!(hb.event.as_deref(), Some("heartbeat"));
    assert_eq!(hb.id.as_deref(), Some(old.as_u64().to_string().as_str()));
    assert_eq!(hb.json()["seq"], old.as_u64());
    assert_eq!(hb.json()["log_epoch"], 1);
    // 不回放历史；之后追加的事件实时到达，data 与 store 中的信封字节一致
    let live = message(&h, 2, None).await;
    let f = c.next_event().await.unwrap();
    assert_eq!(f.id.as_deref(), Some(live.as_u64().to_string().as_str()));
    assert_eq!(f.event.as_deref(), Some("message.posted"));
    assert_eq!(f.data.as_deref(), Some(envelope(&h, live).await.as_str()));
}

#[tokio::test(start_paused = true)]
async fn resumes_from_last_event_id_without_gaps_or_duplicates() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let mut seqs = Vec::new();
    for n in 0..5 {
        seqs.push(message(&h, n, None).await);
    }
    // Last-Event-ID 优先于 after
    let head = req(Method::GET, "/v1/events?after=4", &d)
        .header("last-event-id", &seqs[1].as_u64().to_string());
    let mut c = open(&h, head).await;
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(c.next_event().await.unwrap().id.unwrap());
    }
    // 回放结束后并发追加：继续收到，序列连续
    let live = message(&h, 9, None).await;
    got.push(c.next_event().await.unwrap().id.unwrap());
    let expected: Vec<String> = seqs[2..]
        .iter()
        .chain(std::iter::once(&live))
        .map(|s| s.as_u64().to_string())
        .collect();
    assert_eq!(got, expected);
}

#[tokio::test(start_paused = true)]
async fn entry_checks_return_problems_before_the_stream_opens() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    message(&h, 1, None).await;
    let resp = send(
        &h,
        req(Method::GET, "/v1/events", &d).header("last-event-id", "99"),
        "",
    )
    .await;
    assert_problem(&resp, 409, "cursor_ahead");
    let resp = send(
        &h,
        req(Method::GET, "/v1/events", &d).header("last-event-id", "abc"),
        "",
    )
    .await;
    assert_problem(&resp, 400, "invalid_cursor");
    let resp = send(&h, req(Method::GET, "/v1/events?after=99", &d), "").await;
    assert_problem(&resp, 409, "cursor_ahead");
    let resp = send(&h, req(Method::GET, "/v1/events?types=nope", &d), "").await;
    assert_problem(&resp, 400, "invalid_request");
    let resp = send(&h, req(Method::GET, "/v1/events?unknown=1", &d), "").await;
    assert_problem(&resp, 400, "invalid_request");
}

#[tokio::test(start_paused = true)]
async fn filters_keep_global_events_and_heartbeats_advance_past_gaps() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let mine = ProjectId::from_parts(T0 as u64, 1).unwrap();
    let other = ProjectId::from_parts(T0 as u64, 2).unwrap();
    let path = format!("/v1/events?project_id={mine}&after=0");
    let mut c = open(&h, req(Method::GET, &path, &d)).await;
    message(&h, 1, Some(other)).await;
    let global = message(&h, 2, None).await;
    let own = message(&h, 3, Some(mine)).await;
    let last = message(&h, 4, Some(other)).await;
    assert_eq!(
        c.next_event().await.unwrap().id,
        Some(global.as_u64().to_string())
    );
    assert_eq!(
        c.next_event().await.unwrap().id,
        Some(own.as_u64().to_string())
    );
    // 被过滤掉的事件也推进心跳游标
    let hb = c.next_heartbeat().await;
    assert_eq!(hb.id, Some(last.as_u64().to_string()));

    // types：精确类型与前缀
    let mut c = open(
        &h,
        req(
            Method::GET,
            "/v1/events?types=inbox.*,settings.updated&after=0",
            &d,
        ),
    )
    .await;
    message(&h, 5, None).await;
    let hb = c.next_heartbeat().await;
    assert_eq!(
        hb.json()["seq"].as_u64(),
        Some(h.store.events().head().await.unwrap().as_u64())
    );
}

#[tokio::test(start_paused = true)]
async fn mobile_streams_only_carry_the_whitelist() {
    let h = harness(None);
    let phone = viewer(&h, 1, DevicePlatform::Ios, DeviceRole::Operator);
    let mut c = open(&h, req(Method::GET, "/v1/events?after=0", &phone)).await;
    settings(&h, Seq::ZERO).await;
    let msg = message(&h, 1, None).await;
    let f = c.next_event().await.unwrap();
    assert_eq!(
        f.id,
        Some(msg.as_u64().to_string()),
        "settings.* never reaches mobile"
    );
    // 请求白名单外的类型：不报错，只是收不到
    let mut c = open(
        &h,
        req(Method::GET, "/v1/events?types=delivery.*&after=0", &phone),
    )
    .await;
    let hb = c.next_heartbeat().await;
    assert_eq!(hb.json()["seq"], msg.as_u64());
}

#[tokio::test(start_paused = true)]
async fn settings_events_follow_the_role_at_send_time() {
    let h = harness(None);
    let viewer_dev = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let op = viewer(&h, 2, DevicePlatform::Macos, DeviceRole::Operator);
    let mut v = open(&h, req(Method::GET, "/v1/events?after=0", &viewer_dev)).await;
    let mut o = open(&h, req(Method::GET, "/v1/events?after=0", &op)).await;
    let s1 = settings(&h, Seq::ZERO).await;
    let m1 = message(&h, 1, None).await;
    assert_eq!(
        v.next_event().await.unwrap().id,
        Some(m1.as_u64().to_string())
    );
    assert_eq!(
        o.next_event().await.unwrap().id,
        Some(s1.as_u64().to_string())
    );
    assert_eq!(
        o.next_event().await.unwrap().id,
        Some(m1.as_u64().to_string())
    );
    // 运行中降为 viewer：下一次心跳后不再下发 settings.*
    h.devices
        .update(&op.cert_serial, |d| d.role = DeviceRole::Viewer);
    o.next_heartbeat().await;
    settings(&h, s1).await;
    let m2 = message(&h, 2, None).await;
    assert_eq!(
        o.next_event().await.unwrap().id,
        Some(m2.as_u64().to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn at_most_three_streams_per_device() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let other = viewer(&h, 2, DevicePlatform::Macos, DeviceRole::Viewer);
    let a = open(&h, req(Method::GET, "/v1/events", &d)).await;
    let _b = open(&h, req(Method::GET, "/v1/events", &d)).await;
    let _c = open(&h, req(Method::GET, "/v1/events", &d)).await;
    let resp = send(&h, req(Method::GET, "/v1/events", &d), "").await;
    assert_problem(&resp, 429, "rate_limited");
    assert!(header(&resp, "retry-after").is_some());
    // 其他设备不受影响
    let _x = open(&h, req(Method::GET, "/v1/events", &other)).await;
    // 关闭一条后可再开（任务在下一次发送失败时释放许可）
    drop(a);
    tokio::time::sleep(Duration::from_secs(16)).await;
    let _d = open(&h, req(Method::GET, "/v1/events", &d)).await;
}

#[tokio::test(start_paused = true)]
async fn revoked_device_stream_closes_within_one_heartbeat() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let other = viewer(&h, 2, DevicePlatform::Macos, DeviceRole::Viewer);
    let mut c = open(&h, req(Method::GET, "/v1/events", &d)).await;
    let mut o = open(&h, req(Method::GET, "/v1/events", &other)).await;
    c.next_heartbeat().await;
    o.next_heartbeat().await;
    let start = tokio::time::Instant::now();
    h.devices
        .update(&d.cert_serial, |d| d.status = DeviceStatus::Revoked);
    let close = c.next_event().await.unwrap();
    assert_eq!(close.event.as_deref(), Some("close"));
    assert_eq!(close.json(), json!({"reason": "device_revoked"}));
    assert!(
        start.elapsed() <= Duration::from_secs(15),
        "closed within one heartbeat"
    );
    assert!(c.next().await.is_none(), "stream ends after close");
    // 其他设备的流照常
    let m = message(&h, 1, None).await;
    assert_eq!(
        o.next_event().await.unwrap().id,
        Some(m.as_u64().to_string())
    );
    // 新请求被拒
    assert_problem(
        &send(&h, req(Method::GET, "/v1/events", &d), "").await,
        401,
        "device_revoked",
    );
}

#[tokio::test(start_paused = true)]
async fn expired_certificate_and_shutdown_close_streams() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let other = viewer(&h, 2, DevicePlatform::Macos, DeviceRole::Viewer);
    let mut c = open(&h, req(Method::GET, "/v1/events", &d)).await;
    // 证书到期（peer() 的 notAfter = T0 + 20 天）
    h.clock.advance_secs(20 * 86_400);
    let close = c.next_event().await.unwrap();
    assert_eq!(close.json(), json!({"reason": "cert_expired"}));
    assert!(c.next().await.is_none());
    // 关停：所有流收到 server_shutdown
    h.clock.set(gantry_core::time::Timestamp::from_millis(T0));
    let mut o = open(&h, req(Method::GET, "/v1/events", &other)).await;
    let mut p = open(&h, req(Method::GET, "/v1/events", &d)).await;
    h.core.shutdown();
    for s in [&mut o, &mut p] {
        let close = s.next_event().await.unwrap();
        assert_eq!(close.json(), json!({"reason": "server_shutdown"}));
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeats_carry_the_current_log_epoch() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let mut c = open(&h, req(Method::GET, "/v1/events", &d)).await;
    assert_eq!(c.next_heartbeat().await.json()["log_epoch"], 1);
    h.store.restore_epoch_bump();
    let start = tokio::time::Instant::now();
    let hb = c.next_heartbeat().await;
    assert_eq!(hb.json()["log_epoch"], 2);
    assert!(start.elapsed() <= Duration::from_secs(15));
}

#[tokio::test(start_paused = true)]
async fn slow_consumers_are_closed_as_lagging_and_can_resume() {
    let h = harness(None);
    let d = viewer(&h, 1, DevicePlatform::Macos, DeviceRole::Viewer);
    let mut c = open(&h, req(Method::GET, "/v1/events?after=0", &d)).await;
    let total = gantry_api::sse::SEND_BUFFER_FRAMES as u128 + 50;
    for n in 0..total {
        message(&h, n, None).await;
    }
    // 不读取：缓冲写满并超过写超时
    tokio::time::sleep(gantry_api::sse::SEND_TIMEOUT + Duration::from_secs(1)).await;
    let mut last_id = None;
    let close = loop {
        let f = c.next().await.expect("close frame before the end");
        if f.event.as_deref() == Some("close") {
            break f;
        }
        if f.event.as_deref() == Some("message.posted") {
            last_id = f.id.clone();
        }
    };
    assert_eq!(close.json(), json!({"reason": "lagging"}));
    assert!(c.next().await.is_none());
    // 从最后收到的 id 续传，不丢事件
    let cursor = last_id.unwrap();
    let head = req(Method::GET, "/v1/events", &d).header("last-event-id", &cursor);
    let mut r = open(&h, head).await;
    let next = r.next_event().await.unwrap();
    let next_id: u64 = next.id.unwrap().parse().unwrap();
    assert_eq!(next_id, cursor.parse::<u64>().unwrap() + 1);
}
