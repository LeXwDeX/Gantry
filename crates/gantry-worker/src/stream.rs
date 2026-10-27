//! Claude Code `stream-json` → 协议进度事件（worker.md §6.4 末表；A4）。
//!
//! CLI 的输出受模型影响，是**不可信**输入：解析失败的行与未知类型一律忽略；工具输出正文
//! （`tool_result.content`、`tool_use_result`）从不读取；CLI 给出的费用字段忽略。
//! 形状依据本机 `claude 2.1.303` 的录制（`fixtures/claude-stream-json/`）。

use std::collections::BTreeMap;
use std::time::Duration;

use gantry_core::worker::{ProgressStage, ResultErrorKind};
use serde_json::Value;

use crate::events::{Event, UsageTotals};

/// `system/init` 里取到的信息（`hello` 与 `result.json.agent`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliInit {
    pub agent_version: String,
    pub model: String,
}

/// `result` 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliResult {
    /// `None` 表示成功；否则是 `result.json.error.kind`。
    pub error: Option<ResultErrorKind>,
    /// 成功时的最终回复文本（应是信封 JSON，见 `prompt`）。
    pub text: Option<String>,
    pub num_turns: u64,
}

/// 一行映射出的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mapped {
    Init(CliInit),
    Event(Event),
    Result(CliResult),
}

#[derive(Debug, Default)]
pub struct StreamMapper {
    in_flight: BTreeMap<String, (String, Duration)>,
    /// 每个 assistant 消息 id 的最新 usage；同一消息拆成多行时重复出现。
    per_message: BTreeMap<String, UsageTotals>,
    result_usage: Option<UsageTotals>,
    stage: Option<ProgressStage>,
    tool_calls: u64,
    ignored_lines: u64,
}

impl StreamMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// 累计用量：各 assistant 消息之和，`result` 行给出总数后与其逐项取大。
    pub fn usage(&self) -> UsageTotals {
        let mut total = UsageTotals::default();
        for u in self.per_message.values() {
            total.input_tokens += u.input_tokens;
            total.output_tokens += u.output_tokens;
            total.cache_read_tokens += u.cache_read_tokens;
            total.cache_write_tokens += u.cache_write_tokens;
        }
        total.turns = self.per_message.len() as u64;
        match self.result_usage {
            Some(r) => UsageTotals {
                input_tokens: total.input_tokens.max(r.input_tokens),
                output_tokens: total.output_tokens.max(r.output_tokens),
                cache_read_tokens: total.cache_read_tokens.max(r.cache_read_tokens),
                cache_write_tokens: total.cache_write_tokens.max(r.cache_write_tokens),
                turns: total.turns.max(r.turns),
            },
            None => total,
        }
    }

    pub fn tool_calls(&self) -> u64 {
        self.tool_calls
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.len() as u32
    }

    pub fn ignored_lines(&self) -> u64 {
        self.ignored_lines
    }

    /// 处理一行；`now` 用于计算工具耗时。
    pub fn on_line(&mut self, line: &str, now: Duration) -> Vec<Mapped> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.ignored_lines += 1;
            return Vec::new();
        };
        match (str_at(&value, "/type"), str_at(&value, "/subtype")) {
            (Some("system"), Some("init")) => vec![Mapped::Init(CliInit {
                agent_version: str_at(&value, "/claude_code_version")
                    .unwrap_or("unknown")
                    .to_owned(),
                model: str_at(&value, "/model").unwrap_or("unknown").to_owned(),
            })],
            // 重试等系统消息只说明 agent 仍在活动。
            (Some("system"), _) => vec![self.heartbeat()],
            (Some("assistant"), _) => self.on_assistant(&value, now),
            (Some("user"), _) => self.on_user(&value, now),
            (Some("result"), _) => self.on_result(&value),
            _ => {
                self.ignored_lines += 1;
                Vec::new()
            }
        }
    }

    fn heartbeat(&self) -> Mapped {
        Mapped::Event(Event::Heartbeat {
            in_flight: self.in_flight(),
        })
    }

    fn on_assistant(&mut self, value: &Value, now: Duration) -> Vec<Mapped> {
        let mut out = Vec::new();
        if let (Some(id), Some(usage)) = (
            str_at(value, "/message/id"),
            value.pointer("/message/usage"),
        ) {
            self.per_message.insert(id.to_owned(), usage_of(usage));
        }
        let blocks = value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for block in blocks {
            if str_at(block, "/type") != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(name)) = (str_at(block, "/id"), str_at(block, "/name")) else {
                continue;
            };
            let input = block.get("input").unwrap_or(&Value::Null);
            let summary = summarize(name, input);
            if let Some(stage) = infer_stage(name, input) {
                if self.stage != Some(stage) {
                    self.stage = Some(stage);
                    out.push(Mapped::Event(Event::Stage {
                        stage,
                        detail: summary.clone(),
                    }));
                }
            }
            self.tool_calls += 1;
            self.in_flight.insert(id.to_owned(), (name.to_owned(), now));
            out.push(Mapped::Event(Event::ToolStart {
                call_id: id.to_owned(),
                tool: name.to_owned(),
                summary,
            }));
        }
        out.push(Mapped::Event(Event::Usage(self.usage())));
        out
    }

    fn on_user(&mut self, value: &Value, now: Duration) -> Vec<Mapped> {
        let mut out = Vec::new();
        let blocks = value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for block in blocks {
            if str_at(block, "/type") != Some("tool_result") {
                continue;
            }
            let Some(id) = str_at(block, "/tool_use_id") else {
                continue;
            };
            // 只取成败与耗时；`content` 是工具输出正文，不读。
            let ok = !block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let Some((tool, started)) = self.in_flight.remove(id) else {
                continue;
            };
            out.push(Mapped::Event(Event::ToolEnd {
                call_id: id.to_owned(),
                tool,
                ok,
                duration_ms: now.saturating_sub(started).as_millis() as u64,
            }));
        }
        if out.is_empty() {
            out.push(self.heartbeat());
        }
        out
    }

    fn on_result(&mut self, value: &Value) -> Vec<Mapped> {
        let num_turns = value.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
        if let Some(usage) = value.get("usage") {
            let mut u = usage_of(usage);
            u.turns = num_turns;
            self.result_usage = Some(u);
        }
        let is_error = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let error = match str_at(value, "/subtype") {
            Some("success") if !is_error => None,
            Some("error_max_turns") => Some(ResultErrorKind::MaxTurns),
            _ => Some(ResultErrorKind::AgentError),
        };
        let text = if error.is_none() {
            str_at(value, "/result").map(str::to_owned)
        } else {
            None
        };
        vec![
            Mapped::Event(Event::Usage(self.usage())),
            Mapped::Result(CliResult {
                error,
                text,
                num_turns,
            }),
        ]
    }
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn usage_of(usage: &Value) -> UsageTotals {
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    UsageTotals {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_tokens: n("cache_read_input_tokens"),
        cache_write_tokens: n("cache_creation_input_tokens"),
        turns: 0,
    }
}

/// 工具摘要（worker.md §7.4）：Bash 取命令行，读写取操作与路径，搜索取 pattern；
/// **从不**取写入内容、替换文本或命令输出。长度由事件写入器截断。
pub fn summarize(tool: &str, input: &Value) -> String {
    let s = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    match tool {
        "Bash" => s("command").to_owned(),
        "Read" => format!("read {}", s("file_path")),
        "Edit" | "MultiEdit" => format!("edit {}", s("file_path")),
        "Write" => format!("write {}", s("file_path")),
        "NotebookEdit" => format!("edit {}", s("notebook_path")),
        "Glob" => format!("glob {}", s("pattern")),
        "Grep" => {
            let path = s("path");
            if path.is_empty() {
                format!("grep {}", s("pattern"))
            } else {
                format!("grep {} in {path}", s("pattern"))
            }
        }
        "TodoWrite" => "update todo list".to_owned(),
        other => other.to_owned(),
    }
}

/// `stage` 推断（OQ-W8 暂用：按工具类别粗分）。
pub fn infer_stage(tool: &str, input: &Value) -> Option<ProgressStage> {
    match tool {
        "Read" | "Glob" | "Grep" => Some(ProgressStage::Explore),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Some(ProgressStage::Edit),
        "TodoWrite" => Some(ProgressStage::Plan),
        "Bash" => {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            const TEST_WORDS: [&str; 7] = [
                "test", "pytest", "jest", "vitest", "rspec", "phpunit", "ctest",
            ];
            command
                .split(|c: char| c.is_whitespace() || matches!(c, ':' | '/' | ';' | '&' | '|'))
                .any(|word| TEST_WORDS.contains(&word))
                .then_some(ProgressStage::Test)
        }
        _ => None,
    }
}
