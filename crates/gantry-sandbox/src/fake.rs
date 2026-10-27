//! `FakeDriver`：内存模拟容器，只用于本模块测试（sandbox.md §6.2、§12）。
//!
//! 可脚本化每个沙盒写入 `out/` 的文件、stdout / stderr 行（带相对启动时刻的偏移）、
//! 退出码 / 信号 / OOM，配合 `tokio::time::pause()` 精确控制计时器。作业目录是真实
//! 的宿主目录，`read_out` 走与 Podman 驱动相同的 intake 规则；所有对作业目录的读取
//! 都记录在案，供"从不读 work/ home/ tmp/"一类断言使用。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_core::Stream;
use gantry_core::ids::SandboxId;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::driver::{
    ContainerState, ContainerStatus, ContainerSummary, DriverCaps, DriverError, ExitFacts,
    LabelSelector, LogChunk, LogCursor, LogStream, SandboxDriver,
};
use crate::intake::{IntakePlan, IntakeResult};
use crate::invariants::{validate_invariants, InvariantContext};
use crate::spec::{
    ContainerId, ContainerSpec, ImageDigest, Runtime, StagingLayout, LABEL_SANDBOX_ID,
};

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

/// 容器"进程"的行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeBehavior {
    /// 启动 `after` 后以 `code` 退出。收到 SIGTERM 时按 `honor_term` 处理。
    ExitAfter {
        after: Duration,
        code: i32,
        honor_term: bool,
    },
    /// 启动 `after` 后被 cgroup OOM 杀掉（SIGKILL + `oom_killed`）。
    OomAfter { after: Duration },
    /// 一直运行；`honor_term = false` 时忽略 SIGTERM，只能被 SIGKILL 终止。
    Forever { honor_term: bool },
}

/// `out/` 里的条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeOutEntry {
    File(Vec<u8>),
    Symlink(PathBuf),
    Fifo,
    Dir,
}

/// 一个沙盒的脚本（按 `sandbox_id` 登记，`create` 时取用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeScript {
    pub behavior: FakeBehavior,
    /// `(相对 start 的偏移, 流, 行)`；容器退出前到点的行才会出现在日志里。
    pub log: Vec<(Duration, LogStream, Vec<u8>)>,
    /// 容器退出时写进 `out/`（模拟 worker 在停止前写完）。
    pub out: Vec<(String, FakeOutEntry)>,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            behavior: FakeBehavior::ExitAfter {
                after: Duration::from_secs(1),
                code: 0,
                honor_term: true,
            },
            log: Vec::new(),
            out: Vec::new(),
        }
    }
}

/// 调用记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverCall {
    ImagePresent(ImageDigest),
    Create { name: String },
    Start(ContainerId),
    Stop { id: ContainerId, grace: Duration },
    Kill(ContainerId),
    Wait(ContainerId),
    Inspect(ContainerId),
    Logs(ContainerId, Option<LogCursor>),
    Remove(ContainerId),
    List,
    PrepareDirs(SandboxId),
    ReadOut(ContainerId),
    PurgeStaging(SandboxId),
}

/// 发给容器的信号，带相对启动时刻的偏移。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalRecord {
    pub at: Duration,
    pub signal: i32,
}

/// 可注入的一次性失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FailPoint {
    Create,
    Start,
    PrepareDirs,
    ReadOut,
    Remove,
}

struct FakeContainer {
    spec: ContainerSpec,
    layout: StagingLayout,
    script: FakeScript,
    status: ContainerStatus,
    started_at: Option<Instant>,
    exit: Option<ExitFacts>,
    signals: Vec<SignalRecord>,
    status_tx: watch::Sender<ContainerStatus>,
}

#[derive(Default)]
struct State {
    containers: BTreeMap<ContainerId, FakeContainer>,
    scripts: BTreeMap<SandboxId, FakeScript>,
    missing_images: BTreeSet<ImageDigest>,
    failures: BTreeSet<FailPoint>,
    calls: Vec<DriverCall>,
    reads: Vec<PathBuf>,
    next_id: u64,
}

/// 内存容器驱动。克隆共享同一份状态。
#[derive(Clone)]
pub struct FakeDriver {
    workspaces_dir: PathBuf,
    caps: DriverCaps,
    state: Arc<Mutex<State>>,
}

impl FakeDriver {
    pub fn new(workspaces_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspaces_dir: workspaces_dir.into(),
            caps: DriverCaps {
                runtime: Runtime::Runc,
                oom_reporting: true,
                disk_quota: false,
                supports_pool: false,
                memory_limit: true,
                pids_limit: true,
                cpu_limit: true,
            },
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn script(&self, sandbox_id: SandboxId, script: FakeScript) {
        self.lock().scripts.insert(sandbox_id, script);
    }

    pub fn set_image_missing(&self, image: ImageDigest) {
        self.lock().missing_images.insert(image);
    }

    /// 下一次对应调用失败（一次性）。
    pub fn fail_next(&self, point: FailPoint) {
        self.lock().failures.insert(point);
    }

    pub fn calls(&self) -> Vec<DriverCall> {
        self.lock().calls.clone()
    }

    /// 驱动读过的作业目录内路径。
    pub fn reads(&self) -> Vec<PathBuf> {
        self.lock().reads.clone()
    }

    pub fn signals(&self, id: &ContainerId) -> Vec<SignalRecord> {
        self.lock()
            .containers
            .get(id)
            .map(|c| c.signals.clone())
            .unwrap_or_default()
    }

    pub fn container_count(&self) -> usize {
        self.lock().containers.len()
    }

    /// 模拟容器被外部删除（运行中被杀、宿主重启丢失等）。
    pub fn vanish(&self, id: &ContainerId) {
        let mut state = self.lock();
        if let Some(container) = state.containers.remove(id) {
            container.status_tx.send_replace(ContainerStatus::Exited);
        }
    }

    fn take_failure(&self, point: FailPoint, op: &'static str) -> Result<(), DriverError> {
        if self.lock().failures.remove(&point) {
            return Err(DriverError::Runtime {
                op,
                message: "injected failure".into(),
            });
        }
        Ok(())
    }

    fn layout_for(&self, spec: &ContainerSpec) -> Result<StagingLayout, DriverError> {
        let sandbox_id = spec
            .labels
            .get(LABEL_SANDBOX_ID)
            .and_then(|value| value.parse::<SandboxId>().ok())
            .ok_or(DriverError::InvariantViolated(
                crate::invariants::InvariantViolation::Label(LABEL_SANDBOX_ID),
            ))?;
        StagingLayout::new(&self.workspaces_dir, sandbox_id)
            .map_err(|err| DriverError::Io(err.to_string()))
    }

    /// 把容器置为已退出：写 `out/`，记退出事实，唤醒等待者。只在仍运行时生效。
    fn finish(state: &mut State, id: &ContainerId, exit: ExitFacts) {
        let Some(container) = state.containers.get_mut(id) else {
            return;
        };
        if container.status != ContainerStatus::Running {
            return;
        }
        container.status = ContainerStatus::Exited;
        container.exit = Some(exit);
        materialize_out(&container.layout.out(), &container.script.out);
        container.status_tx.send_replace(ContainerStatus::Exited);
    }

    fn deliver(&self, id: &ContainerId, signal: i32) {
        let mut state = self.lock();
        let Some(container) = state.containers.get_mut(id) else {
            return;
        };
        if container.status != ContainerStatus::Running {
            return;
        }
        let at = container
            .started_at
            .map(|start| Instant::now().duration_since(start))
            .unwrap_or_default();
        container.signals.push(SignalRecord { at, signal });
        let honor_term = match container.script.behavior {
            FakeBehavior::ExitAfter { honor_term, .. } | FakeBehavior::Forever { honor_term } => {
                honor_term
            }
            FakeBehavior::OomAfter { .. } => true,
        };
        if signal == SIGKILL || honor_term {
            Self::finish(
                &mut state,
                id,
                ExitFacts {
                    exit_code: Some(128 + signal),
                    signal: Some(signal),
                    oom_killed: false,
                    finished_at: None,
                },
            );
        }
    }

    fn status_rx(&self, id: &ContainerId) -> Result<watch::Receiver<ContainerStatus>, DriverError> {
        self.lock()
            .containers
            .get(id)
            .map(|c| c.status_tx.subscribe())
            .ok_or(DriverError::NotFound)
    }
}

fn materialize_out(out: &Path, entries: &[(String, FakeOutEntry)]) {
    if !out.is_dir() {
        return;
    }
    for (name, entry) in entries {
        let path = out.join(name);
        let _ = match entry {
            FakeOutEntry::File(bytes) => std::fs::write(&path, bytes),
            FakeOutEntry::Dir => std::fs::create_dir_all(&path),
            #[cfg(unix)]
            FakeOutEntry::Symlink(target) => std::os::unix::fs::symlink(target, &path),
            #[cfg(unix)]
            FakeOutEntry::Fifo => mkfifo(&path),
            #[cfg(not(unix))]
            _ => Ok(()),
        };
    }
}

#[cfg(unix)]
fn mkfifo(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("NUL in path"))?;
    // SAFETY: c 是有效的 NUL 结尾路径。
    if unsafe { libc::mkfifo(c.as_ptr(), 0o644) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

struct ChannelStream<T>(mpsc::UnboundedReceiver<T>);

impl<T> Stream for ChannelStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

#[async_trait]
impl SandboxDriver for FakeDriver {
    fn capabilities(&self) -> DriverCaps {
        self.caps
    }

    async fn image_present(&self, digest: &ImageDigest) -> Result<bool, DriverError> {
        let mut state = self.lock();
        state.calls.push(DriverCall::ImagePresent(digest.clone()));
        Ok(!state.missing_images.contains(digest))
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<ContainerId, DriverError> {
        self.lock().calls.push(DriverCall::Create {
            name: spec.name.clone(),
        });
        let layout = self.layout_for(spec)?;
        validate_invariants(
            spec,
            InvariantContext {
                layout: &layout,
                job_token: None,
            },
        )?;
        self.take_failure(FailPoint::Create, "create")?;
        let mut state = self.lock();
        if state.missing_images.contains(&spec.image) {
            return Err(DriverError::ImageMissing(spec.image.to_string()));
        }
        // 同名容器先删除再建（幂等重建）。
        let existing: Vec<ContainerId> = state
            .containers
            .iter()
            .filter(|(_, c)| c.spec.name == spec.name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in existing {
            if let Some(old) = state.containers.remove(&id) {
                old.status_tx.send_replace(ContainerStatus::Exited);
            }
        }
        state.next_id += 1;
        let id = ContainerId::new(format!("fake{:060}", state.next_id));
        let sandbox_id = layout.sandbox_id();
        let script = state.scripts.get(&sandbox_id).cloned().unwrap_or_default();
        let (status_tx, _) = watch::channel(ContainerStatus::Created);
        state.containers.insert(
            id.clone(),
            FakeContainer {
                spec: spec.clone(),
                layout,
                script,
                status: ContainerStatus::Created,
                started_at: None,
                exit: None,
                signals: Vec::new(),
                status_tx,
            },
        );
        Ok(id)
    }

    async fn start(&self, id: &ContainerId) -> Result<(), DriverError> {
        self.lock().calls.push(DriverCall::Start(id.clone()));
        self.take_failure(FailPoint::Start, "start")?;
        let behavior = {
            let mut state = self.lock();
            let container = state.containers.get_mut(id).ok_or(DriverError::NotFound)?;
            match container.status {
                ContainerStatus::Created => {}
                ContainerStatus::Running => return Ok(()),
                ContainerStatus::Exited => return Err(DriverError::WrongState("start")),
            }
            container.status = ContainerStatus::Running;
            container.started_at = Some(Instant::now());
            container.status_tx.send_replace(ContainerStatus::Running);
            container.script.behavior
        };
        let (after, exit) = match behavior {
            FakeBehavior::ExitAfter { after, code, .. } => (
                after,
                ExitFacts {
                    exit_code: Some(code),
                    signal: None,
                    oom_killed: false,
                    finished_at: None,
                },
            ),
            FakeBehavior::OomAfter { after } => (
                after,
                ExitFacts {
                    exit_code: Some(128 + SIGKILL),
                    signal: Some(SIGKILL),
                    oom_killed: true,
                    finished_at: None,
                },
            ),
            FakeBehavior::Forever { .. } => return Ok(()),
        };
        let state = Arc::clone(&self.state);
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            FakeDriver::finish(&mut state, &id, exit);
        });
        Ok(())
    }

    async fn stop(&self, id: &ContainerId, grace: Duration) -> Result<(), DriverError> {
        self.lock().calls.push(DriverCall::Stop {
            id: id.clone(),
            grace,
        });
        let mut rx = self.status_rx(id)?;
        if *rx.borrow() == ContainerStatus::Created {
            // 未启动的容器直接视为已停止。
            let mut state = self.lock();
            if let Some(container) = state.containers.get_mut(id) {
                container.status = ContainerStatus::Exited;
                container.exit = Some(ExitFacts {
                    exit_code: None,
                    signal: None,
                    oom_killed: false,
                    finished_at: None,
                });
                container.status_tx.send_replace(ContainerStatus::Exited);
            }
            return Ok(());
        }
        self.deliver(id, SIGTERM);
        let exited =
            tokio::time::timeout(grace, rx.wait_for(|s| *s == ContainerStatus::Exited)).await;
        if exited.is_err() {
            self.deliver(id, SIGKILL);
        }
        Ok(())
    }

    async fn kill(&self, id: &ContainerId) -> Result<(), DriverError> {
        self.lock().calls.push(DriverCall::Kill(id.clone()));
        if !self.lock().containers.contains_key(id) {
            return Err(DriverError::NotFound);
        }
        self.deliver(id, SIGKILL);
        Ok(())
    }

    async fn wait(&self, id: &ContainerId) -> Result<ExitFacts, DriverError> {
        self.lock().calls.push(DriverCall::Wait(id.clone()));
        let mut rx = self.status_rx(id)?;
        let _ = rx.wait_for(|s| *s == ContainerStatus::Exited).await;
        self.lock()
            .containers
            .get(id)
            .and_then(|c| c.exit)
            .ok_or(DriverError::NotFound)
    }

    async fn inspect(&self, id: &ContainerId) -> Result<ContainerState, DriverError> {
        let mut state = self.lock();
        state.calls.push(DriverCall::Inspect(id.clone()));
        let container = state.containers.get(id).ok_or(DriverError::NotFound)?;
        Ok(ContainerState {
            id: id.clone(),
            status: container.status,
            exit: container.exit,
            labels: container.spec.labels.clone(),
        })
    }

    fn logs(
        &self,
        id: &ContainerId,
        since: Option<LogCursor>,
    ) -> BoxStream<'static, Result<LogChunk, DriverError>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = self.lock();
        state.calls.push(DriverCall::Logs(id.clone(), since));
        let Some(container) = state.containers.get(id) else {
            let _ = tx.send(Err(DriverError::NotFound));
            return Box::pin(ChannelStream(rx));
        };
        let mut lines = container.script.log.clone();
        lines.sort_by_key(|(at, _, _)| *at);
        let started_at = container.started_at;
        let mut status = container.status_tx.subscribe();
        drop(state);
        let skip = since.map_or(0, |c| c.0 as usize);
        tokio::spawn(async move {
            // 等容器启动。
            let started_at = match started_at {
                Some(at) => at,
                None => {
                    if status
                        .wait_for(|s| *s != ContainerStatus::Created)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    Instant::now()
                }
            };
            for (index, (offset, stream, line)) in lines.into_iter().enumerate() {
                let due = started_at + offset;
                let exited = *status.borrow() == ContainerStatus::Exited;
                if !exited {
                    tokio::select! {
                        _ = tokio::time::sleep_until(due) => {}
                        _ = status.wait_for(|s| *s == ContainerStatus::Exited) => {}
                    }
                }
                // 容器在这一行到点之前就退出了：这一行从未写出。
                if Instant::now() < due {
                    break;
                }
                if index < skip {
                    continue;
                }
                let chunk = LogChunk {
                    cursor: LogCursor(index as u64 + 1),
                    stream,
                    line,
                };
                if tx.send(Ok(chunk)).is_err() {
                    return;
                }
            }
        });
        Box::pin(ChannelStream(rx))
    }

    async fn remove(&self, id: &ContainerId) -> Result<(), DriverError> {
        self.lock().calls.push(DriverCall::Remove(id.clone()));
        self.take_failure(FailPoint::Remove, "remove")?;
        if let Some(container) = self.lock().containers.remove(id) {
            container.status_tx.send_replace(ContainerStatus::Exited);
        }
        Ok(())
    }

    async fn list(&self, selector: &LabelSelector) -> Result<Vec<ContainerSummary>, DriverError> {
        let mut state = self.lock();
        state.calls.push(DriverCall::List);
        Ok(state
            .containers
            .iter()
            .filter(|(_, c)| {
                selector
                    .0
                    .iter()
                    .all(|(k, v)| c.spec.labels.get(k) == Some(v))
            })
            .map(|(id, c)| ContainerSummary {
                id: id.clone(),
                name: c.spec.name.clone(),
                labels: c.spec.labels.clone(),
                status: c.status,
            })
            .collect())
    }

    async fn prepare_dirs(&self, layout: &StagingLayout) -> Result<(), DriverError> {
        self.lock()
            .calls
            .push(DriverCall::PrepareDirs(layout.sandbox_id()));
        self.take_failure(FailPoint::PrepareDirs, "prepare_dirs")?;
        for dir in [layout.dir(), layout.inputs(), layout.work(), layout.out()] {
            std::fs::create_dir_all(&dir).map_err(|err| DriverError::Io(err.to_string()))?;
        }
        Ok(())
    }

    async fn read_out(
        &self,
        id: &ContainerId,
        layout: &StagingLayout,
        plan: &IntakePlan,
    ) -> Result<IntakeResult, DriverError> {
        {
            let mut state = self.lock();
            state.calls.push(DriverCall::ReadOut(id.clone()));
            // 容器已不在（被删除）时允许读：没有进程能再写 out/。
            if let Some(container) = state.containers.get(id) {
                if container.status == ContainerStatus::Running {
                    return Err(DriverError::StillRunning);
                }
            }
            state.reads.push(layout.out());
        }
        self.take_failure(FailPoint::ReadOut, "read_out")?;
        #[cfg(unix)]
        {
            crate::intake::host::read_out_dir(&layout.out(), plan)
                .map_err(|err| DriverError::Io(err.to_string()))
        }
        #[cfg(not(unix))]
        {
            let _ = plan;
            Err(DriverError::Io("host intake requires unix".into()))
        }
    }

    async fn purge_staging(&self, layout: &StagingLayout) -> Result<(), DriverError> {
        self.lock()
            .calls
            .push(DriverCall::PurgeStaging(layout.sandbox_id()));
        match std::fs::remove_dir_all(layout.dir()) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(DriverError::Io(err.to_string())),
        }
    }
}
