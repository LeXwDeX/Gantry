//! 出网审计（`docs/modules/egress.md` §6.5、§7.8）：JSON Lines，按日轮转，追加写入，
//! **不可关闭**。
//!
//! 记录里只有 `job_id` / `svc_token_id` 之类的标识，没有令牌原文、头值、请求 / 响应体、
//! 查询串或密钥；文本字段去掉控制字符（I5）。写失败由调用方失败闭合（拒绝新请求、
//! `health = unhealthy`，OQ-E10）。

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gantry_core::egress::SvcTokenId;
use gantry_core::entities::{Tier, TokenUsage};
use gantry_core::ids::{AttemptId, Clock, Ulid};
use gantry_core::time::{Date, Timestamp};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditKind {
    Model,
    Connect,
    Registry,
    Control,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

/// 调用方：只记标识，不记令牌。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuditCaller {
    Job {
        job_id: AttemptId,
        tier: Tier,
    },
    Svc {
        svc_token_id: SvcTokenId,
        tier: Option<Tier>,
        call_id: Option<Ulid>,
    },
    /// 令牌无效或控制面对端不符：只记来源。
    Unknown {
        src: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AuditUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl From<TokenUsage> for AuditUsage {
    fn from(usage: TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }
    }
}

/// 一条审计事件；`seq` 与 `ts` 由写入方填。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    pub kind: AuditKind,
    pub caller: AuditCaller,
    pub decision: Decision,
    pub rule: Option<&'static str>,
    pub method: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub path: Option<String>,
    pub status: Option<u16>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub duration_ms: u64,
    pub provider: Option<String>,
    pub usage: Option<AuditUsage>,
    pub src: Option<String>,
    pub resolved_ip: Option<String>,
    pub error: Option<String>,
}

impl AuditEvent {
    pub fn new(kind: AuditKind, caller: AuditCaller, decision: Decision) -> Self {
        Self {
            kind,
            caller,
            decision,
            rule: None,
            method: None,
            host: None,
            port: None,
            path: None,
            status: None,
            bytes_in: 0,
            bytes_out: 0,
            duration_ms: 0,
            provider: None,
            usage: None,
            src: None,
            resolved_ip: None,
            error: None,
        }
    }
}

#[derive(Serialize)]
struct Line<'a> {
    seq: u64,
    ts: Timestamp,
    kind: AuditKind,
    caller: &'a AuditCaller,
    decision: Decision,
    rule: Option<&'static str>,
    method: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    status: Option<u16>,
    bytes_in: u64,
    bytes_out: u64,
    duration_ms: u64,
    provider: Option<String>,
    usage: Option<AuditUsage>,
    src: Option<String>,
    resolved_ip: Option<String>,
    /// M3 的哈希链；M1 留空。
    prev_hash: Option<String>,
    error: Option<String>,
}

fn render(seq: u64, ts: Timestamp, event: &AuditEvent) -> Result<String, AuditError> {
    let line = Line {
        seq,
        ts,
        kind: event.kind,
        caller: &event.caller,
        decision: event.decision,
        rule: event.rule,
        method: event.method.as_deref().map(clean),
        host: event.host.as_deref().map(clean),
        port: event.port,
        path: event.path.as_deref().map(clean_path),
        status: event.status,
        bytes_in: event.bytes_in,
        bytes_out: event.bytes_out,
        duration_ms: event.duration_ms,
        provider: event.provider.as_deref().map(clean),
        usage: event.usage,
        src: event.src.as_deref().map(clean),
        resolved_ip: event.resolved_ip.as_deref().map(clean),
        prev_hash: None,
        error: event.error.as_deref().map(clean),
    };
    let mut text = serde_json::to_string(&line).map_err(|e| AuditError(e.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// 去掉控制字符并截断（I5）。
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}

/// 只记路径，不记查询串与片段。
fn clean_path(value: &str) -> String {
    let path = value.split(['?', '#']).next().unwrap_or_default();
    clean(path)
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("audit write failed: {0}")]
pub struct AuditError(pub String);

/// 审计写入端。实现必须要么持久写入、要么返回错误（不得静默丢弃）。
pub trait AuditSink: Send + Sync {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError>;
}

/// `<dir>/YYYY-MM-DD.jsonl`，文件模式 0600，`seq` 跨重启单调。
pub struct JsonlAudit {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
    state: Mutex<JsonlState>,
}

struct JsonlState {
    seq: u64,
    day: Option<Date>,
    file: Option<File>,
}

impl JsonlAudit {
    pub fn open(dir: impl Into<PathBuf>, clock: Arc<dyn Clock>) -> Result<Self, AuditError> {
        let dir = dir.into();
        create_private_dir(&dir)?;
        let seq = last_seq(&dir)?;
        Ok(Self {
            dir,
            clock,
            state: Mutex::new(JsonlState {
                seq,
                day: None,
                file: None,
            }),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl AuditSink for JsonlAudit {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        let ts = self.clock.now();
        let day = ts.to_date();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.day != Some(day) || state.file.is_none() {
            let path = self.dir.join(format!("{day}.jsonl"));
            state.file = Some(open_append(&path)?);
            state.day = Some(day);
        }
        let seq = state.seq + 1;
        let line = render(seq, ts, &event)?;
        let file = state.file.as_mut().expect("opened above");
        if let Err(error) = file.write_all(line.as_bytes()) {
            // 下次重新打开文件；本条视为未写入。
            state.file = None;
            return Err(AuditError(error.to_string()));
        }
        state.seq = seq;
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), AuditError> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| AuditError(e.to_string()))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), AuditError> {
    fs::create_dir_all(dir).map_err(|e| AuditError(e.to_string()))
}

fn open_append(path: &Path) -> Result<File, AuditError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| AuditError(e.to_string()))
}

/// 最新一天文件的最后一条记录的 `seq`；没有文件为 0。
fn last_seq(dir: &Path) -> Result<u64, AuditError> {
    let mut latest: Option<PathBuf> = None;
    for entry in fs::read_dir(dir).map_err(|e| AuditError(e.to_string()))? {
        let path = entry.map_err(|e| AuditError(e.to_string()))?.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && latest.as_ref().is_none_or(|current| path > *current)
        {
            latest = Some(path);
        }
    }
    let Some(path) = latest else {
        return Ok(0);
    };
    let file = File::open(&path).map_err(|e| AuditError(e.to_string()))?;
    let mut seq = 0;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| AuditError(e.to_string()))?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(n) = value.get("seq").and_then(serde_json::Value::as_u64) {
                seq = seq.max(n);
            }
        }
    }
    Ok(seq)
}

/// 内存审计（测试用）：保存渲染后的行，可注入写失败。
#[derive(Default)]
pub struct MemoryAudit {
    state: Mutex<(u64, Vec<String>, bool)>,
}

impl MemoryAudit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lines(&self) -> Vec<String> {
        self.lock().1.clone()
    }

    pub fn events(&self) -> Vec<serde_json::Value> {
        self.lines()
            .iter()
            .map(|line| serde_json::from_str(line).expect("audit lines are JSON"))
            .collect()
    }

    /// 之后的写入都失败（模拟磁盘满）。
    pub fn fail_writes(&self, fail: bool) {
        self.lock().2 = fail;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (u64, Vec<String>, bool)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AuditSink for MemoryAudit {
    fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        let mut state = self.lock();
        if state.2 {
            return Err(AuditError("injected failure".into()));
        }
        let seq = state.0 + 1;
        let line = render(seq, Timestamp::from_millis(0), &event)?;
        state.0 = seq;
        state.1.push(line);
        Ok(())
    }
}
