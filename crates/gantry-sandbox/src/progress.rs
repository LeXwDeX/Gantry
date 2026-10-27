//! 进度采集（sandbox.md §7.6，worker-protocol §7）。
//!
//! 输入是驱动日志接口给出的行（不可信自报）。每行：长度上限 → 严格 JSON →
//! `ProgressEvent` 反序列化（未知 `type` 丢弃）→ `v == 1` → `seq` 严格递增 → 速率与总数
//! 上限。字符串字段去 ANSI / 控制符 / bidi / Tag 并截断；worker 的 `ts` 忽略，打服务端
//! `received_at`。超限只丢弃并计数，调用方继续排空日志流，不让容器因管道写满而阻塞。
//!
//! 事件只用于展示：本模块不产出任何终态，`finish` 与自报 `usage` 不影响 `JobOutcome`（W3）。
//! 合法事件（含因限速被丢弃的）重置空闲计时，由 [`LineOutcome::activity`] 告诉调用方。

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use gantry_core::ids::AttemptId;
use gantry_core::json_strict::{parse_json_strict, JsonLimits};
use gantry_core::ports::sandbox::{ProgressSample, ProgressStats};
use gantry_core::settings::resource::ProgressIo;
use gantry_core::text::{normalize_untrusted, NormalizeLimits, UntrustedText};
use gantry_core::time::Timestamp;
use gantry_core::worker::{ProgressEvent, ProgressStage};
use serde::Serialize;
use tokio::time::Instant;

use crate::driver::LogStream;

/// 展示字段的上限（worker-protocol §7.1：`detail` / `summary` ≤ 200 字符）。
pub const MAX_TEXT_CHARS: usize = 200;

/// 采集参数：`sandbox.io.progress.*`、`logs_tail_bytes` 与采样间隔。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressLimits {
    pub max_line_bytes: u64,
    pub max_lines: u64,
    pub max_rate_per_s: u64,
    pub logs_tail_bytes: u64,
    /// `attempt.progress` 的采样间隔（`sandbox.progress_sample_ms`，默认 1 s）。
    pub sample_interval: Duration,
}

impl ProgressLimits {
    pub fn new(io: &ProgressIo, logs_tail_bytes: u64, sample_interval: Duration) -> Self {
        Self {
            max_line_bytes: io.max_line_bytes,
            max_lines: io.max_lines,
            max_rate_per_s: io.max_rate_per_s,
            logs_tail_bytes,
            sample_interval,
        }
    }
}

/// 丢弃原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    TooLong,
    NotJson,
    InvalidEvent,
    UnknownType,
    Version,
    Sequence,
    Rate,
    Total,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLong => "too_long",
            Self::NotJson => "not_json",
            Self::InvalidEvent => "invalid_event",
            Self::UnknownType => "unknown_type",
            Self::Version => "version",
            Self::Sequence => "sequence",
            Self::Rate => "rate",
            Self::Total => "total",
        }
    }
}

/// 一行的处理结果。
#[derive(Clone, Debug, PartialEq)]
pub enum LineOutcome {
    /// 合法事件，已接受（写 spool、参与采样）。
    Accepted(AcceptedEvent),
    /// 丢弃；`activity = true` 表示它是合法事件，只是被限速 / 总数上限丢弃，仍重置空闲计时。
    Dropped { reason: DropReason, activity: bool },
    /// 重启后重读到的已接受事件（`seq` ≤ spool 的最后一条）：不计数、不重复写。
    Replayed,
    /// 非协议输出（stderr 或 stdout 的非 JSON 行）：只进尾部。
    Tail,
}

impl LineOutcome {
    /// 是否重置空闲计时（§7.5：合法事件，包括因限速被丢弃的）。
    pub fn activity(&self) -> bool {
        matches!(
            self,
            Self::Accepted(_) | Self::Dropped { activity: true, .. }
        )
    }
}

/// 已接受的事件：字符串已净化，带服务端时间。写进 `progress.spool.jsonl` 的就是它。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AcceptedEvent {
    pub received_at: Timestamp,
    pub event: ProgressEvent,
}

impl AcceptedEvent {
    pub fn seq(&self) -> u64 {
        self.event.seq()
    }

    fn kind(&self) -> &'static str {
        match &self.event {
            ProgressEvent::Hello(_) => "hello",
            ProgressEvent::Stage(_) => "stage",
            ProgressEvent::Tool(_) => "tool",
            ProgressEvent::Usage(_) => "usage",
            ProgressEvent::Heartbeat(_) => "heartbeat",
            ProgressEvent::Warn(_) => "warn",
            ProgressEvent::Finish(_) => "finish",
        }
    }

    /// spool 的一行（JSON + `\n`）。
    pub fn spool_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).unwrap_or_default();
        line.push(b'\n');
        line
    }
}

/// 只保留末尾 `cap` 字节的缓冲。
#[derive(Clone, Debug, Default)]
pub struct TailBuffer {
    cap: usize,
    bytes: VecDeque<u8>,
    total: u64,
}

impl TailBuffer {
    pub fn new(cap: u64) -> Self {
        Self {
            cap: usize::try_from(cap).unwrap_or(usize::MAX),
            bytes: VecDeque::new(),
            total: 0,
        }
    }

    pub fn push_line(&mut self, line: &[u8]) {
        self.total += line.len() as u64 + 1;
        if self.cap == 0 {
            return;
        }
        // 单行比上限还长时只看它的末尾。
        let start = line.len().saturating_sub(self.cap);
        self.bytes.extend(&line[start..]);
        self.bytes.push_back(b'\n');
        while self.bytes.len() > self.cap {
            self.bytes.pop_front();
        }
    }

    pub fn contents(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    /// 曾经写入的总字节数（含被挤出的部分）。
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// 一个 job 的进度采集器。
#[derive(Clone, Debug)]
pub struct ProgressCollector {
    job_id: AttemptId,
    limits: ProgressLimits,
    last_seq: u64,
    /// 重启前 spool 里最后一条的 seq；重读到 ≤ 它的事件视为重放。
    replay_until: u64,
    accepted: u64,
    /// 计入总数上限的合法事件数（接受 + 因限速丢弃）。
    legal: u64,
    dropped: BTreeMap<DropReason, u64>,
    window_start: Option<Instant>,
    window_count: u64,
    stdout_tail: TailBuffer,
    stderr_tail: TailBuffer,
    /// 采样：同一类事件只留最新一条。
    pending: BTreeMap<&'static str, AcceptedEvent>,
    last_flush: Option<Instant>,
    last_stage: ProgressStage,
}

impl ProgressCollector {
    pub fn new(job_id: AttemptId, limits: ProgressLimits) -> Self {
        Self {
            job_id,
            limits,
            last_seq: 0,
            replay_until: 0,
            accepted: 0,
            legal: 0,
            dropped: BTreeMap::new(),
            window_start: None,
            window_count: 0,
            stdout_tail: TailBuffer::new(limits.logs_tail_bytes),
            stderr_tail: TailBuffer::new(limits.logs_tail_bytes),
            pending: BTreeMap::new(),
            last_flush: None,
            last_stage: ProgressStage::Init,
        }
    }

    /// 重启恢复：从 spool 里已接受的事件重建 seq 与计数（§7.10「从头重读日志，按 seq 去重」）。
    pub fn resume(job_id: AttemptId, limits: ProgressLimits, spool: &[AcceptedEvent]) -> Self {
        Self::resume_seqs(job_id, limits, spool.iter().map(AcceptedEvent::seq))
    }

    /// 同 [`Self::resume`]，直接读 `progress.spool.jsonl` 的字节（每行取 `event.seq`；
    /// 写到一半的末行与坏行跳过）。
    pub fn resume_from_spool(job_id: AttemptId, limits: ProgressLimits, spool: &[u8]) -> Self {
        let seqs = spool.split(|b| *b == b'\n').filter_map(|line| {
            let value: serde_json::Value = serde_json::from_slice(line).ok()?;
            value.get("event")?.get("seq")?.as_u64()
        });
        Self::resume_seqs(job_id, limits, seqs)
    }

    fn resume_seqs(
        job_id: AttemptId,
        limits: ProgressLimits,
        seqs: impl IntoIterator<Item = u64>,
    ) -> Self {
        let mut collector = Self::new(job_id, limits);
        for seq in seqs {
            if seq > collector.last_seq {
                collector.last_seq = seq;
                collector.accepted += 1;
                collector.legal += 1;
            }
        }
        collector.replay_until = collector.last_seq;
        collector
    }

    pub fn push_line(
        &mut self,
        stream: LogStream,
        line: &[u8],
        now: Instant,
        received_at: Timestamp,
    ) -> LineOutcome {
        if stream == LogStream::Stderr {
            self.stderr_tail.push_line(line);
            return LineOutcome::Tail;
        }
        if line.len() as u64 > self.limits.max_line_bytes {
            return self.drop(DropReason::TooLong, false);
        }
        let Ok(value) = parse_json_strict(line, JsonLimits::permissive()) else {
            self.stdout_tail.push_line(line);
            self.count(DropReason::NotJson);
            return LineOutcome::Tail;
        };
        if !value.is_object() {
            self.stdout_tail.push_line(line);
            self.count(DropReason::NotJson);
            return LineOutcome::Tail;
        }
        let known_type = value.get("type").and_then(|t| t.as_str()).is_some_and(|t| {
            matches!(
                t,
                "hello" | "stage" | "tool" | "usage" | "heartbeat" | "warn" | "finish"
            )
        });
        if !known_type {
            return self.drop(DropReason::UnknownType, false);
        }
        let Ok(event) = serde_json::from_value::<ProgressEvent>(value) else {
            return self.drop(DropReason::InvalidEvent, false);
        };
        if event.version() != 1 {
            return self.drop(DropReason::Version, false);
        }
        let seq = event.seq();
        if seq <= self.last_seq {
            if seq <= self.replay_until {
                return LineOutcome::Replayed;
            }
            return self.drop(DropReason::Sequence, false);
        }
        // 从这里起是合法事件：seq 推进、重置空闲计时，即使随后被限速丢弃。
        self.last_seq = seq;
        self.replay_until = 0;
        if self.legal >= self.limits.max_lines {
            return self.drop(DropReason::Total, true);
        }
        self.legal += 1;
        if !self.within_rate(now) {
            return self.drop(DropReason::Rate, true);
        }
        let accepted = AcceptedEvent {
            received_at,
            event: sanitize(event),
        };
        self.accepted += 1;
        if let ProgressEvent::Stage(stage) = &accepted.event {
            self.last_stage = stage.stage;
        }
        self.pending.insert(accepted.kind(), accepted.clone());
        LineOutcome::Accepted(accepted)
    }

    fn within_rate(&mut self, now: Instant) -> bool {
        match self.window_start {
            Some(start) if now.duration_since(start) < Duration::from_secs(1) => {}
            _ => {
                self.window_start = Some(now);
                self.window_count = 0;
            }
        }
        if self.window_count >= self.limits.max_rate_per_s {
            return false;
        }
        self.window_count += 1;
        true
    }

    fn count(&mut self, reason: DropReason) {
        *self.dropped.entry(reason).or_default() += 1;
    }

    fn drop(&mut self, reason: DropReason, activity: bool) -> LineOutcome {
        self.count(reason);
        LineOutcome::Dropped { reason, activity }
    }

    /// 到采样间隔时取出待发的样本（每类事件最新一条），否则返回空。
    pub fn sample(&mut self, now: Instant) -> Vec<ProgressSample> {
        if let Some(last) = self.last_flush {
            if now.duration_since(last) < self.limits.sample_interval {
                return Vec::new();
            }
        }
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.last_flush = Some(now);
        let mut samples: Vec<ProgressSample> = std::mem::take(&mut self.pending)
            .into_values()
            .map(|event| self.to_sample(&event))
            .collect();
        samples.sort_by_key(|sample| sample.seq);
        samples
    }

    fn to_sample(&self, accepted: &AcceptedEvent) -> ProgressSample {
        let (stage, detail) = match &accepted.event {
            ProgressEvent::Stage(event) => (event.stage, event.detail.clone()),
            ProgressEvent::Tool(event) => (
                self.last_stage,
                format!(
                    "{} {}",
                    event.tool,
                    event.summary.clone().unwrap_or_default()
                ),
            ),
            ProgressEvent::Usage(event) => (
                self.last_stage,
                format!(
                    "self-reported usage: in={} out={} turns={}",
                    event.input_tokens, event.output_tokens, event.turns
                ),
            ),
            ProgressEvent::Warn(event) => {
                (self.last_stage, format!("{}: {}", event.code, event.detail))
            }
            ProgressEvent::Finish(_) => (self.last_stage, "self-reported finish".into()),
            ProgressEvent::Hello(event) => (
                self.last_stage,
                format!("{} {}", event.agent, event.agent_version),
            ),
            ProgressEvent::Heartbeat(event) => {
                (self.last_stage, format!("in_flight={}", event.in_flight))
            }
        };
        ProgressSample {
            job_id: self.job_id,
            seq: accepted.seq(),
            stage,
            detail: UntrustedText::new(clean(&detail), "sandbox:progress".into(), false),
            at: accepted.received_at,
        }
    }

    pub fn stats(&self) -> ProgressStats {
        ProgressStats {
            samples: self.accepted,
            dropped: self.dropped.values().sum(),
        }
    }

    /// 分项丢弃计数（日志与指标 `sandbox_progress_dropped_total{reason}`）。
    pub fn dropped_by_reason(&self) -> &BTreeMap<DropReason, u64> {
        &self.dropped
    }

    pub fn stdout_tail(&self) -> &TailBuffer {
        &self.stdout_tail
    }

    pub fn stderr_tail(&self) -> &TailBuffer {
        &self.stderr_tail
    }
}

/// 去控制字符并截断到 [`MAX_TEXT_CHARS`]。
fn clean(text: &str) -> String {
    let normalized = normalize_untrusted(
        text.as_bytes(),
        NormalizeLimits {
            max_bytes: usize::MAX,
            head_bytes: 0,
            tail_bytes: 0,
        },
    );
    normalized
        .text
        .replace(['\n', '\t'], " ")
        .chars()
        .take(MAX_TEXT_CHARS)
        .collect()
}

fn sanitize(event: ProgressEvent) -> ProgressEvent {
    match event {
        ProgressEvent::Hello(mut e) => {
            e.agent_version = clean(&e.agent_version);
            ProgressEvent::Hello(e)
        }
        ProgressEvent::Stage(mut e) => {
            e.detail = clean(&e.detail);
            ProgressEvent::Stage(e)
        }
        ProgressEvent::Tool(mut e) => {
            e.call_id = clean(&e.call_id);
            e.tool = clean(&e.tool);
            e.summary = e.summary.map(|s| clean(&s));
            ProgressEvent::Tool(e)
        }
        ProgressEvent::Warn(mut e) => {
            e.code = clean(&e.code);
            e.detail = clean(&e.detail);
            ProgressEvent::Warn(e)
        }
        other => other,
    }
}
