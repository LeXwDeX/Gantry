//! 沙盒内执行者 `gantry-worker run`（K1；worker.md §7.1、§6.1、§9）。
//!
//! 流程：READY → 读 job.json 与 A1 校验 → 准备 HOME 配置 → 组装 prompt → 驱动 CLI 并翻译事件
//! → 校验最终产出（至多修复 1 轮）→ 生成补丁 / 报告 / 提议 → 最后原子写 `result.json`。
//! 收到 SIGTERM / SIGINT：终止 agent 进程组，跳过产物生成，写 `cancelled`，退出码 12。
//!
//! agent CLI 的 stdout / stderr 一律由管道接走，不继承 fd 1；worker 的 stdout 只有协议事件。

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gantry_core::canon::GitOid;
use gantry_core::time::Timestamp;
use gantry_core::worker::{
    AgentId, AgentReport, JobSpec, ResultArtifacts, ResultError, ResultErrorKind, ResultFlags,
    ResultJson, ResultStatus, StructuredOutput, UsageReported, WorkerProtocol,
};
use serde_json::Value;

use crate::artifacts::{write_proposals, write_report, write_result};
use crate::claude_cli::{build_invocation, write_home, Invocation};
use crate::events::{Clock, Event, EventWriter, MonotonicClock, WarnCode};
use crate::patch::{package_patch, CleanGit, GIT_TIMEOUT};
use crate::prompt::{build_prompt, parse_envelope, Envelope};
use crate::proposals::filter_proposals;
use crate::startup::{read_job, verify_manifest, BadJob, LoadedJob, Paths};
use crate::stream::{CliResult, Mapped, StreamMapper};
use crate::{layout, ClaudeCodeAdapter};

/// 终止宽限 **[建议值，待调优]**（OQ-W11：JobSpec 里没有服务端的宽限）。
pub const KILL_GRACE: Duration = Duration::from_secs(5);
/// 修复轮数（A6）。
pub const REPAIR_ROUNDS: usize = 1;
/// 单行 stream-json 上限；更长的行丢弃（只可能是不可信的超大输出）。
const MAX_CLI_LINE: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(50);

/// 退出码（worker.md §6.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    BadJob = 10,
    AgentError = 11,
    Aborted = 12,
}

/// 运行环境。生产环境是固定布局；测试经 `GANTRY_*` 指向临时目录（协议 §2.2）。
#[derive(Clone, Debug)]
pub struct RunEnv {
    pub paths: Paths,
    pub home: PathBuf,
    pub tmp: PathBuf,
    /// agent CLI 程序名，按 `PATH` 查找。
    pub program: OsString,
}

impl RunEnv {
    /// `GANTRY_JOB` / `GANTRY_OUT` / `GANTRY_WORK`，缺省取固定路径；`inputs/` 是 `job.json`
    /// 的同级目录（固定布局下即 `/gantry/inputs`）。
    pub fn from_env() -> Self {
        let var = |name: &str, default: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(default))
        };
        let job = var(layout::ENV_JOB, layout::JOB_JSON);
        let inputs = job
            .parent()
            .map(|dir| dir.join("inputs"))
            .unwrap_or_else(|| PathBuf::from(layout::INPUTS));
        Self {
            paths: Paths {
                job,
                inputs,
                work: var(layout::ENV_WORK, layout::WORK),
                out: var(layout::ENV_OUT, layout::OUT),
            },
            home: var("HOME", layout::HOME),
            tmp: var("TMPDIR", layout::TMP),
            program: OsString::from(crate::claude_cli::PROGRAM),
        }
    }
}

/// 由信号处理函数置位；第二次信号不改变流程。指向一个 `'static` 的原子量，信号处理函数
/// 只做一次原子写（async-signal-safe）。
#[derive(Clone, Copy, Debug)]
pub struct AbortFlag(&'static AtomicBool);

impl AbortFlag {
    /// 新建一个标志（泄漏一个原子量；每个进程只建少数几个）。
    pub fn new() -> Self {
        Self(Box::leak(Box::new(AtomicBool::new(false))))
    }

    pub fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_raised(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn atomic(&self) -> &'static AtomicBool {
        self.0
    }
}

impl Default for AbortFlag {
    fn default() -> Self {
        Self::new()
    }
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Timestamp::from_millis(ms)
}

/// stderr 诊断：每阶段一行，不含环境变量与工具输出正文。
struct Diag<E: Write> {
    err: E,
    started: Instant,
}

impl<E: Write> Diag<E> {
    fn phase(&mut self, phase: &str, note: &str) {
        let ms = self.started.elapsed().as_millis();
        let note: String = note.chars().filter(|c| !c.is_control()).take(300).collect();
        let _ = writeln!(self.err, "gantry-worker phase={phase} ms={ms} {note}");
    }
}

/// 执行一次 `run`。`out` 是 stdout（只写协议事件），`err` 是 stderr。
pub fn run<W: Write, E: Write>(env: &RunEnv, abort: &AbortFlag, out: W, err: E) -> Exit {
    let mut diag = Diag {
        err,
        started: Instant::now(),
    };
    let mut events = EventWriter::new(out, MonotonicClock::new());
    let started_at = now();
    let adapter = ClaudeCodeAdapter::new(placeholder_image());

    // 2. READY
    let ready = env.paths.inputs.join("READY");
    while !ready.exists() {
        if abort.is_raised() {
            diag.phase("ready", "aborted before READY");
            return Exit::Aborted;
        }
        thread::sleep(POLL);
    }

    // 3. load + A1
    let loaded = match load(env, &adapter) {
        Ok(loaded) => loaded,
        Err((job, reason)) => {
            diag.phase("load", &format!("error.kind=bad_job {reason}"));
            if let Some(job) = job {
                let _ = events.hello(&job.job.job_id, &AgentId::ClaudeCode, "unknown");
                let result = failure(
                    &job,
                    ResultStatus::Failed,
                    ResultErrorKind::BadJob,
                    &reason.to_string(),
                    started_at,
                );
                let _ = write_result(&env.paths.out, &result);
                let _ = events.finish(ResultStatus::Failed);
            }
            return Exit::BadJob;
        }
    };
    diag.phase("load", "ok");
    let job = &loaded.job;

    // 4. prepare
    let system_prompt;
    let invocation;
    let validator;
    let prompt;
    {
        prompt = build_prompt(job, |path| {
            resolve(env, path).and_then(|local| fs::read(local).ok())
        });
        system_prompt = prompt.system.clone();
        invocation = match build_invocation(job, &env.paths.work, &system_prompt) {
            Ok(inv) => inv,
            Err(e) => {
                return bad_job_after_load(
                    env,
                    &mut events,
                    &mut diag,
                    &loaded,
                    &e.to_string(),
                    started_at,
                )
            }
        };
        if let Err(e) = write_home(&env.home, &invocation) {
            diag.phase(
                "prepare",
                &format!("cannot write HOME config: {}", e.kind()),
            );
            return agent_error(
                env,
                &mut events,
                &loaded,
                "cannot prepare agent configuration",
                started_at,
            );
        }
        let schema_path = resolve(env, &job.output.structured_schema.path);
        let schema: Option<Value> = schema_path
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok());
        validator = match schema.as_ref().map(jsonschema::validator_for) {
            Some(Ok(v)) => v,
            _ => {
                return bad_job_after_load(
                    env,
                    &mut events,
                    &mut diag,
                    &loaded,
                    "output.schema.json does not compile",
                    started_at,
                )
            }
        };
    }
    diag.phase("prepare", crate::prompt::PREAMBLE_VERSION);

    // 6. agent
    let mut mapper = StreamMapper::new();
    let clock = MonotonicClock::new();
    let mut session = SessionState::default();
    let first = drive(
        env,
        &invocation,
        &[],
        &prompt.user,
        abort,
        &mut mapper,
        &clock,
        &mut events,
        &mut session,
        &loaded,
    );
    for id in &prompt.truncated_materials {
        let _ = events.emit(Event::Warn {
            code: WarnCode::MaterialTruncated,
            detail: format!("{id} was truncated by the server"),
        });
    }
    let mut cli_result = match first {
        Drive::Aborted => {
            return cancelled(
                env,
                &mut events,
                &mut diag,
                &loaded,
                &session,
                &mapper,
                started_at,
            )
        }
        Drive::SpawnFailed(reason) => {
            diag.phase(
                "agent",
                &format!("error.kind=agent_error cannot start agent: {reason}"),
            );
            return agent_error(
                env,
                &mut events,
                &loaded,
                "agent CLI could not be started",
                started_at,
            );
        }
        Drive::NoResult => {
            diag.phase(
                "agent",
                "error.kind=agent_error agent exited without a result",
            );
            return agent_error(
                env,
                &mut events,
                &loaded,
                "agent CLI exited without a result",
                started_at,
            );
        }
        Drive::Done(result) => result,
    };
    diag.phase("agent", "done");

    // 7. validate (+ repair)
    let mut validated = validate(&cli_result, &validator, job);
    let mut rounds = 0;
    while let (Err(errors), Some(session_id)) = (&validated, session.session_id.clone()) {
        if rounds >= REPAIR_ROUNDS || cli_result.error.is_some() {
            break;
        }
        rounds += 1;
        let _ = events.emit(Event::Warn {
            code: WarnCode::SchemaRepair,
            detail: format!("{} problem(s) in the final reply", errors.len()),
        });
        let repair_prompt = repair_message(errors);
        let resume = ["--resume".to_owned(), session_id];
        match drive(
            env,
            &invocation,
            &resume,
            &repair_prompt,
            abort,
            &mut mapper,
            &clock,
            &mut events,
            &mut session,
            &loaded,
        ) {
            Drive::Aborted => {
                return cancelled(
                    env,
                    &mut events,
                    &mut diag,
                    &loaded,
                    &session,
                    &mapper,
                    started_at,
                )
            }
            Drive::Done(result) => {
                cli_result = result;
                validated = validate(&cli_result, &validator, job);
            }
            Drive::SpawnFailed(_) | Drive::NoResult => break,
        }
    }
    diag.phase("validate", if validated.is_ok() { "ok" } else { "invalid" });
    if abort.is_raised() {
        return cancelled(
            env,
            &mut events,
            &mut diag,
            &loaded,
            &session,
            &mapper,
            started_at,
        );
    }

    // 8. package（agent 进程组已终止）
    let mut artifacts = ResultArtifacts {
        patch: None,
        report: None,
        proposals: None,
    };
    let mut internal: Option<String> = None;
    let scratch = env.tmp.join("gantry-worker-patch");
    let patch = fs::create_dir_all(&scratch)
        .map_err(crate::patch::PatchError::Setup)
        .and_then(|_| CleanGit::prepare(&env.paths.work, &scratch, GIT_TIMEOUT))
        .and_then(|mut git| package_patch(job, &mut git, &env.paths.out));
    match patch {
        Ok(outcome) => {
            for code in outcome.warnings {
                let _ = events.emit(Event::Warn {
                    code,
                    detail: "see changes.patch".into(),
                });
            }
            artifacts.patch = outcome.artifact;
        }
        Err(e) => {
            diag.phase("package", &format!("error.kind=internal {e}"));
            internal = Some("patch generation failed".into());
        }
    }
    let envelope = validated
        .as_ref()
        .ok()
        .map(|(env, _)| env.clone())
        .or_else(|| {
            cli_result
                .text
                .as_deref()
                .and_then(|t| parse_envelope(t).ok())
        });
    let mut flags = ResultFlags::default();
    if let Some(envelope) = &envelope {
        if let Some(report) = &envelope.report_markdown {
            match write_report(&env.paths.out, report) {
                Ok((reference, truncated)) => {
                    if truncated {
                        let _ = events.emit(Event::Warn {
                            code: WarnCode::ReportTruncated,
                            detail: "report.md exceeded 256 KiB".into(),
                        });
                    }
                    artifacts.report = Some(reference);
                }
                Err(_) => internal = Some("cannot write report.md".into()),
            }
        }
        let filtered = filter_proposals(job, &envelope.proposals);
        for (index, reason) in &filtered.dropped {
            let _ = events.emit(Event::Warn {
                code: WarnCode::ProposalDropped,
                detail: format!("proposal #{index}: {}", reason.as_str()),
            });
        }
        if let Some(proposals) = &filtered.proposals {
            match write_proposals(&env.paths.out, proposals) {
                Ok(reference) => artifacts.proposals = Some(reference),
                Err(_) => internal = Some("cannot write proposals.json".into()),
            }
        }
        flags.suspected_injection = envelope.suspected_injection.clone();
    }
    diag.phase("package", "done");

    // 9. finish
    let (status, error, structured_output, summary) =
        match (&validated, &cli_result.error, &internal) {
            (_, _, Some(message)) => (
                ResultStatus::Failed,
                Some(error_of(ResultErrorKind::Internal, message)),
                None,
                String::new(),
            ),
            (_, Some(kind), None) => (
                ResultStatus::Failed,
                Some(error_of(*kind, cli_error_message(*kind))),
                None,
                String::new(),
            ),
            (Ok((envelope, output)), None, None) => (
                ResultStatus::Succeeded,
                None,
                Some(output.clone()),
                summary_of(&envelope.structured_output),
            ),
            (Err(_), None, None) => (
                ResultStatus::Failed,
                Some(error_of(
                    ResultErrorKind::OutputInvalid,
                    "structured output did not satisfy the schema after repair",
                )),
                None,
                String::new(),
            ),
        };
    let result = ResultJson {
        protocol: WorkerProtocol,
        job_id: job.job_id,
        job_sha256: loaded.job_sha256,
        status,
        summary,
        error,
        structured_output,
        artifacts,
        flags,
        usage_reported: usage_reported(&mapper),
        agent: agent_report(&session, job),
        started_at,
        ended_at: now(),
    };
    if write_result(&env.paths.out, &result).is_err() {
        diag.phase("finish", "error.kind=internal cannot write result.json");
        return Exit::AgentError;
    }
    let _ = events.finish(status);
    diag.phase("finish", &format!("status={}", status_str(status)));
    Exit::Ok
}

/// 镜像引用只对服务端侧有意义；沙盒内的执行者不读它。
fn placeholder_image() -> gantry_core::ports::worker::ImageRef {
    gantry_core::ports::worker::ImageRef {
        reference: "in-sandbox".into(),
        digest: gantry_core::canon::sha256(b"in-sandbox"),
    }
}

fn load(
    env: &RunEnv,
    adapter: &ClaudeCodeAdapter,
) -> Result<LoadedJob, (Option<Box<LoadedJob>>, BadJob)> {
    let loaded = read_job(&env.paths, adapter).map_err(|e| (None, e))?;
    if let Err(e) = verify_manifest(&env.paths, &loaded) {
        return Err((Some(Box::new(loaded)), e));
    }
    match workspace_head(&env.paths.work) {
        Some(head) if head == loaded.job.workspace.base_sha => Ok(loaded),
        _ => Err((
            Some(Box::new(loaded)),
            BadJob::InvalidSpec("workspace HEAD does not match workspace.base_sha".into()),
        )),
    }
}

/// 读工作区 HEAD（不运行 git：只解析 `.git/HEAD`、松散引用与 `packed-refs`）。
/// 这只用于发现挂载错误；工作区 `.git` 不可信。
pub fn workspace_head(work: &Path) -> Option<GitOid> {
    let git = work.join(".git");
    let head = fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref: ") else {
        return head.parse().ok();
    };
    if reference.contains("..") || !reference.starts_with("refs/") {
        return None;
    }
    if let Ok(text) = fs::read_to_string(git.join(reference)) {
        return text.trim().parse().ok();
    }
    let packed = fs::read_to_string(git.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (oid, name) = line.split_once(' ')?;
        (name == reference).then(|| oid.parse().ok()).flatten()
    })
}

/// 容器内路径（`/gantry/inputs/...`）→ 本地路径。
fn resolve(env: &RunEnv, container_path: &str) -> Option<PathBuf> {
    let rest = container_path
        .strip_prefix(layout::INPUTS)?
        .strip_prefix('/')?;
    let rel = Path::new(rest);
    rel.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        .then(|| env.paths.inputs.join(rel))
}

#[derive(Debug, Default)]
struct SessionState {
    session_id: Option<String>,
    agent_version: Option<String>,
    model: Option<String>,
}

enum Drive {
    Done(CliResult),
    NoResult,
    SpawnFailed(String),
    Aborted,
}

#[allow(clippy::too_many_arguments)]
fn drive<W: Write>(
    env: &RunEnv,
    invocation: &Invocation,
    extra_args: &[String],
    stdin: &str,
    abort: &AbortFlag,
    mapper: &mut StreamMapper,
    clock: &MonotonicClock,
    events: &mut EventWriter<W, MonotonicClock>,
    session: &mut SessionState,
    loaded: &LoadedJob,
) -> Drive {
    let mut child = match spawn(env, invocation, extra_args, stdin) {
        Ok(child) => child,
        Err(e) => {
            if !events.hello_sent() {
                let _ = events.hello(&loaded.job.job_id, &AgentId::ClaudeCode, "unknown");
            }
            return Drive::SpawnFailed(e.kind().to_string());
        }
    };
    let mut result = None;
    loop {
        if abort.is_raised() {
            terminate(&mut child.process, KILL_GRACE);
            return Drive::Aborted;
        }
        match child.lines.recv_timeout(POLL) {
            Ok(line) => {
                for mapped in mapper.on_line(&line, clock.now()) {
                    match mapped {
                        Mapped::Init(init) => {
                            session.session_id =
                                init.session_id.clone().or(session.session_id.take());
                            session.agent_version = Some(init.agent_version.clone());
                            session.model = Some(init.model.clone());
                            let _ = events.hello(
                                &loaded.job.job_id,
                                &AgentId::ClaudeCode,
                                &init.agent_version,
                            );
                        }
                        Mapped::Event(event) => {
                            let _ = events.emit(event);
                        }
                        Mapped::Result(r) => result = Some(r),
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // stdout 已关闭：等进程退出，并确保整个进程组都已终止，补丁才不会再变。
    let deadline = Instant::now() + KILL_GRACE;
    loop {
        if abort.is_raised() {
            terminate(&mut child.process, KILL_GRACE);
            return Drive::Aborted;
        }
        if child.process.try_wait().ok().flatten().is_some() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(POLL);
    }
    terminate(&mut child.process, Duration::ZERO);
    if !events.hello_sent() {
        let _ = events.hello(&loaded.job.job_id, &AgentId::ClaudeCode, "unknown");
    }
    match result {
        Some(r) => Drive::Done(r),
        None => Drive::NoResult,
    }
}

struct Running {
    process: Child,
    lines: Receiver<String>,
}

fn spawn(
    env: &RunEnv,
    invocation: &Invocation,
    extra_args: &[String],
    stdin: &str,
) -> io::Result<Running> {
    let mut command = Command::new(&env.program);
    command
        .args(&invocation.args)
        .args(extra_args)
        .envs(&invocation.env)
        .current_dir(&env.paths.work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 独立进程组：终止时整组发信号。
        .process_group(0);
    let mut process = command.spawn()?;
    let mut input = process.stdin.take().expect("piped");
    let stdin = stdin.to_owned();
    thread::spawn(move || {
        let _ = input.write_all(stdin.as_bytes());
    });
    // agent 的 stderr 不转发（可能含工具输出正文），只排空。
    let mut stderr = process.stderr.take().expect("piped");
    thread::spawn(move || {
        let _ = io::copy(&mut stderr, &mut io::sink());
    });
    let stdout = process.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match (&mut reader)
                .take(MAX_CLI_LINE as u64 + 1)
                .read_until(b'\n', &mut buf)
            {
                Ok(0) => break,
                Ok(_) => {
                    if buf.len() > MAX_CLI_LINE && !buf.ends_with(b"\n") {
                        // 超长行：丢弃到行尾。
                        let mut skip = Vec::new();
                        if reader.read_until(b'\n', &mut skip).is_err() {
                            break;
                        }
                        continue;
                    }
                    let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    Ok(Running { process, lines: rx })
}

/// A7：向 agent 进程组发 SIGTERM，宽限内未退出则 SIGKILL。worker 是 PID 1（容器内）时，
/// 整个沙盒只有它与 agent 的后代，`kill(-1)` 连 `setsid` 脱离的孙进程一并终止。
fn terminate(child: &mut Child, grace: Duration) {
    let pgid = child.id() as libc::pid_t;
    let as_init = std::process::id() == 1;
    let signal_all = |sig: libc::c_int| unsafe {
        libc::killpg(pgid, sig);
        if as_init {
            libc::kill(-1, sig);
        }
    };
    if !grace.is_zero() {
        signal_all(libc::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(POLL);
        }
    }
    signal_all(libc::SIGKILL);
    let _ = child.wait();
}

type Validated = Result<(Envelope, StructuredOutput), Vec<String>>;

/// A6：信封 + `output.schema.json` + `structured_output.kind` 与 role 对应。
/// 错误只含路径与原因，不含 agent 原文。
fn validate(result: &CliResult, validator: &jsonschema::Validator, job: &JobSpec) -> Validated {
    let Some(text) = &result.text else {
        return Err(vec!["no final reply".into()]);
    };
    let envelope = parse_envelope(text).map_err(|e| vec![e.to_string()])?;
    let errors: Vec<String> = validator
        .iter_errors(&envelope.structured_output)
        .take(20)
        .map(|e| {
            format!(
                "structured_output{}: violates {}",
                e.instance_path(),
                e.schema_path()
            )
        })
        .collect();
    if !errors.is_empty() {
        return Err(errors);
    }
    let output: StructuredOutput = serde_json::from_value(envelope.structured_output.clone())
        .map_err(|_| vec!["structured_output does not match the protocol type".to_owned()])?;
    if output.kind() != StructuredOutput::expected_for_role(&job.node.role) {
        return Err(vec![
            "structured_output.kind does not match the node role".into()
        ]);
    }
    output
        .validate()
        .map_err(|e| vec![format!("structured_output: {e}")])?;
    Ok((envelope, output))
}

fn repair_message(errors: &[String]) -> String {
    let mut text =
        String::from("Your final reply did not satisfy the output contract. Problems:\n");
    for error in errors {
        text.push_str("- ");
        text.push_str(error);
        text.push('\n');
    }
    text.push_str(
        "Reply again with exactly one JSON object as specified in the system prompt, and nothing else.\n",
    );
    text
}

fn summary_of(output: &Value) -> String {
    let text = ["summary", "changes_summary", "answer"]
        .iter()
        .find_map(|k| output.get(*k).and_then(Value::as_str))
        .unwrap_or("");
    crate::events::clean(text, 1000)
}

fn error_of(kind: ResultErrorKind, message: &str) -> ResultError {
    ResultError {
        kind,
        message: message.chars().take(1000).collect(),
        retryable_hint: None,
    }
}

fn cli_error_message(kind: ResultErrorKind) -> &'static str {
    match kind {
        ResultErrorKind::MaxTurns => "agent reached max_turns",
        ResultErrorKind::Budget => "token budget exhausted",
        _ => "agent reported an execution error",
    }
}

fn status_str(status: ResultStatus) -> &'static str {
    match status {
        ResultStatus::Succeeded => "succeeded",
        ResultStatus::Failed => "failed",
        ResultStatus::Cancelled => "cancelled",
    }
}

fn usage_reported(mapper: &StreamMapper) -> UsageReported {
    let u = mapper.usage();
    UsageReported {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_write_tokens: u.cache_write_tokens,
        turns: u.turns,
        tool_calls: mapper.tool_calls(),
    }
}

fn agent_report(session: &SessionState, job: &JobSpec) -> AgentReport {
    AgentReport {
        kind: AgentId::ClaudeCode,
        version: session
            .agent_version
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        model: session
            .model
            .clone()
            .unwrap_or_else(|| job.execution.model.clone()),
    }
}

fn failure(
    loaded: &LoadedJob,
    status: ResultStatus,
    kind: ResultErrorKind,
    message: &str,
    started_at: Timestamp,
) -> ResultJson {
    ResultJson {
        protocol: WorkerProtocol,
        job_id: loaded.job.job_id,
        job_sha256: loaded.job_sha256,
        status,
        summary: String::new(),
        error: Some(error_of(kind, message)),
        structured_output: None,
        artifacts: ResultArtifacts {
            patch: None,
            report: None,
            proposals: None,
        },
        flags: ResultFlags::default(),
        usage_reported: UsageReported::default(),
        agent: AgentReport {
            kind: AgentId::ClaudeCode,
            version: "unknown".into(),
            model: loaded.job.execution.model.clone(),
        },
        started_at,
        ended_at: now(),
    }
}

fn bad_job_after_load<W: Write, E: Write>(
    env: &RunEnv,
    events: &mut EventWriter<W, MonotonicClock>,
    diag: &mut Diag<E>,
    loaded: &LoadedJob,
    reason: &str,
    started_at: Timestamp,
) -> Exit {
    diag.phase("prepare", &format!("error.kind=bad_job {reason}"));
    let _ = events.hello(&loaded.job.job_id, &AgentId::ClaudeCode, "unknown");
    let result = failure(
        loaded,
        ResultStatus::Failed,
        ResultErrorKind::BadJob,
        reason,
        started_at,
    );
    let _ = write_result(&env.paths.out, &result);
    let _ = events.finish(ResultStatus::Failed);
    Exit::BadJob
}

fn agent_error<W: Write>(
    env: &RunEnv,
    events: &mut EventWriter<W, MonotonicClock>,
    loaded: &LoadedJob,
    message: &str,
    started_at: Timestamp,
) -> Exit {
    let _ = events.hello(&loaded.job.job_id, &AgentId::ClaudeCode, "unknown");
    let result = failure(
        loaded,
        ResultStatus::Failed,
        ResultErrorKind::AgentError,
        message,
        started_at,
    );
    let _ = write_result(&env.paths.out, &result);
    let _ = events.finish(ResultStatus::Failed);
    Exit::AgentError
}

/// A7：跳过补丁与报告，写 `cancelled`。
fn cancelled<W: Write, E: Write>(
    env: &RunEnv,
    events: &mut EventWriter<W, MonotonicClock>,
    diag: &mut Diag<E>,
    loaded: &LoadedJob,
    session: &SessionState,
    mapper: &StreamMapper,
    started_at: Timestamp,
) -> Exit {
    diag.phase("finish", "error.kind=aborted signal received");
    let _ = events.hello(&loaded.job.job_id, &AgentId::ClaudeCode, "unknown");
    let mut result = failure(
        loaded,
        ResultStatus::Cancelled,
        ResultErrorKind::Aborted,
        "terminated by signal",
        started_at,
    );
    result.usage_reported = usage_reported(mapper);
    result.agent = agent_report(session, &loaded.job);
    let _ = write_result(&env.paths.out, &result);
    let _ = events.finish(ResultStatus::Cancelled);
    Exit::Aborted
}
