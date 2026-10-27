//! worker.md §12.G：stream-json 映射（用真实 CLI 的录制），以及 §12.B 中"工具输出不外泄"。

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use common::{assert_valid_with, validator};
use gantry_core::worker::{AgentId, ProgressStage, ResultErrorKind, ResultStatus};
use gantry_worker::events::{Clock, Event, EventWriter, UsageTotals};
use gantry_worker::prompt::parse_envelope;
use gantry_worker::stream::{infer_stage, summarize, CliInit, Mapped, StreamMapper};
use serde_json::{json, Value};

const SUCCESS: &str = include_str!("../fixtures/claude-stream-json/success.jsonl");
const MAX_TURNS: &str = include_str!("../fixtures/claude-stream-json/max_turns.jsonl");

#[derive(Clone, Default)]
struct FakeClock(Rc<Cell<Duration>>);

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

fn map_all(text: &str) -> (StreamMapper, Vec<Mapped>) {
    let mut mapper = StreamMapper::new();
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        out.extend(mapper.on_line(line, Duration::from_millis(100 * i as u64)));
    }
    (mapper, out)
}

/// 把映射结果送进事件写入器，返回 stdout 的每一行（已过 schema）。
fn render(mapped: &[Mapped]) -> Vec<Value> {
    let clock = FakeClock::default();
    let mut writer = EventWriter::new(Vec::new(), clock.clone());
    let job_id = "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD".parse().unwrap();
    let mut status = ResultStatus::Succeeded;
    for item in mapped {
        clock.0.set(clock.0.get() + Duration::from_secs(2));
        match item {
            Mapped::Init(init) => writer
                .hello(&job_id, &AgentId::ClaudeCode, &init.agent_version)
                .unwrap(),
            Mapped::Event(event) => {
                writer.emit(event.clone()).unwrap();
            }
            Mapped::Result(result) => {
                if result.error.is_some() {
                    status = ResultStatus::Failed;
                }
            }
        }
    }
    writer.finish(status).unwrap();
    let schema = validator("progress-event.schema.json");
    String::from_utf8(writer.into_inner())
        .unwrap()
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_valid_with(&schema, "progress-event.schema.json", &value);
            value
        })
        .collect()
}

#[test]
fn recorded_success_maps_to_protocol_events() {
    let (mapper, mapped) = map_all(SUCCESS);
    assert_eq!(
        mapped.first(),
        Some(&Mapped::Init(CliInit {
            agent_version: "2.1.303".into(),
            model: "claude-sonnet-5-5".into(),
            session_id: Some("697f657b-4bf1-43d2-aef0-5fd38bbedfe4".into()),
        }))
    );
    let starts: Vec<&Event> = mapped
        .iter()
        .filter_map(|m| match m {
            Mapped::Event(e @ Event::ToolStart { .. }) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        [&Event::ToolStart {
            call_id: "toolu_stub_1".into(),
            tool: "Bash".into(),
            summary: "echo CANARY-tool-output-77".into(),
        }]
    );
    assert!(mapped.iter().any(|m| matches!(
        m,
        Mapped::Event(Event::ToolEnd { call_id, ok: true, duration_ms, .. })
            if call_id == "toolu_stub_1" && *duration_ms == 100
    )));
    let Some(Mapped::Result(result)) = mapped.last() else {
        panic!("last mapped item is the result");
    };
    assert_eq!(result.error, None);
    assert_eq!(result.num_turns, 2);
    let envelope = parse_envelope(result.text.as_deref().unwrap()).unwrap();
    assert_eq!(envelope.structured_output["kind"], "build");
    assert_eq!(mapper.tool_calls(), 1);
    assert_eq!(mapper.in_flight(), 0);

    // 用量取 result 行的累计值；CLI 的费用字段不进入任何输出。
    let last: Value = serde_json::from_str(SUCCESS.lines().last().unwrap()).unwrap();
    let usage = mapper.usage();
    assert_eq!(
        usage.input_tokens,
        last["usage"]["input_tokens"].as_u64().unwrap()
    );
    assert_eq!(
        usage.output_tokens,
        last["usage"]["output_tokens"].as_u64().unwrap()
    );
    assert_eq!(usage.turns, 2);
    let lines = render(&mapped);
    assert_eq!(lines[0]["type"], "hello");
    assert_eq!(lines[0]["agent_version"], "2.1.303");
    assert_eq!(lines.last().unwrap()["type"], "finish");
    for line in &lines {
        assert!(!line.to_string().contains("cost"), "{line}");
    }
}

#[test]
fn recorded_max_turns_maps_to_max_turns_error() {
    let (mapper, mapped) = map_all(MAX_TURNS);
    let Some(Mapped::Result(result)) = mapped.last() else {
        panic!("last mapped item is the result");
    };
    assert_eq!(result.error, Some(ResultErrorKind::MaxTurns));
    assert_eq!(result.text, None);
    assert_eq!(result.num_turns, 6);
    assert_eq!(mapper.tool_calls(), 5);
    let lines = render(&mapped);
    assert_eq!(lines.last().unwrap()["status"], "failed");
}

/// 工具输出正文（`tool_result.content`、`tool_use_result.stdout`）与写入内容从不进入事件。
#[test]
fn tool_output_and_written_content_never_reach_events() {
    let sentinel = "CANARY-TOOL-OUTPUT-9f2e";
    let written = "CANARY-WRITTEN-CONTENT-1a7b";
    let mut lines: Vec<String> = Vec::new();
    for line in SUCCESS.lines() {
        let mut value: Value = serde_json::from_str(line).unwrap();
        if value["type"] == "user" {
            value["message"]["content"][0]["content"] = json!(sentinel);
            value["tool_use_result"]["stdout"] = json!(sentinel);
            value["tool_use_result"]["stderr"] = json!(sentinel);
        }
        lines.push(value.to_string());
    }
    // 再加一轮 Write / Edit：内容与替换文本只在 input 里。
    lines.insert(
        lines.len() - 1,
        json!({"type": "assistant", "message": {"id": "msg_2", "content": [
            {"type": "tool_use", "id": "t2", "name": "Write",
             "input": {"file_path": "src/a.rs", "content": written}},
            {"type": "tool_use", "id": "t3", "name": "Edit",
             "input": {"file_path": "src/b.rs", "old_string": written, "new_string": written}}
        ], "usage": {"input_tokens": 1, "output_tokens": 1}}})
        .to_string(),
    );
    lines.insert(
        lines.len() - 1,
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t2", "content": sentinel, "is_error": false},
            {"type": "tool_result", "tool_use_id": "t3", "content": sentinel, "is_error": true}
        ]}})
        .to_string(),
    );
    let (_, mapped) = map_all(&lines.join("\n"));
    let rendered = render(&mapped);
    for line in &rendered {
        let text = line.to_string();
        assert!(!text.contains(sentinel), "{text}");
        assert!(!text.contains(written), "{text}");
    }
    assert!(rendered
        .iter()
        .any(|l| l["type"] == "tool" && l["summary"] == "write src/a.rs"));
    assert!(rendered
        .iter()
        .any(|l| l["type"] == "tool" && l["call_id"] == "t3" && l["ok"] == false));
    assert!(rendered
        .iter()
        .any(|l| l["type"] == "stage" && l["stage"] == "edit"));
}

#[test]
fn malformed_and_unknown_lines_are_ignored() {
    let mut mapper = StreamMapper::new();
    for line in [
        "not json",
        "{\"type\":\"bogus\"}",
        "[1,2,3]",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"never-started\"}]}}",
    ] {
        let mapped = mapper.on_line(line, Duration::ZERO);
        assert!(
            mapped
                .iter()
                .all(|m| matches!(m, Mapped::Event(Event::Heartbeat { .. }))),
            "{line}: {mapped:?}"
        );
    }
    assert_eq!(mapper.ignored_lines(), 3);
}

#[test]
fn result_subtypes_map_to_error_kinds() {
    let cases = [
        (
            json!({"type":"result","subtype":"success","is_error":false,"result":"{}"}),
            None,
        ),
        (
            json!({"type":"result","subtype":"success","is_error":true,"result":"x"}),
            Some(ResultErrorKind::AgentError),
        ),
        (
            json!({"type":"result","subtype":"error_max_turns","is_error":true}),
            Some(ResultErrorKind::MaxTurns),
        ),
        (
            json!({"type":"result","subtype":"error_during_execution","is_error":true}),
            Some(ResultErrorKind::AgentError),
        ),
        (
            json!({"type":"result","subtype":"error_something_new"}),
            Some(ResultErrorKind::AgentError),
        ),
    ];
    for (line, expected) in cases {
        let mapped = StreamMapper::new().on_line(&line.to_string(), Duration::ZERO);
        let Some(Mapped::Result(result)) = mapped.last() else {
            panic!("{line}");
        };
        assert_eq!(result.error, expected, "{line}");
        assert_eq!(result.text.is_some(), expected.is_none(), "{line}");
    }
}

#[test]
fn usage_sums_distinct_messages_and_never_decreases() {
    let mut mapper = StreamMapper::new();
    let msg = |id: &str, input: u64| {
        json!({"type":"assistant","message":{"id":id,"content":[],
            "usage":{"input_tokens":input,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}})
        .to_string()
    };
    mapper.on_line(&msg("m1", 10), Duration::ZERO);
    // 同一条消息被拆成多行时重复出现，不重复计数。
    mapper.on_line(&msg("m1", 10), Duration::ZERO);
    mapper.on_line(&msg("m2", 5), Duration::ZERO);
    assert_eq!(
        mapper.usage(),
        UsageTotals {
            input_tokens: 15,
            output_tokens: 4,
            cache_read_tokens: 6,
            cache_write_tokens: 8,
            turns: 2,
        }
    );
    // result 给出的总数更小时不倒退。
    mapper.on_line(
        &json!({"type":"result","subtype":"success","is_error":false,"num_turns":1,
                "usage":{"input_tokens":1,"output_tokens":100}})
        .to_string(),
        Duration::ZERO,
    );
    let usage = mapper.usage();
    assert_eq!(usage.input_tokens, 15);
    assert_eq!(usage.output_tokens, 100);
    assert_eq!(usage.turns, 2);
}

#[test]
fn summaries_and_stages_follow_tool_categories() {
    let cases = [
        (
            "Bash",
            json!({"command": "cargo test -p api"}),
            "cargo test -p api",
            Some(ProgressStage::Test),
        ),
        (
            "Bash",
            json!({"command": "npm test"}),
            "npm test",
            Some(ProgressStage::Test),
        ),
        ("Bash", json!({"command": "ls -la"}), "ls -la", None),
        (
            "Read",
            json!({"file_path": "src/lib.rs"}),
            "read src/lib.rs",
            Some(ProgressStage::Explore),
        ),
        (
            "Grep",
            json!({"pattern": "fn main", "path": "src"}),
            "grep fn main in src",
            Some(ProgressStage::Explore),
        ),
        (
            "Glob",
            json!({"pattern": "**/*.rs"}),
            "glob **/*.rs",
            Some(ProgressStage::Explore),
        ),
        (
            "Edit",
            json!({"file_path": "a", "old_string": "x", "new_string": "y"}),
            "edit a",
            Some(ProgressStage::Edit),
        ),
        (
            "Write",
            json!({"file_path": "a", "content": "secret"}),
            "write a",
            Some(ProgressStage::Edit),
        ),
        (
            "TodoWrite",
            json!({"todos": [{"content": "secret"}]}),
            "update todo list",
            Some(ProgressStage::Plan),
        ),
        ("WebFetch", json!({"url": "https://x"}), "WebFetch", None),
    ];
    for (tool, input, summary, stage) in cases {
        assert_eq!(summarize(tool, &input), summary, "{tool}");
        assert_eq!(infer_stage(tool, &input), stage, "{tool} {input}");
    }
}
