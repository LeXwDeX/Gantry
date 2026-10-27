//! worker.md §12.B：stdout 与事件卫生（A4 / W3，M1）中写入器本身负责的部分。
//! 子进程写垃圾、哨兵串不外泄由执行者与 stream-json 映射的测试覆盖。

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use common::{assert_valid_with, validator};
use gantry_core::worker::{AgentId, ProgressEvent, ProgressStage, ResultStatus};
use gantry_worker::events::{
    clean, Clock, Event, EventWriter, UsageTotals, WarnCode, MAX_LINE_BYTES, MAX_TEXT_CHARS,
};
use serde_json::Value;

const PROGRESS: &str = include_str!("../../gantry-core/fixtures/protocol/worker/progress.jsonl");

#[derive(Clone, Default)]
struct FakeClock(Rc<Cell<Duration>>);

impl FakeClock {
    fn advance(&self, by: Duration) {
        self.0.set(self.0.get() + by);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

fn job_id() -> gantry_core::ids::AttemptId {
    "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD".parse().unwrap()
}

fn writer() -> (EventWriter<Vec<u8>, FakeClock>, FakeClock) {
    let clock = FakeClock::default();
    (EventWriter::new(Vec::new(), clock.clone()), clock)
}

/// 逐行检查：合法 JSON、≤ 8 KiB、通过 schema、seq 从 1 严格递增、hello 第一行。
fn lines(bytes: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(bytes).unwrap();
    assert!(text.is_empty() || text.ends_with('\n'));
    let schema = validator("progress-event.schema.json");
    let values: Vec<Value> = text
        .lines()
        .map(|line| {
            assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
            let value: Value = serde_json::from_str(line).unwrap();
            assert_valid_with(&schema, "progress-event.schema.json", &value);
            serde_json::from_value::<ProgressEvent>(value.clone()).unwrap();
            value
        })
        .collect();
    for (i, value) in values.iter().enumerate() {
        assert_eq!(value["seq"], (i + 1) as u64, "{value}");
        assert_eq!(value["v"], 1);
    }
    if let Some(first) = values.first() {
        assert_eq!(first["type"], "hello");
    }
    values
}

fn tool_start(call_id: &str, summary: &str) -> Event {
    Event::ToolStart {
        call_id: call_id.into(),
        tool: "Bash".into(),
        summary: summary.into(),
    }
}

#[test]
fn replaying_the_fixture_reproduces_it() {
    let (mut w, _clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, "x.y.z").unwrap();
    w.emit(Event::Stage {
        stage: ProgressStage::Explore,
        detail: "reading src/api/items.rs".into(),
    })
    .unwrap();
    w.emit(tool_start("c1", "cargo test -p api items::pagination"))
        .unwrap();
    w.emit(Event::ToolEnd {
        call_id: "c1".into(),
        tool: "Bash".into(),
        ok: true,
        duration_ms: 4210,
    })
    .unwrap();
    w.emit(Event::Usage(UsageTotals {
        input_tokens: 18200,
        output_tokens: 2400,
        cache_read_tokens: 9000,
        cache_write_tokens: 1200,
        turns: 3,
    }))
    .unwrap();
    w.emit(Event::Heartbeat { in_flight: 1 }).unwrap();
    w.emit(Event::Warn {
        code: WarnCode::MaterialTruncated,
        detail: "mat-002 was truncated by the server".into(),
    })
    .unwrap();
    w.finish(ResultStatus::Succeeded).unwrap();

    let ours: Vec<ProgressEvent> = lines(&w.into_inner())
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
    let fixture: Vec<ProgressEvent> = PROGRESS
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(ours, fixture);
}

#[test]
fn nothing_before_hello_and_nothing_after_finish() {
    let (mut w, _clock) = writer();
    assert!(!w.emit(tool_start("c0", "ls")).unwrap());
    w.finish(ResultStatus::Failed).unwrap();
    assert!(w.into_inner().is_empty());

    let (mut w, _clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, "1").unwrap();
    w.hello(&job_id(), &AgentId::ClaudeCode, "2").unwrap();
    w.finish(ResultStatus::Cancelled).unwrap();
    assert!(!w.emit(tool_start("c1", "ls")).unwrap());
    w.finish(ResultStatus::Succeeded).unwrap();
    let out = lines(&w.into_inner());
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["agent_version"], "1");
    assert_eq!(out[1]["type"], "finish");
    assert_eq!(out[1]["status"], "cancelled");
}

#[test]
fn only_whitelisted_fields_are_written() {
    let (mut w, clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, "1").unwrap();
    w.emit(Event::Stage {
        stage: ProgressStage::Edit,
        detail: "x".into(),
    })
    .unwrap();
    w.emit(tool_start("c1", "ls")).unwrap();
    w.emit(Event::ToolEnd {
        call_id: "c1".into(),
        tool: "Bash".into(),
        ok: false,
        duration_ms: 1,
    })
    .unwrap();
    w.emit(Event::Usage(UsageTotals::default())).unwrap();
    clock.advance(Duration::from_secs(2));
    w.emit(Event::Heartbeat { in_flight: 0 }).unwrap();
    w.emit(Event::Warn {
        code: WarnCode::SchemaRepair,
        detail: "x".into(),
    })
    .unwrap();
    w.finish(ResultStatus::Succeeded).unwrap();
    for value in lines(&w.into_inner()) {
        let allowed: &[&str] = match value["type"].as_str().unwrap() {
            "hello" => &["job_id", "agent", "agent_version"],
            "stage" => &["stage", "detail"],
            "tool" => &["phase", "call_id", "tool", "summary", "ok", "duration_ms"],
            "usage" => &[
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "turns",
            ],
            "heartbeat" => &["in_flight"],
            "warn" => &["code", "detail"],
            "finish" => &["status"],
            other => panic!("unexpected type {other}"),
        };
        for key in value.as_object().unwrap().keys() {
            assert!(
                ["v", "seq", "type"].contains(&key.as_str()) || allowed.contains(&key.as_str()),
                "{key} in {value}"
            );
        }
    }
}

#[test]
fn strings_are_cleaned_and_truncated() {
    let hostile = format!(
        "\u{1b}[31mred\u{1b}[0m\u{0}\u{7}line\nbreak\u{202e}evil\u{2066}\u{e0041}tag{}",
        "界".repeat(10_000)
    );
    let cleaned = clean(&hostile, MAX_TEXT_CHARS);
    assert!(cleaned.starts_with("redlinebreakeviltag"), "{cleaned}");
    assert_eq!(cleaned.chars().count(), MAX_TEXT_CHARS);

    let (mut w, _clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, &hostile).unwrap();
    w.emit(tool_start(&hostile, &hostile)).unwrap();
    w.emit(Event::Stage {
        stage: ProgressStage::Test,
        detail: hostile.clone(),
    })
    .unwrap();
    w.emit(Event::Warn {
        code: WarnCode::ProposalDropped,
        detail: hostile,
    })
    .unwrap();
    for value in lines(&w.into_inner()) {
        for (key, field) in value.as_object().unwrap() {
            if let Some(text) = field.as_str() {
                assert!(text.chars().all(|c| !c.is_control()), "{key}");
                assert!(!text.contains('\u{202e}') && !text.contains('\u{e0041}'));
                assert!(text.chars().count() <= MAX_TEXT_CHARS, "{key}");
            }
        }
    }
}

#[test]
fn usage_is_cumulative_monotonic_and_coalesced() {
    let (mut w, clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, "1").unwrap();
    // 一秒内 100 个样本，其中有回退的：只出 1 行，且不倒退。
    for i in 0..100u64 {
        let input = if i % 7 == 0 { i / 2 } else { i * 10 };
        w.emit(Event::Usage(UsageTotals {
            input_tokens: input,
            output_tokens: i,
            turns: i / 10,
            ..UsageTotals::default()
        }))
        .unwrap();
        clock.advance(Duration::from_millis(5));
    }
    clock.advance(Duration::from_secs(1));
    // 到期后的下一个事件顺带发出合并中的最新累计值。
    w.emit(tool_start("c1", "ls")).unwrap();
    w.emit(Event::Usage(UsageTotals {
        input_tokens: 1,
        ..UsageTotals::default()
    }))
    .unwrap();
    w.finish(ResultStatus::Succeeded).unwrap();
    let out = lines(&w.into_inner());
    let usages: Vec<&Value> = out.iter().filter(|v| v["type"] == "usage").collect();
    assert_eq!(usages.len(), 2, "{out:#?}");
    assert_eq!(usages[0]["input_tokens"], 0);
    assert_eq!(usages[1]["input_tokens"], 990);
    assert_eq!(usages[1]["output_tokens"], 99);
    assert_eq!(usages[1]["turns"], 9);
    for pair in usages.windows(2) {
        for key in ["input_tokens", "output_tokens", "turns"] {
            assert!(pair[0][key].as_u64() <= pair[1][key].as_u64(), "{key}");
        }
    }
    // finish 之前补发的那一行就是最后一个累计值，finish 是最后一行。
    assert_eq!(out.last().unwrap()["type"], "finish");
}

#[test]
fn heartbeat_and_usage_storm_stays_under_ten_lines_per_second() {
    let (mut w, clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, "1").unwrap();
    for i in 0..10_000u64 {
        w.emit(Event::Heartbeat {
            in_flight: (i % 3) as u32,
        })
        .unwrap();
        w.emit(Event::Usage(UsageTotals {
            input_tokens: i,
            ..UsageTotals::default()
        }))
        .unwrap();
        clock.advance(Duration::from_millis(1));
    }
    let out = lines(&w.into_inner());
    // 10 秒的风暴：每类每秒至多一行。
    let seconds = 10;
    assert!(out.len() - 1 <= 2 * seconds, "{} lines", out.len());
    assert!(out.len() > seconds, "{} lines", out.len());
}

#[test]
fn tool_and_stage_events_are_never_dropped() {
    let (mut w, _clock) = writer();
    w.hello(&job_id(), &AgentId::ClaudeCode, "1").unwrap();
    for i in 0..500 {
        assert!(w.emit(tool_start(&format!("c{i}"), "ls")).unwrap());
        assert!(w
            .emit(Event::Stage {
                stage: ProgressStage::Explore,
                detail: "x".into()
            })
            .unwrap());
    }
    assert_eq!(lines(&w.into_inner()).len(), 1001);
}
