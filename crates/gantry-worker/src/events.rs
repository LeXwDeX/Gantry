//! 进度事件写入器（A4；worker-protocol §7.1；worker.md §7.4）。
//!
//! 唯一拥有 stdout 的写入者：`hello` 必须是第一行，`seq` 从 1 严格递增，只输出白名单字段，
//! 字符串去控制符并截断，每行 ≤ 8 KiB。`usage`（累计值）与 `heartbeat` 合并节流到
//! ≤ 1 次/秒，`tool` 与 `stage` 不丢。事件只是不可信自报，服务端不据此判定终态。

use std::io::{self, Write};
use std::time::{Duration, Instant};

use gantry_core::ids::AttemptId;
use gantry_core::worker::{
    AgentId, FinishEvent, HeartbeatEvent, HelloEvent, ProgressEvent, ProgressStage, ResultStatus,
    StageEvent, ToolEvent, ToolPhase, UsageEvent, WarnEvent, PROGRESS_EVENT_VERSION,
};

/// 每行上限（worker-protocol §7.2）。
pub const MAX_LINE_BYTES: usize = 8 * 1024;
/// `detail` / `summary` 上限（字符）。
pub const MAX_TEXT_CHARS: usize = 200;
/// 其余短字符串（`call_id`、`tool`、`agent_version`）上限（字符）。
pub const MAX_SHORT_CHARS: usize = 64;
/// `usage` 与 `heartbeat` 的合并间隔 **[建议值，待调优]**。
pub const COALESCE_INTERVAL: Duration = Duration::from_secs(1);

/// `warn.code` 的小词表 **[提议]**（worker.md §7.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarnCode {
    SchemaRepair,
    PatchTooLarge,
    BinaryDetected,
    GitlinkDetected,
    SymlinkDetected,
    ProposalDropped,
    MaterialTruncated,
    ReportTruncated,
}

impl WarnCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SchemaRepair => "schema_repair",
            Self::PatchTooLarge => "patch_too_large",
            Self::BinaryDetected => "binary_detected",
            Self::GitlinkDetected => "gitlink_detected",
            Self::SymlinkDetected => "symlink_detected",
            Self::ProposalDropped => "proposal_dropped",
            Self::MaterialTruncated => "material_truncated",
            Self::ReportTruncated => "report_truncated",
        }
    }
}

/// 累计用量（自报，仅展示）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub turns: u64,
}

impl UsageTotals {
    /// 逐项取大：累计值单调不减，迟到或回退的样本不会让展示倒退。
    fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.max(other.input_tokens),
            output_tokens: self.output_tokens.max(other.output_tokens),
            cache_read_tokens: self.cache_read_tokens.max(other.cache_read_tokens),
            cache_write_tokens: self.cache_write_tokens.max(other.cache_write_tokens),
            turns: self.turns.max(other.turns),
        }
    }
}

/// 单调时钟；测试注入假时钟。
pub trait Clock {
    fn now(&self) -> Duration;
}

/// 进程启动后的单调时间。
pub struct MonotonicClock(Instant);

impl MonotonicClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// 事件的输入形状；`v` 与 `seq` 由写入器填。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Stage {
        stage: ProgressStage,
        detail: String,
    },
    ToolStart {
        call_id: String,
        tool: String,
        summary: String,
    },
    ToolEnd {
        call_id: String,
        tool: String,
        ok: bool,
        duration_ms: u64,
    },
    Usage(UsageTotals),
    Heartbeat {
        in_flight: u32,
    },
    Warn {
        code: WarnCode,
        detail: String,
    },
}

pub struct EventWriter<W: Write, C: Clock> {
    out: W,
    clock: C,
    seq: u64,
    hello_sent: bool,
    finished: bool,
    usage: UsageTotals,
    usage_emitted: Option<UsageTotals>,
    usage_at: Option<Duration>,
    heartbeat_pending: Option<u32>,
    heartbeat_at: Option<Duration>,
}

impl<W: Write, C: Clock> EventWriter<W, C> {
    pub fn new(out: W, clock: C) -> Self {
        Self {
            out,
            clock,
            seq: 0,
            hello_sent: false,
            finished: false,
            usage: UsageTotals::default(),
            usage_emitted: None,
            usage_at: None,
            heartbeat_pending: None,
            heartbeat_at: None,
        }
    }

    pub fn hello_sent(&self) -> bool {
        self.hello_sent
    }

    /// 当前累计用量（写 `result.json.usage_reported` 用）。
    pub fn usage(&self) -> UsageTotals {
        self.usage
    }

    /// 第一行。重复调用被忽略。
    pub fn hello(
        &mut self,
        job_id: &AttemptId,
        agent: &AgentId,
        agent_version: &str,
    ) -> io::Result<()> {
        if self.hello_sent {
            return Ok(());
        }
        self.hello_sent = true;
        let seq = self.next_seq();
        self.write(ProgressEvent::Hello(HelloEvent {
            v: PROGRESS_EVENT_VERSION,
            seq,
            job_id: *job_id,
            agent: agent.clone(),
            agent_version: clean(agent_version, MAX_SHORT_CHARS),
        }))
    }

    /// 发一个事件。`hello` 之前或 `finish` 之后的事件被丢弃（返回 `Ok(false)`）。
    pub fn emit(&mut self, event: Event) -> io::Result<bool> {
        if !self.hello_sent || self.finished {
            return Ok(false);
        }
        let now = self.clock.now();
        match event {
            Event::Usage(totals) => {
                self.usage = self.usage.merge(totals);
                if due(self.usage_at, now) {
                    return self.flush_usage(now).map(|_| true);
                }
                return Ok(false);
            }
            Event::Heartbeat { in_flight } => {
                if due(self.heartbeat_at, now) {
                    self.heartbeat_pending = Some(in_flight);
                    return self.flush_heartbeat(now).map(|_| true);
                }
                self.heartbeat_pending = Some(in_flight);
                return Ok(false);
            }
            _ => {}
        }
        // 合并中的样本到期后，借下一个事件顺带发出。
        self.flush_due(now)?;
        let seq = self.next_seq();
        let line = match event {
            Event::Stage { stage, detail } => ProgressEvent::Stage(StageEvent {
                v: PROGRESS_EVENT_VERSION,
                seq,
                stage,
                detail: clean(&detail, MAX_TEXT_CHARS),
            }),
            Event::ToolStart {
                call_id,
                tool,
                summary,
            } => ProgressEvent::Tool(ToolEvent {
                v: PROGRESS_EVENT_VERSION,
                seq,
                phase: ToolPhase::Start,
                call_id: clean(&call_id, MAX_SHORT_CHARS),
                tool: clean(&tool, MAX_SHORT_CHARS),
                summary: Some(clean(&summary, MAX_TEXT_CHARS)),
                ok: None,
                duration_ms: None,
            }),
            Event::ToolEnd {
                call_id,
                tool,
                ok,
                duration_ms,
            } => ProgressEvent::Tool(ToolEvent {
                v: PROGRESS_EVENT_VERSION,
                seq,
                phase: ToolPhase::End,
                call_id: clean(&call_id, MAX_SHORT_CHARS),
                tool: clean(&tool, MAX_SHORT_CHARS),
                summary: None,
                ok: Some(ok),
                duration_ms: Some(duration_ms),
            }),
            Event::Warn { code, detail } => ProgressEvent::Warn(WarnEvent {
                v: PROGRESS_EVENT_VERSION,
                seq,
                code: code.as_str().to_owned(),
                detail: clean(&detail, MAX_TEXT_CHARS),
            }),
            Event::Usage(_) | Event::Heartbeat { .. } => unreachable!("handled above"),
        };
        self.write(line).map(|_| true)
    }

    /// 最后一行：先补发合并中的 `usage`，再写 `finish`；之后不再输出任何事件。
    pub fn finish(&mut self, status: ResultStatus) -> io::Result<()> {
        if !self.hello_sent || self.finished {
            return Ok(());
        }
        if self.usage_emitted != Some(self.usage) && self.usage != UsageTotals::default() {
            let now = self.clock.now();
            self.flush_usage(now)?;
        }
        self.finished = true;
        let seq = self.next_seq();
        self.write(ProgressEvent::Finish(FinishEvent {
            v: PROGRESS_EVENT_VERSION,
            seq,
            status,
        }))
    }

    pub fn into_inner(self) -> W {
        self.out
    }

    fn flush_due(&mut self, now: Duration) -> io::Result<()> {
        if self.usage_emitted != Some(self.usage)
            && self.usage != UsageTotals::default()
            && due(self.usage_at, now)
        {
            self.flush_usage(now)?;
        }
        if self.heartbeat_pending.is_some() && due(self.heartbeat_at, now) {
            self.flush_heartbeat(now)?;
        }
        Ok(())
    }

    fn flush_usage(&mut self, now: Duration) -> io::Result<()> {
        let totals = self.usage;
        let seq = self.next_seq();
        self.usage_at = Some(now);
        self.usage_emitted = Some(totals);
        self.write(ProgressEvent::Usage(UsageEvent {
            v: PROGRESS_EVENT_VERSION,
            seq,
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cache_read_tokens: totals.cache_read_tokens,
            cache_write_tokens: totals.cache_write_tokens,
            turns: totals.turns,
        }))
    }

    fn flush_heartbeat(&mut self, now: Duration) -> io::Result<()> {
        let Some(in_flight) = self.heartbeat_pending.take() else {
            return Ok(());
        };
        let seq = self.next_seq();
        self.heartbeat_at = Some(now);
        self.write(ProgressEvent::Heartbeat(HeartbeatEvent {
            v: PROGRESS_EVENT_VERSION,
            seq,
            in_flight,
        }))
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn write(&mut self, event: ProgressEvent) -> io::Result<()> {
        let mut line = serde_json::to_vec(&event).map_err(io::Error::other)?;
        // 所有字符串都已截断，正常不会超长；超长说明有缺陷，宁可不写也不写半行。
        if line.len() >= MAX_LINE_BYTES {
            return Err(io::Error::other("progress event exceeds the line limit"));
        }
        line.push(b'\n');
        self.out.write_all(&line)?;
        self.out.flush()
    }
}

fn due(last: Option<Duration>, now: Duration) -> bool {
    last.is_none_or(|at| now.saturating_sub(at) >= COALESCE_INTERVAL)
}

/// 去 ANSI 转义、控制符、bidi 控制符与 Unicode Tag 字符，再按字符截断
/// （worker-protocol §4.4 第 2 步的同一组字符）。
pub fn clean(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 4));
    let mut chars = text.chars().peekable();
    let mut count = 0;
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI：ESC [ 参数 … 终止字节 0x40–0x7E；其他 ESC 序列吞掉下一个字符。
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if c.is_control()
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{e0000}'..='\u{e007f}')
        {
            continue;
        }
        if count == max_chars {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}
