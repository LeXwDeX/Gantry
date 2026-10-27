//! `PodmanDriver`：经 `podman --remote` 调用 `gantry-sbx` 的 Podman 套接字（sandbox.md §7.3、§7.7）。
//!
//! 每次调用是一个子进程：参数以数组传递，不拼 shell；环境变量清空后只给
//! `PATH` / `HOME` / `LC_ALL`；有墙钟超时与输出上限。容器参数只来自
//! [`render_create_args`]，它先过 `validate_invariants`。
//!
//! 本驱动在 macOS / A 级环境里只能用假 `podman` 脚本验证命令构造与输出解析；
//! 真实的运行时行为（`stop` 后无残留进程、OOM 报告、日志轮转、`cp` 对 FIFO 的处理等）
//! **需核实**，由 `[集成:podman]` 用例在 B 级环境（Linux）里验证。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::driver::{
    ContainerState, ContainerStatus, ContainerSummary, DriverCaps, DriverError, ExitFacts,
    LabelSelector, LogChunk, LogCursor, LogStream, SandboxDriver,
};
use crate::intake::{IntakePlan, IntakeResult};
use crate::invariants::{InvariantContext, InvariantViolation};
use crate::podman::render_create_args;
use crate::spec::{
    ContainerId, ContainerSpec, ImageDigest, Runtime, StagingLayout, LABEL_SANDBOX_ID,
};

/// `out/` 的读取方式（§7.8；选哪种取决于 OQ-S2 的属主方案）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutReadMode {
    /// 方式 A：作业目录对 `gantryd` 可读（开发 / CI：同一用户）。
    HostDirect,
    /// 方式 B：经 `podman cp <id>:/gantry/out -` 的 tar 流。
    Archive,
}

#[derive(Clone, Debug)]
pub struct PodmanConfig {
    /// `podman` 可执行文件的绝对路径。
    pub podman_bin: PathBuf,
    /// `sandbox.runtime_socket`（生产 `/run/gantry-sbx/podman.sock`）。
    pub socket: PathBuf,
    pub workspaces_dir: PathBuf,
    pub runtime: Runtime,
    pub out_read: OutReadMode,
    /// 单次调用（`wait` / `logs --follow` 除外）的墙钟上限。
    pub call_timeout: Duration,
    /// 单次调用 stdout 上限。
    pub max_output: usize,
    /// 由启动自检探测后填入（cgroup 控制器委派、OOM 报告等）。
    pub caps: DriverCaps,
}

pub struct PodmanDriver {
    config: PodmanConfig,
}

struct Output {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

const STDERR_TAIL: usize = 4096;

impl PodmanDriver {
    pub fn new(config: PodmanConfig) -> Self {
        Self { config }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.config.podman_bin);
        command
            .arg("--remote")
            .arg(format!("--url=unix://{}", self.config.socket.display()))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn run_raw(
        &self,
        op: &'static str,
        args: &[String],
        timeout: Option<Duration>,
    ) -> Result<Output, DriverError> {
        self.run_capped(op, args, timeout, self.config.max_output, Overflow::Fail)
            .await
    }

    async fn run_capped(
        &self,
        op: &'static str,
        args: &[String],
        timeout: Option<Duration>,
        max: usize,
        overflow: Overflow,
    ) -> Result<Output, DriverError> {
        let mut child = self
            .command(args)
            .spawn()
            .map_err(|err| DriverError::Runtime {
                op,
                message: format!("spawn podman: {err}"),
            })?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let work = async move {
            let (out, err) = tokio::join!(
                read_limited(&mut stdout, max, overflow),
                read_tail(&mut stderr)
            );
            let status = child.wait().await.map_err(|err| DriverError::Runtime {
                op,
                message: format!("wait podman: {err}"),
            })?;
            let stdout = out.map_err(|message| DriverError::Runtime { op, message })?;
            Ok::<_, DriverError>(Output {
                status: status.code(),
                stdout,
                stderr: err,
            })
        };
        match timeout {
            Some(limit) => {
                tokio::time::timeout(limit, work)
                    .await
                    .map_err(|_| DriverError::Runtime {
                        op,
                        message: "podman call timed out".into(),
                    })?
            }
            None => work.await,
        }
    }

    /// 运行并要求退出码为 0；`no such container` 映射为 `NotFound`。
    async fn run(&self, op: &'static str, args: &[String]) -> Result<Vec<u8>, DriverError> {
        let output = self
            .run_raw(op, args, Some(self.config.call_timeout))
            .await?;
        if output.status == Some(0) {
            return Ok(output.stdout);
        }
        if is_not_found(&output.stderr) {
            return Err(DriverError::NotFound);
        }
        Err(DriverError::Runtime {
            op,
            message: format!("exit {:?}: {}", output.status, output.stderr.trim()),
        })
    }

    fn layout_for(&self, spec: &ContainerSpec) -> Result<StagingLayout, DriverError> {
        let sandbox_id = spec
            .labels
            .get(LABEL_SANDBOX_ID)
            .and_then(|value| value.parse().ok())
            .ok_or(DriverError::InvariantViolated(InvariantViolation::Label(
                LABEL_SANDBOX_ID,
            )))?;
        StagingLayout::new(&self.config.workspaces_dir, sandbox_id)
            .map_err(|err| DriverError::Io(err.to_string()))
    }
}

fn is_not_found(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no such container") || lower.contains("no container with name or id")
}

/// stdout 超过上限时的处理。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overflow {
    /// 视为调用失败（JSON 输出等不该这么大）。
    Fail,
    /// 保留前 `max + 1` 字节交给调用方判定（归档导出：由 intake 记 `archive_total_exceeded`）。
    Truncate,
}

async fn read_limited<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
    overflow: Overflow,
) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let read = (&mut *reader)
        .take(max as u64 + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|err| err.to_string())?;
    if read > max {
        // 排空剩余输出，避免子进程因管道写满而阻塞。
        let _ = tokio::io::copy(reader, &mut tokio::io::sink()).await;
        if overflow == Overflow::Fail {
            return Err("podman output exceeds the limit".into());
        }
    }
    Ok(buf)
}

async fn read_tail<R: AsyncRead + Unpin>(reader: &mut R) -> String {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    let start = buf.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&buf[start..]).into_owned()
}

/// `podman container inspect --format=json` 的子集。
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectItem {
    id: String,
    state: InspectState,
    config: InspectConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectState {
    status: String,
    #[serde(default)]
    exit_code: i32,
    #[serde(default, rename = "OOMKilled")]
    oom_killed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectConfig {
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

/// `podman ps --all --format=json` 的子集。
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsItem {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    state: String,
}

fn parse_status(value: &str) -> ContainerStatus {
    match value {
        "created" | "configured" | "initialized" => ContainerStatus::Created,
        "running" | "paused" | "stopping" => ContainerStatus::Running,
        _ => ContainerStatus::Exited,
    }
}

/// 退出码 128 + n 推出信号 n（Podman 不单独报告终止信号；**需核实**）。
fn exit_facts(state: &InspectState) -> ExitFacts {
    let signal = (129..=159)
        .contains(&state.exit_code)
        .then_some(state.exit_code - 128);
    ExitFacts {
        exit_code: Some(state.exit_code),
        signal,
        oom_killed: state.oom_killed,
        finished_at: None,
    }
}

fn parse_inspect(bytes: &[u8]) -> Result<ContainerState, DriverError> {
    let items: Vec<InspectItem> =
        serde_json::from_slice(bytes).map_err(|err| DriverError::Runtime {
            op: "inspect",
            message: format!("unexpected inspect output: {err}"),
        })?;
    let item = items.into_iter().next().ok_or(DriverError::NotFound)?;
    let status = parse_status(&item.state.status);
    Ok(ContainerState {
        id: ContainerId::new(item.id),
        status,
        exit: (status == ContainerStatus::Exited).then(|| exit_facts(&item.state)),
        labels: item.config.labels.unwrap_or_default(),
    })
}

fn valid_container_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

struct ChannelStream<T>(mpsc::Receiver<T>);

impl<T> Stream for ChannelStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

#[async_trait]
impl SandboxDriver for PodmanDriver {
    fn capabilities(&self) -> DriverCaps {
        self.config.caps
    }

    async fn image_present(&self, digest: &ImageDigest) -> Result<bool, DriverError> {
        let args = vec!["image".into(), "exists".into(), digest.as_str().to_owned()];
        let output = self
            .run_raw("image_exists", &args, Some(self.config.call_timeout))
            .await?;
        match output.status {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            status => Err(DriverError::Runtime {
                op: "image_exists",
                message: format!("exit {status:?}: {}", output.stderr.trim()),
            }),
        }
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<ContainerId, DriverError> {
        let layout = self.layout_for(spec)?;
        if spec.runtime != self.config.runtime {
            return Err(DriverError::WrongState(
                "create: runtime differs from sandbox.runtime",
            ));
        }
        let mut args = render_create_args(
            spec,
            InvariantContext {
                layout: &layout,
                job_token: None,
            },
        )?;
        // 同名容器先替换（崩溃后重走 Creating 相位的幂等保证，§7.10）。
        args.insert(1, "--replace".into());
        let stdout = self.run("create", &args).await?;
        let id = String::from_utf8_lossy(&stdout).trim().to_owned();
        if !valid_container_id(&id) {
            return Err(DriverError::Runtime {
                op: "create",
                message: "podman returned an unexpected container id".into(),
            });
        }
        Ok(ContainerId::new(id))
    }

    async fn start(&self, id: &ContainerId) -> Result<(), DriverError> {
        self.run("start", &["start".into(), id.to_string()]).await?;
        Ok(())
    }

    async fn stop(&self, id: &ContainerId, grace: Duration) -> Result<(), DriverError> {
        let args = vec![
            "stop".into(),
            format!("--time={}", grace.as_secs().max(1)),
            id.to_string(),
        ];
        self.run("stop", &args).await?;
        // 返回前确认已 exited（`--init` + 私有 PID 命名空间：PID 1 退出即全部进程终止）。
        let state = self.inspect(id).await?;
        if state.status != ContainerStatus::Exited {
            return Err(DriverError::WrongState("stop: container did not exit"));
        }
        Ok(())
    }

    async fn kill(&self, id: &ContainerId) -> Result<(), DriverError> {
        let args = vec!["kill".into(), "--signal=KILL".into(), id.to_string()];
        match self.run("kill", &args).await {
            Ok(_) => Ok(()),
            // 已经停止的容器不能再 kill；以 inspect 为准。
            Err(DriverError::Runtime { .. }) => match self.inspect(id).await?.status {
                ContainerStatus::Exited => Ok(()),
                _ => Err(DriverError::WrongState("kill")),
            },
            Err(err) => Err(err),
        }
    }

    async fn wait(&self, id: &ContainerId) -> Result<ExitFacts, DriverError> {
        let output = self
            .run_raw("wait", &["wait".into(), id.to_string()], None)
            .await?;
        if output.status != Some(0) {
            if is_not_found(&output.stderr) {
                return Err(DriverError::NotFound);
            }
            return Err(DriverError::Runtime {
                op: "wait",
                message: output.stderr.trim().to_owned(),
            });
        }
        self.inspect(id)
            .await?
            .exit
            .ok_or(DriverError::WrongState("wait"))
    }

    async fn inspect(&self, id: &ContainerId) -> Result<ContainerState, DriverError> {
        let args = vec![
            "container".into(),
            "inspect".into(),
            "--format=json".into(),
            id.to_string(),
        ];
        parse_inspect(&self.run("inspect", &args).await?)
    }

    fn logs(
        &self,
        id: &ContainerId,
        since: Option<LogCursor>,
    ) -> BoxStream<'static, Result<LogChunk, DriverError>> {
        let (tx, rx) = mpsc::channel(256);
        let args = vec!["logs".into(), "--follow".into(), id.to_string()];
        let spawned = self.command(&args).spawn();
        let skip = since.map_or(0, |cursor| cursor.0);
        tokio::spawn(async move {
            let mut child = match spawned {
                Ok(child) => child,
                Err(err) => {
                    let _ = tx
                        .send(Err(DriverError::Runtime {
                            op: "logs",
                            message: err.to_string(),
                        }))
                        .await;
                    return;
                }
            };
            let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
            let stderr = BufReader::new(child.stderr.take().expect("stderr is piped"));
            let (line_tx, mut line_rx) = mpsc::channel::<(LogStream, Vec<u8>)>(256);
            for (reader, stream) in [
                (
                    Box::new(stdout) as Box<dyn tokio::io::AsyncBufRead + Send + Unpin>,
                    LogStream::Stdout,
                ),
                (Box::new(stderr), LogStream::Stderr),
            ] {
                let line_tx = line_tx.clone();
                tokio::spawn(async move {
                    let mut reader = reader;
                    loop {
                        let mut line = Vec::new();
                        match reader.read_until(b'\n', &mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if line.last() == Some(&b'\n') {
                                    line.pop();
                                }
                                if line_tx.send((stream, line)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            drop(line_tx);
            // 两条管道交错的顺序不保证跨重读稳定（**需核实**）；进度去重依赖事件的 seq。
            let mut index = 0u64;
            while let Some((stream, line)) = line_rx.recv().await {
                index += 1;
                if index <= skip {
                    continue;
                }
                let chunk = LogChunk {
                    cursor: LogCursor(index),
                    stream,
                    line,
                };
                if tx.send(Ok(chunk)).await.is_err() {
                    break;
                }
            }
            let _ = child.wait().await;
        });
        Box::pin(ChannelStream(rx))
    }

    async fn remove(&self, id: &ContainerId) -> Result<(), DriverError> {
        let args = vec![
            "rm".into(),
            "--force".into(),
            "--ignore".into(),
            id.to_string(),
        ];
        self.run("rm", &args).await?;
        Ok(())
    }

    async fn list(&self, selector: &LabelSelector) -> Result<Vec<ContainerSummary>, DriverError> {
        let mut args = vec!["ps".into(), "--all".into(), "--format=json".into()];
        for (key, value) in &selector.0 {
            args.push(format!("--filter=label={key}={value}"));
        }
        let stdout = self.run("ps", &args).await?;
        let items: Vec<PsItem> =
            serde_json::from_slice(&stdout).map_err(|err| DriverError::Runtime {
                op: "ps",
                message: format!("unexpected ps output: {err}"),
            })?;
        Ok(items
            .into_iter()
            .map(|item| ContainerSummary {
                id: ContainerId::new(item.id),
                name: item.names.into_iter().next().unwrap_or_default(),
                labels: item.labels.unwrap_or_default(),
                status: parse_status(&item.state),
            })
            // 再按标签过滤一遍：不依赖运行时 filter 的语义。
            .filter(|summary| {
                selector
                    .0
                    .iter()
                    .all(|(k, v)| summary.labels.get(k) == Some(v))
            })
            .collect())
    }

    /// 开发 / CI 布局（同一用户）：直接建目录。生产属主方案见 OQ-S2，未决。
    async fn prepare_dirs(&self, layout: &StagingLayout) -> Result<(), DriverError> {
        for dir in [layout.dir(), layout.inputs(), layout.work(), layout.out()] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|err| DriverError::Io(err.to_string()))?;
        }
        Ok(())
    }

    async fn read_out(
        &self,
        id: &ContainerId,
        layout: &StagingLayout,
        plan: &IntakePlan,
    ) -> Result<IntakeResult, DriverError> {
        match self.inspect(id).await {
            Ok(state) if state.status != ContainerStatus::Exited => {
                return Err(DriverError::StillRunning)
            }
            Ok(_) => {}
            // 容器已不在：没有进程能再写 out/；只有主机直读还能取。
            Err(DriverError::NotFound) if self.config.out_read == OutReadMode::HostDirect => {}
            Err(err) => return Err(err),
        }
        match self.config.out_read {
            OutReadMode::HostDirect => {
                #[cfg(unix)]
                {
                    let out = layout.out();
                    let plan = *plan;
                    tokio::task::spawn_blocking(move || {
                        crate::intake::host::read_out_dir(&out, &plan)
                    })
                    .await
                    .map_err(|err| DriverError::Io(err.to_string()))?
                    .map_err(|err| DriverError::Io(err.to_string()))
                }
                #[cfg(not(unix))]
                {
                    let _ = (layout, plan);
                    Err(DriverError::Io("host intake requires unix".into()))
                }
            }
            OutReadMode::Archive => {
                let args = vec!["cp".into(), format!("{id}:/gantry/out"), "-".into()];
                // 读到封顶 + 1 字节为止，超限由 intake 判定并记 archive_total_exceeded，
                // 不把沙盒写大文件变成 Infra{Collect}。
                let cap = crate::intake::tar::archive_cap(plan).saturating_add(1);
                let output = self
                    .run_capped(
                        "cp",
                        &args,
                        Some(self.config.call_timeout),
                        usize::try_from(cap).unwrap_or(usize::MAX),
                        Overflow::Truncate,
                    )
                    .await?;
                if output.status != Some(0) {
                    return Err(DriverError::Runtime {
                        op: "cp",
                        message: output.stderr.trim().to_owned(),
                    });
                }
                Ok(crate::intake::tar::read_out_tar(
                    std::io::Cursor::new(output.stdout),
                    plan,
                    Some("out"),
                ))
            }
        }
    }

    async fn purge_staging(&self, layout: &StagingLayout) -> Result<(), DriverError> {
        let dir = layout.dir();
        let _ = tokio::task::spawn_blocking(move || crate::stage::unseal(&dir)).await;
        match tokio::fs::remove_dir_all(layout.dir()).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(DriverError::Io(err.to_string())),
        }
    }
}
