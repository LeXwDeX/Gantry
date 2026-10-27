//! SSE 解析与游标（desktop-shell.md §12"事件流"的 A 级部分）。

use gantry_desktop_shell::sse::{backoff_ms, Action, CloseReason, Parser, RawEvent, StreamState};
use serde_json::json;

fn parse_all(chunks: &[&[u8]]) -> Vec<RawEvent> {
    let mut p = Parser::new();
    chunks.iter().flat_map(|c| p.feed(c).unwrap()).collect()
}

fn event(id: u64, ty: &str) -> String {
    let envelope = json!({"id": "evt_x", "seq": id, "type": ty, "data": {}});
    format!("id: {id}\nevent: {ty}\ndata: {envelope}\n\n")
}

#[test]
fn multi_line_data_comments_retry_and_line_endings() {
    let text = "\u{feff}: 注释行\nretry: 3000\nid: 7\nevent: x\ndata: a\ndata: b\n\n";
    for ending in ["\n", "\r\n", "\r"] {
        let t = text.replace('\n', ending);
        let mut p = Parser::new();
        let out = p.feed(t.as_bytes()).unwrap();
        assert_eq!(
            out,
            vec![RawEvent {
                id: Some("7".into()),
                event: "x".into(),
                data: "a\nb".into()
            }],
            "{ending:?}"
        );
        assert_eq!(p.retry_ms, Some(3000));
    }
}

#[test]
fn arbitrary_chunking_including_split_crlf() {
    let text = format!("{}{}", event(1, "a.b"), event(2, "a.c")).replace('\n', "\r\n");
    let bytes = text.as_bytes();
    let whole = parse_all(&[bytes]);
    assert_eq!(whole.len(), 2);
    for size in [1, 2, 3, 7, 64] {
        let chunks: Vec<&[u8]> = bytes.chunks(size).collect();
        assert_eq!(parse_all(&chunks), whole, "chunk size {size}");
    }
}

#[test]
fn empty_event_and_field_edge_cases() {
    // 只有 event 没有 data 的块不派发；`data` 空值也算有 data。
    let out = parse_all(&[b"event: x\n\ndata\n\nfield-without-colon\nid\ndata:no-space\n\n"]);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].data, "");
    assert_eq!(out[0].event, "message");
    assert_eq!(out[1].data, "no-space");
    assert_eq!(out[1].id.as_deref(), Some(""));
    // 未结束的块不派发。
    assert!(parse_all(&[b"data: half"]).is_empty());
}

fn raw(id: Option<&str>, ev: &str, data: serde_json::Value) -> RawEvent {
    RawEvent {
        id: id.map(str::to_string),
        event: ev.into(),
        data: data.to_string(),
    }
}

fn biz(seq: u64) -> RawEvent {
    raw(
        Some(&seq.to_string()),
        "delivery.stage_changed",
        json!({"seq": seq, "type": "delivery.stage_changed"}),
    )
}

#[test]
fn id_must_equal_seq_and_three_violations_reconnect() {
    let mut s = StreamState::new(None, None);
    assert!(matches!(s.handle(&biz(5)), Action::Forward(_)));
    let bad = raw(
        Some("6"),
        "delivery.stage_changed",
        json!({"seq": 7, "type": "delivery.stage_changed"}),
    );
    assert_eq!(s.handle(&bad), Action::Drop);
    let wrong_type = raw(
        Some("8"),
        "x.y",
        json!({"seq": 8, "type": "delivery.stage_changed"}),
    );
    assert_eq!(s.handle(&wrong_type), Action::Drop);
    assert_eq!(
        s.handle(&raw(Some("9"), "a.b", json!("not an envelope"))),
        Action::Reconnect
    );
    // 合法帧把计数清零。
    assert!(matches!(s.handle(&biz(10)), Action::Forward(_)));
    assert_eq!(s.handle(&bad), Action::Drop);
}

#[test]
fn duplicates_dropped_and_cursor_only_moves_on_ack() {
    let mut s = StreamState::new(Some(100), Some(1));
    assert_eq!(s.cursor(), Some(100));
    assert_eq!(s.handle(&biz(100)), Action::Drop, "≤ 初始游标的直接丢弃");
    let Action::Forward(frame) = s.handle(&biz(101)) else {
        panic!()
    };
    assert_eq!(frame["t"], "event");
    assert_eq!(frame["envelope"]["seq"], 101);
    assert_eq!(s.handle(&biz(101)), Action::Drop);
    // 未 ack 前游标不动：断线重放 101。
    assert_eq!(s.cursor(), Some(100));
    assert!(s.ack(101));
    assert_eq!(s.cursor(), Some(101));
    // 乱序、回退、超前（未转发）的 ack 都忽略。
    assert!(!s.ack(101));
    assert!(!s.ack(50));
    assert!(!s.ack(500));
    assert_eq!(s.cursor(), Some(101));
}

#[test]
fn heartbeat_advances_cursor_on_filtered_streams() {
    let mut s = StreamState::new(Some(10), Some(1));
    let hb = raw(Some("40"), "heartbeat", json!({"seq": 40, "log_epoch": 1}));
    assert_eq!(
        s.handle(&hb),
        Action::Forward(json!({"t": "heartbeat", "seq": 40}))
    );
    assert!(s.ack(40));
    assert_eq!(s.cursor(), Some(40), "过滤流里只有心跳时，重连用心跳 id");
    // 心跳之后到来的更小 seq 视为已扫描过。
    assert_eq!(s.handle(&biz(30)), Action::Drop);
}

#[test]
fn log_epoch_change_and_reset() {
    let mut s = StreamState::new(Some(10), Some(1));
    let hb = raw(Some("12"), "heartbeat", json!({"seq": 12, "log_epoch": 2}));
    assert_eq!(s.handle(&hb), Action::LogEpochChanged(2));
    s.reset();
    assert_eq!(s.cursor(), None, "纪元变化后重连不带 Last-Event-ID");
    // 首次得知纪元号只记录。
    let mut s = StreamState::new(None, None);
    assert!(matches!(
        s.handle(&raw(Some("1"), "heartbeat", json!({"log_epoch": 5}))),
        Action::Forward(_)
    ));
    assert_eq!(s.log_epoch, Some(5));
}

#[test]
fn close_frames() {
    let mut s = StreamState::new(None, None);
    for (reason, want) in [
        ("device_revoked", CloseReason::DeviceRevoked),
        ("cert_expired", CloseReason::CertExpired),
        ("server_shutdown", CloseReason::ServerShutdown),
        ("lagging", CloseReason::Lagging),
        ("future", CloseReason::Other("future".into())),
    ] {
        assert_eq!(
            s.handle(&raw(None, "close", json!({"reason": reason}))),
            Action::Close(want)
        );
    }
}

#[test]
fn backoff_is_full_jitter_capped_at_30s() {
    for attempt in 1..=10u32 {
        let cap = 2u64.pow(attempt - 1).min(30) * 1000;
        assert_eq!(backoff_ms(attempt, None, 0.0), 0);
        assert!(backoff_ms(attempt, None, 0.999_999) <= cap);
        assert!(backoff_ms(attempt, None, 0.5) <= cap);
    }
    assert_eq!(backoff_ms(20, None, 1.0), 30_000);
    // `retry:` 更大时取它（上限 30 s）。
    assert_eq!(backoff_ms(1, Some(3_000), 0.1), 3_000);
    assert_eq!(backoff_ms(1, Some(90_000), 0.0), 30_000);
}
