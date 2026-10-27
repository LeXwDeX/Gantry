//! sandbox.md §12.E：进度采集。

mod common;

use std::time::Duration;

use gantry_core::time::Timestamp;
use gantry_core::worker::{ProgressEvent, ProgressStage};
use gantry_sandbox::driver::LogStream;
use gantry_sandbox::progress::{
    AcceptedEvent, DropReason, LineOutcome, ProgressCollector, ProgressLimits, MAX_TEXT_CHARS,
};
use tokio::time::Instant;

fn limits() -> ProgressLimits {
    ProgressLimits {
        max_line_bytes: 8192,
        max_lines: 20_000,
        max_rate_per_s: 20,
        logs_tail_bytes: 1024,
        sample_interval: Duration::from_secs(1),
    }
}

fn collector() -> ProgressCollector {
    ProgressCollector::new(common::job_id(1), limits())
}

fn at(ms: i64) -> Timestamp {
    Timestamp::from_millis(1_700_000_000_000 + ms)
}

fn stage(seq: u64, detail: &str) -> Vec<u8> {
    serde_json::json!({"v":1,"seq":seq,"type":"stage","stage":"edit","detail":detail})
        .to_string()
        .into_bytes()
}

fn heartbeat(seq: u64) -> Vec<u8> {
    format!(r#"{{"v":1,"seq":{seq},"type":"heartbeat","in_flight":0}}"#).into_bytes()
}

fn reason(outcome: &LineOutcome) -> Option<DropReason> {
    match outcome {
        LineOutcome::Dropped { reason, .. } => Some(*reason),
        _ => None,
    }
}

#[test]
fn forged_and_malformed_lines_are_dropped() {
    let mut c = collector();
    let now = Instant::now();
    let push =
        |c: &mut ProgressCollector, line: &[u8]| c.push_line(LogStream::Stdout, line, now, at(0));

    assert!(matches!(
        push(&mut c, &stage(2, "ok")),
        LineOutcome::Accepted(_)
    ));
    // 乱序、重复 seq。
    assert_eq!(
        reason(&push(&mut c, &stage(1, "old"))),
        Some(DropReason::Sequence)
    );
    assert_eq!(
        reason(&push(&mut c, &stage(2, "dup"))),
        Some(DropReason::Sequence)
    );
    // 超长行。
    let long = stage(3, &"x".repeat(9000));
    assert_eq!(reason(&push(&mut c, &long)), Some(DropReason::TooLong));
    // 未知 type、缺字段、版本不对。
    assert_eq!(
        reason(&push(
            &mut c,
            br#"{"v":1,"seq":4,"type":"exec","cmd":"rm -rf /"}"#
        )),
        Some(DropReason::UnknownType)
    );
    assert_eq!(
        reason(&push(&mut c, br#"{"v":1,"seq":4,"type":"stage"}"#)),
        Some(DropReason::InvalidEvent)
    );
    assert_eq!(
        reason(&push(
            &mut c,
            br#"{"v":2,"seq":4,"type":"heartbeat","in_flight":0}"#
        )),
        Some(DropReason::Version)
    );
    // 重复键的 JSON 不是合法事件。
    assert!(matches!(
        push(
            &mut c,
            br#"{"v":1,"seq":4,"seq":5,"type":"heartbeat","in_flight":0}"#
        ),
        LineOutcome::Tail
    ));
    // 非 JSON 只进尾部。
    assert_eq!(push(&mut c, b"plain output from cargo"), LineOutcome::Tail);
    assert!(String::from_utf8(c.stdout_tail().contents())
        .unwrap()
        .contains("plain output"));

    let by = c.dropped_by_reason();
    assert_eq!(by[&DropReason::Sequence], 2);
    assert_eq!(by[&DropReason::TooLong], 1);
    assert_eq!(by[&DropReason::UnknownType], 1);
    assert_eq!(by[&DropReason::InvalidEvent], 1);
    assert_eq!(by[&DropReason::Version], 1);
    assert_eq!(by[&DropReason::NotJson], 2);
    let stats = c.stats();
    assert_eq!((stats.samples, stats.dropped), (1, 8));
}

#[test]
fn strings_are_sanitized_and_ts_ignored() {
    let mut c = collector();
    let line = serde_json::json!({
        "v": 1, "seq": 1, "type": "tool", "phase": "start", "call_id": "c\u{1b}[31m1",
        "tool": "Bash\u{202e}", "summary": format!("{}\u{e0041}", "s".repeat(300)),
        "ts": "1999-01-01T00:00:00Z"
    })
    .to_string();
    let LineOutcome::Accepted(accepted) =
        c.push_line(LogStream::Stdout, line.as_bytes(), Instant::now(), at(42))
    else {
        panic!("tool event must be accepted");
    };
    assert_eq!(accepted.received_at, at(42));
    let ProgressEvent::Tool(tool) = &accepted.event else {
        panic!()
    };
    assert_eq!(tool.call_id, "c1");
    assert_eq!(tool.tool, "Bash");
    let summary = tool.summary.as_ref().unwrap();
    assert_eq!(summary.chars().count(), MAX_TEXT_CHARS);
    assert!(!summary.contains('\u{e0041}'));
    // spool 行是带 received_at 的 JSON。
    let spool: serde_json::Value = serde_json::from_slice(&accepted.spool_line()).unwrap();
    assert_eq!(spool["event"]["type"], "tool");
}

#[test]
fn self_reported_usage_and_finish_are_display_only() {
    let mut c = collector();
    let now = Instant::now();
    let usage = br#"{"v":1,"seq":1,"type":"usage","input_tokens":1,"output_tokens":1,"cache_read_tokens":0,"cache_write_tokens":0,"turns":1}"#;
    let finish = br#"{"v":1,"seq":2,"type":"finish","status":"succeeded"}"#;
    assert!(matches!(
        c.push_line(LogStream::Stdout, usage, now, at(0)),
        LineOutcome::Accepted(_)
    ));
    assert!(matches!(
        c.push_line(LogStream::Stdout, finish, now, at(0)),
        LineOutcome::Accepted(_)
    ));
    // 采集器只暴露展示样本与计数，没有任何终态或用量出口；样本标注为自报。
    let samples = c.sample(now);
    assert!(samples
        .iter()
        .any(|s| s.detail.as_untrusted_str().starts_with("self-reported")));
}

#[test]
fn rate_and_total_limits_keep_draining_and_count() {
    let mut c = ProgressCollector::new(
        common::job_id(1),
        ProgressLimits {
            max_lines: 30,
            ..limits()
        },
    );
    let start = Instant::now();
    let mut accepted = 0;
    let mut rate = 0;
    let mut total = 0;
    let mut activity = 0;
    // 3 秒内每秒 25 行：每秒只收 20；累计合法事件到 30 后全部按总数上限丢弃。
    for second in 0..3u64 {
        for i in 0..25u64 {
            let seq = second * 25 + i + 1;
            let now = start + Duration::from_millis(second * 1000 + i * 10);
            let outcome = c.push_line(LogStream::Stdout, &heartbeat(seq), now, at(0));
            if outcome.activity() {
                activity += 1;
            }
            match outcome {
                LineOutcome::Accepted(_) => accepted += 1,
                LineOutcome::Dropped {
                    reason: DropReason::Rate,
                    ..
                } => rate += 1,
                LineOutcome::Dropped {
                    reason: DropReason::Total,
                    ..
                } => total += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    assert_eq!(accepted + rate + total, 75);
    assert_eq!(activity, 75, "被丢弃的合法事件仍重置空闲计时");
    assert_eq!(accepted, 30 - 5);
    assert_eq!(rate, 5);
    assert_eq!(total, 45);
    assert_eq!(c.stats().dropped, 50);
}

#[test]
fn tails_keep_only_the_end() {
    let mut c = collector();
    let now = Instant::now();
    for i in 0..200 {
        c.push_line(
            LogStream::Stderr,
            format!("err line {i:04}").as_bytes(),
            now,
            at(0),
        );
    }
    let tail = c.stderr_tail().contents();
    assert!(tail.len() <= 1024);
    let text = String::from_utf8(tail).unwrap();
    assert!(text.ends_with("err line 0199\n"));
    assert!(!text.contains("err line 0000"));
    assert!(c.stderr_tail().total() > 1024);
    // 单行超过上限：只留末尾。
    c.push_line(LogStream::Stderr, &vec![b'z'; 5000], now, at(0));
    assert_eq!(c.stderr_tail().contents().len(), 1024);
}

#[test]
fn sampling_is_throttled_and_keeps_latest_per_type() {
    let mut c = collector();
    let start = Instant::now();
    for seq in 1..=5 {
        c.push_line(
            LogStream::Stdout,
            &stage(seq, &format!("step {seq}")),
            start,
            at(0),
        );
    }
    c.push_line(LogStream::Stdout, &heartbeat(6), start, at(0));
    let first = c.sample(start);
    assert_eq!(first.len(), 2, "stage 与 heartbeat 各取最新一条");
    assert_eq!(first[0].seq, 5);
    assert_eq!(first[0].stage, ProgressStage::Edit);
    assert_eq!(first[0].detail.as_untrusted_str(), "step 5");

    c.push_line(LogStream::Stdout, &stage(7, "next"), start, at(0));
    assert!(c.sample(start + Duration::from_millis(500)).is_empty());
    let second = c.sample(start + Duration::from_millis(1000));
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].seq, 7);
}

#[test]
fn resume_dedups_replayed_lines() {
    let mut first = collector();
    let now = Instant::now();
    let mut spool: Vec<AcceptedEvent> = Vec::new();
    for seq in 1..=3 {
        if let LineOutcome::Accepted(event) =
            first.push_line(LogStream::Stdout, &stage(seq, "s"), now, at(0))
        {
            spool.push(event);
        }
    }
    // 重启：从头重读日志。
    let mut resumed = ProgressCollector::resume(common::job_id(1), limits(), &spool);
    for seq in 1..=3 {
        assert_eq!(
            resumed.push_line(LogStream::Stdout, &stage(seq, "s"), now, at(0)),
            LineOutcome::Replayed
        );
    }
    assert!(matches!(
        resumed.push_line(LogStream::Stdout, &stage(4, "s"), now, at(0)),
        LineOutcome::Accepted(_)
    ));
    // 越过重放点之后，再出现旧 seq 就是乱序。
    assert_eq!(
        reason(&resumed.push_line(LogStream::Stdout, &stage(2, "s"), now, at(0))),
        Some(DropReason::Sequence)
    );
    let stats = resumed.stats();
    assert_eq!((stats.samples, stats.dropped), (4, 1));
}
