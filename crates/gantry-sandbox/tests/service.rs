//! sandbox.md §12.A 第 5 条、§12.D、§12.E 末条、§12.F「先停后取」、§12.G、§12.H：
//! `SandboxManager` 跑在 `FakeDriver`、`FakeRepo`、`EgressStub`、`Store::open_in_memory` 之上。
//! 崩溃用 `SandboxManager::crash` 模拟（中止全部任务、不清理），再在同一根目录、驱动、
//! store 与 Egress 上新建服务并 `recover`。

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::TempRoot;
use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, SandboxLimits, SandboxTrust, TokenWeights};
use gantry_core::ids::{AttemptId, Clock, Id, IdGen, SandboxId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::{
    AppliedConfig, CallClosed, CallStatement, CallUsage, EgressConfigView, EgressControl,
    EgressEndpoints, EgressError, EgressStatus, FinalUsage, IssueJobToken, JobTokenGrant, JobUsage,
    StageSecret, StagedInfo, StagedSecret, SvcTokenGrant, UsageCursor,
};
use gantry_core::ports::models::CallId;
use gantry_core::ports::repo::CommitInfo;
use gantry_core::ports::sandbox::{
    EgressRequest, ExecSpec, GroupName, InfraDetail, JobOutcome, ProfileName, RecoveredJob,
    SandboxError, SandboxService, SandboxSpec, TerminalCause, TimerKind, Timers, TokenEnvSpec,
    UnfinishedAttempt, WorkspaceMode, WorkspaceRequest,
};
use gantry_core::secret::SecretName;
use gantry_core::settings::resource::{
    ArtifactIo, MaterialIo, ProgressIo, SandboxDriver, SandboxIo,
};
use gantry_core::testing::SeqEntropy;
use gantry_core::time::Timestamp;
use gantry_core::worker::JobSpec;
use gantry_fakes::egress::EgressStub;
use gantry_fakes::repo::FakeRepo;
use gantry_fakes::sandbox::FakeSandboxService;
use gantry_sandbox::driver::LogStream;
use gantry_sandbox::fake::{
    DriverCall, FailPoint, FakeBehavior, FakeDriver, FakeOutEntry, FakeScript, SIGKILL, SIGTERM,
};
use gantry_sandbox::service::seams::{MemoryArtifacts, StaticSchemas};
use gantry_sandbox::service::{SandboxConfig, SandboxDeps, SandboxManager};
use gantry_sandbox::spec::{NetworkAttach, Runtime};
use gantry_store::{Store, StoreDeps};

const JOB: &str = include_str!("../../gantry-core/fixtures/protocol/worker/job_spec.json");
const SCHEMA: &[u8] = br#"{"type":"object"}"#;
const MS: i64 = 1_700_000_000_000;

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "engine"),
        device: None,
        role: None,
        client_profile: None,
        request_id: gantry_core::ids::Ulid::from_parts(MS as u64, 7).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn base_job() -> JobSpec {
    let mut job: JobSpec = serde_json::from_str(JOB).unwrap();
    job.output.structured_schema.sha256 = sha256(SCHEMA);
    job.materials.clear();
    job.upstream.clear();
    job
}

fn sbx(n: u128) -> SandboxId {
    Id::from_parts(MS as u64, 100 + n).unwrap()
}

fn att(n: u128) -> AttemptId {
    Id::from_parts(MS as u64, 200 + n).unwrap()
}

fn spec(n: u128) -> SandboxSpec {
    let mut job = base_job();
    job.job_id = att(n);
    let mut model_base_url = BTreeMap::new();
    model_base_url.insert("anthropic".to_owned(), "ANTHROPIC_BASE_URL".to_owned());
    let mut env = BTreeMap::new();
    env.insert("HOME".to_owned(), "/gantry/home".to_owned());
    env.insert(
        "ANTHROPIC_API_KEY".to_owned(),
        "gantry-egress-managed".to_owned(),
    );
    SandboxSpec {
        job_id: att(n),
        sandbox_id: sbx(n),
        project_id: job.project_id,
        delivery_id: Some(job.delivery_id),
        conversation_id: None,
        workspace: WorkspaceRequest {
            repo: RepoSlug::parse("acme/widgets").unwrap(),
            base_sha: job.workspace.base_sha,
            pr_base_sha: None,
            mode: WorkspaceMode::Rw,
        },
        job,
        raw_materials: Vec::new(),
        upstream: Vec::new(),
        exec: ExecSpec {
            image_digest: sha256(b"worker-image"),
            argv: vec!["gantry-worker".into(), "run".into()],
            env,
            token_env: TokenEnvSpec {
                model_base_url,
                proxy: vec!["HTTPS_PROXY".into()],
            },
        },
        driver: SandboxDriver::Podman,
        trust: SandboxTrust::Untrusted,
        limits: SandboxLimits {
            cpu_cores: 1.0,
            mem_mb: 512,
            disk_mb: 512,
            pids: 64,
            wall_time_s: 600,
        },
        timers: Timers {
            idle_timeout_s: 120,
            kill_grace_s: 10,
        },
        io: SandboxIo {
            progress: ProgressIo {
                max_line_bytes: 8192,
                max_lines: 20_000,
                max_rate_per_s: 20,
            },
            artifacts: ArtifactIo {
                result_max_bytes: 1 << 20,
                report_max_bytes: 1 << 18,
                proposals_max_bytes: 1 << 18,
                proposals_max_count: 20,
            },
            materials: MaterialIo {
                max_item_bytes: 64 * 1024,
                max_total_bytes: 512 * 1024,
            },
            logs_tail_bytes: 1 << 20,
        },
        egress: EgressRequest {
            profile: ProfileName::new("default"),
            groups: vec![GroupName::new("model_api")],
            tokens_total: 100_000,
            weights: TokenWeights::default(),
        },
        config_rev: gantry_core::entities::Seq::from_wire(DECIDED_REV).unwrap(),
    }
}

/// 引擎组装 spec 时的配置修订；故意与 Egress 已应用的修订不同。
const DECIDED_REV: u64 = 7;

/// 包一层 EgressStub：记录签发 / 吊销的顺序，可让签发失败。
struct RecordingEgress {
    inner: EgressStub,
    log: std::sync::Mutex<Vec<String>>,
    fail_issue: std::sync::atomic::AtomicBool,
    fail_revoke: std::sync::atomic::AtomicBool,
    issued_revs: std::sync::Mutex<Vec<u64>>,
}

impl RecordingEgress {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl EgressControl for RecordingEgress {
    async fn endpoints(&self) -> Result<EgressEndpoints, EgressError> {
        self.inner.endpoints().await
    }
    async fn issue_job_token(
        &self,
        req: IssueJobToken,
        ctx: &ActorCtx,
    ) -> Result<JobTokenGrant, EgressError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("issue {}", req.job_id));
        self.issued_revs
            .lock()
            .unwrap()
            .push(req.config_rev.as_u64());
        if self.fail_issue.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(EgressError::Unavailable {
                message: "egress health != ok".into(),
            });
        }
        self.inner.issue_job_token(req, ctx).await
    }
    async fn job_usage(&self, job_id: &AttemptId) -> Result<JobUsage, EgressError> {
        self.inner.job_usage(job_id).await
    }
    async fn revoke_job_token(
        &self,
        job_id: &AttemptId,
        ctx: &ActorCtx,
    ) -> Result<FinalUsage, EgressError> {
        self.log.lock().unwrap().push(format!("revoke {job_id}"));
        if self.fail_revoke.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(EgressError::Unavailable {
                message: "egress restarting".into(),
            });
        }
        self.inner.revoke_job_token(job_id, ctx).await
    }
    async fn forget_job(&self, job_id: &AttemptId, ctx: &ActorCtx) -> Result<(), EgressError> {
        self.log.lock().unwrap().push(format!("forget {job_id}"));
        self.inner.forget_job(job_id, ctx).await
    }
    async fn service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        self.inner.service_token(ctx).await
    }
    async fn rotate_service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        self.inner.rotate_service_token(ctx).await
    }
    async fn apply_config(
        &self,
        view: EgressConfigView,
        ctx: &ActorCtx,
    ) -> Result<AppliedConfig, EgressError> {
        self.inner.apply_config(view, ctx).await
    }
    async fn secret_present(
        &self,
        refs: &[SecretName],
    ) -> Result<BTreeMap<SecretName, bool>, EgressError> {
        self.inner.secret_present(refs).await
    }
    async fn stage_secret(
        &self,
        req: StageSecret,
        ctx: &ActorCtx,
    ) -> Result<StagedSecret, EgressError> {
        self.inner.stage_secret(req, ctx).await
    }
    async fn activate_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<gantry_core::ports::secrets::SecretVersion, EgressError> {
        self.inner.activate_secret(staged, ctx).await
    }
    async fn discard_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<(), EgressError> {
        self.inner.discard_secret(staged, ctx).await
    }
    async fn staged_secrets(&self) -> Result<Vec<StagedInfo>, EgressError> {
        self.inner.staged_secrets().await
    }
    async fn call_usage(&self, call_id: &CallId) -> Result<CallUsage, EgressError> {
        self.inner.call_usage(call_id).await
    }
    async fn usage_since(
        &self,
        cursor: UsageCursor,
        limit: u32,
    ) -> Result<gantry_core::egress::UsagePage, EgressError> {
        self.inner.usage_since(cursor, limit).await
    }
    async fn svc_token_usage(
        &self,
        id: &gantry_core::egress::SvcTokenId,
        cursor: Option<UsageCursor>,
    ) -> Result<gantry_core::egress::UsagePage, EgressError> {
        self.inner.svc_token_usage(id, cursor).await
    }
    async fn close_call(
        &self,
        id: &gantry_core::egress::SvcTokenId,
        call_id: &CallId,
        ctx: &ActorCtx,
    ) -> Result<CallClosed, EgressError> {
        self.inner.close_call(id, call_id, ctx).await
    }
    async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError> {
        self.inner.call_terminal(call_id).await
    }
    async fn status(&self) -> Result<EgressStatus, EgressError> {
        self.inner.status().await
    }
}

/// 与 tokio（可暂停的）时间一致的服务端时钟：deadline 按 UTC 持久化、重启后按它重建，
/// 测试里两者必须走同一条时间线。
struct TokioClock {
    base: tokio::time::Instant,
}

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(MS + self.base.elapsed().as_millis() as i64)
    }
}

struct Harness {
    root: TempRoot,
    driver: Arc<FakeDriver>,
    egress: Arc<RecordingEgress>,
    store: Store,
    clock: Arc<TokioClock>,
    max_concurrent: usize,
    orphan_grace: Duration,
    service: SandboxManager<FakeDriver>,
}

fn harness(max_concurrent: usize) -> Harness {
    let root = TempRoot::new();
    let driver = Arc::new(FakeDriver::new(root.path()));
    let clock = Arc::new(TokioClock {
        base: tokio::time::Instant::now(),
    });
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(9))));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids,
    })
    .unwrap();
    let egress = Arc::new(RecordingEgress {
        inner: EgressStub::with_default_config(),
        log: Default::default(),
        fail_issue: Default::default(),
        fail_revoke: Default::default(),
        issued_revs: Default::default(),
    });
    let orphan_grace = Duration::ZERO;
    let service = manager(
        &root,
        &driver,
        &egress,
        &store,
        &clock,
        max_concurrent,
        orphan_grace,
    );
    Harness {
        root,
        driver,
        egress,
        store,
        clock,
        max_concurrent,
        orphan_grace,
        service,
    }
}

fn manager(
    root: &TempRoot,
    driver: &Arc<FakeDriver>,
    egress: &Arc<RecordingEgress>,
    store: &Store,
    clock: &Arc<TokioClock>,
    max_concurrent: usize,
    orphan_grace: Duration,
) -> SandboxManager<FakeDriver> {
    let repo = FakeRepo::new(gantry_core::time::Timestamp::from_millis(MS));
    let job = base_job();
    repo.insert_commit(
        job.project_id,
        job.workspace.base_sha,
        CommitInfo {
            parents: Vec::new(),
            tree: GitOid::parse("1111111111111111111111111111111111111111").unwrap(),
            trailers: Vec::new(),
        },
    );
    let mut schemas = BTreeMap::new();
    schemas.insert(job.output.structured_schema.id.clone(), SCHEMA.to_vec());
    SandboxManager::new(
        SandboxDeps {
            driver: Arc::clone(driver),
            repo: Arc::new(repo),
            egress: egress.clone(),
            store: store.clone(),
            clock: clock.clone(),
            artifacts: Arc::new(MemoryArtifacts::default()),
            schemas: Arc::new(StaticSchemas(schemas)),
        },
        SandboxConfig {
            workspaces_dir: root.path().to_path_buf(),
            runtime: Runtime::Runc,
            max_concurrent_sandboxes: max_concurrent,
            network: NetworkAttach {
                network: "gantry-sbx".into(),
                add_hosts: Vec::new(),
            },
            log_max_bytes: 16 << 20,
            out_quota_bytes: 12 << 20,
            tmpfs_home_mb: 256,
            tmpfs_tmp_mb: 512,
            progress_sample: Duration::from_secs(1),
            log_drain_timeout: Duration::from_secs(5),
            orphan_grace,
        },
    )
}

impl Harness {
    /// `gantryd` 崩溃并重启：旧服务的任务全部中止，新服务接手同一份宿主状态。
    fn restart(&mut self) {
        self.service.crash();
        self.service = manager(
            &self.root,
            &self.driver,
            &self.egress,
            &self.store,
            &self.clock,
            self.max_concurrent,
            self.orphan_grace,
        );
    }
}

fn golden_script() -> FakeScript {
    FakeScript {
        behavior: FakeBehavior::ExitAfter {
            after: Duration::from_secs(30),
            code: 0,
            honor_term: true,
        },
        log: vec![
            (
                Duration::from_secs(1),
                LogStream::Stdout,
                br#"{"v":1,"seq":1,"type":"stage","stage":"explore","detail":"reading"}"#.to_vec(),
            ),
            (
                Duration::from_secs(2),
                LogStream::Stderr,
                b"warning: x".to_vec(),
            ),
            (
                Duration::from_secs(3),
                LogStream::Stdout,
                br#"{"v":1,"seq":2,"type":"finish","status":"succeeded"}"#.to_vec(),
            ),
        ],
        out: vec![
            (
                "result.json".into(),
                FakeOutEntry::File(br#"{"status":"succeeded"}"#.to_vec()),
            ),
            (
                "changes.patch".into(),
                FakeOutEntry::File(b"diff --git a/x b/x\n".to_vec()),
            ),
            ("report.md".into(), FakeOutEntry::File(b"# done\n".to_vec())),
            ("proposals.json".into(), FakeOutEntry::File(b"[]".to_vec())),
        ],
    }
}

fn container_id(h: &Harness) -> gantry_sandbox::spec::ContainerId {
    h.driver
        .calls()
        .iter()
        .find_map(|call| match call {
            DriverCall::Start(id) => Some(id.clone()),
            _ => None,
        })
        .expect("container was started")
}

async fn event_types(store: &Store) -> Vec<String> {
    let page = store
        .events()
        .scan(gantry_core::entities::Seq::ZERO, &Default::default(), 200)
        .await
        .unwrap();
    page.events
        .iter()
        .map(|e| e.ty.as_str().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn golden_path() {
    let h = harness(2);
    h.driver.script(sbx(1), golden_script());
    let handle = h.service.submit(&ctx(), spec(1)).await.unwrap();
    assert_eq!((handle.job_id, handle.sandbox_id), (att(1), sbx(1)));
    let outcome = h.service.wait(&att(1)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    assert_eq!(outcome.terminal.exit_code, Some(0));
    assert!(!outcome.partial);
    assert_eq!(outcome.files.len(), 4);
    assert!(outcome.unexpected.is_empty());
    assert!(outcome.progress.is_some());
    assert_eq!(outcome.progress_stats.samples, 2);
    assert_eq!(outcome.usage_metered, Default::default());
    assert!(!outcome.egress.incomplete);
    assert!(outcome.timings.started.is_some() && outcome.timings.collected.is_some());

    // 事件顺序：sandbox.created → attempt.progress… → sandbox.destroyed。
    let types = event_types(&h.store).await;
    assert_eq!(types.first().map(String::as_str), Some("sandbox.created"));
    assert_eq!(types.last().map(String::as_str), Some("sandbox.destroyed"));
    assert!(types.iter().any(|t| t == "attempt.progress"));

    // 令牌：签发在 create 之前、吊销在 collect 之后（各一次）。
    assert_eq!(
        h.egress.log(),
        vec![format!("issue {}", att(1)), format!("revoke {}", att(1))]
    );
    let calls = h.driver.calls();
    let create = calls
        .iter()
        .position(|c| matches!(c, DriverCall::Create { .. }))
        .unwrap();
    let read_out = calls
        .iter()
        .position(|c| matches!(c, DriverCall::ReadOut(_)))
        .unwrap();
    let remove = calls
        .iter()
        .position(|c| matches!(c, DriverCall::Remove(_)))
        .unwrap();
    assert!(create < read_out && read_out < remove);
    // 先停后取 + 只读 out/。
    assert_eq!(
        h.driver.reads(),
        vec![common::layout(h.root.path(), 0)
            .workspaces_dir()
            .join(sbx(1).to_string())
            .join("out")]
    );

    // wait 对同一 job 恒返回同一结果；release 幂等。
    assert_eq!(h.service.wait(&att(1)).await.unwrap(), outcome);
    h.service.release(&ctx(), &att(1)).await.unwrap();
    h.service.release(&ctx(), &att(1)).await.unwrap();
    assert!(h.egress.log().contains(&format!("forget {}", att(1))));
    assert!(!h.root.path().join(sbx(1).to_string()).exists());
}

#[tokio::test(start_paused = true)]
async fn wall_timer_sigterm_then_sigkill_after_grace() {
    let h = harness(1);
    h.driver.script(
        sbx(2),
        FakeScript {
            behavior: FakeBehavior::Forever { honor_term: false },
            log: (1..=1000u64)
                .map(|i| {
                    (
                        Duration::from_secs(i * 5),
                        LogStream::Stdout,
                        format!(r#"{{"v":1,"seq":{i},"type":"heartbeat","in_flight":1}}"#)
                            .into_bytes(),
                    )
                })
                .collect(),
            out: vec![("result.json".into(), FakeOutEntry::File(b"{}".to_vec()))],
        },
    );
    let mut s = spec(2);
    s.limits.wall_time_s = 60;
    h.service.submit(&ctx(), s).await.unwrap();
    let outcome = h.service.wait(&att(2)).await.unwrap();
    // 只被伪造心跳续命的 worker 在 wall_time 到点仍被杀。
    assert_eq!(outcome.terminal.cause, TerminalCause::KilledTimer);
    assert_eq!(outcome.terminal.timer, Some(TimerKind::Wall));
    assert!(outcome.partial);
    let signals = h.driver.signals(&container_id(&h));
    assert_eq!(signals.len(), 2);
    assert_eq!(
        (signals[0].signal, signals[0].at),
        (SIGTERM, Duration::from_secs(60))
    );
    assert_eq!(
        (signals[1].signal, signals[1].at),
        (SIGKILL, Duration::from_secs(70))
    );
    // 超时后仍 collect（partial）。
    assert_eq!(outcome.files.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn idle_timer_fires_without_progress() {
    let h = harness(1);
    h.driver.script(
        sbx(3),
        FakeScript {
            behavior: FakeBehavior::Forever { honor_term: true },
            log: vec![(
                Duration::from_secs(10),
                LogStream::Stdout,
                br#"{"v":1,"seq":1,"type":"heartbeat","in_flight":0}"#.to_vec(),
            )],
            out: Vec::new(),
        },
    );
    h.service.submit(&ctx(), spec(3)).await.unwrap();
    let outcome = h.service.wait(&att(3)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::KilledTimer);
    assert_eq!(outcome.terminal.timer, Some(TimerKind::Idle));
    // 最后一条合法事件在第 10 秒，空闲 120 秒 → 第 130 秒发 SIGTERM。
    let signals = h.driver.signals(&container_id(&h));
    assert_eq!(
        (signals[0].signal, signals[0].at),
        (SIGTERM, Duration::from_secs(130))
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_while_running_and_first_writer_wins() {
    let h = harness(2);
    // a 响应 SIGTERM；b 忽略它，取消会在 b 的宽限期内到达。
    for (n, honor_term) in [(4, true), (5, false)] {
        h.driver.script(
            sbx(n),
            FakeScript {
                behavior: FakeBehavior::Forever { honor_term },
                ..FakeScript::default()
            },
        );
    }
    let mut a = spec(4);
    a.limits.wall_time_s = 60;
    let mut b = spec(5);
    b.limits.wall_time_s = 60;
    h.service.submit(&ctx(), a).await.unwrap();
    h.service.submit(&ctx(), b).await.unwrap();

    // 取消在墙钟之前一刻到达：取消胜。
    tokio::time::sleep(Duration::from_millis(59_999)).await;
    h.service.cancel(&ctx(), &att(4)).await.unwrap();
    // 墙钟先到（第 60 秒），取消在宽限期内才到：计时器胜，cancel_requested 仍记下。
    tokio::time::sleep(Duration::from_millis(2)).await;
    h.service.cancel(&ctx(), &att(5)).await.unwrap();

    let a = h.service.wait(&att(4)).await.unwrap();
    assert_eq!(a.terminal.cause, TerminalCause::KilledCancel);
    assert!(a.terminal.cancel_requested);
    let b = h.service.wait(&att(5)).await.unwrap();
    assert_eq!(b.terminal.cause, TerminalCause::KilledTimer);
    assert!(b.terminal.cancel_requested);
    // cancel 幂等。
    h.service.cancel(&ctx(), &att(4)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancel_while_queued_never_creates() {
    let h = harness(1);
    h.driver.script(
        sbx(6),
        FakeScript {
            behavior: FakeBehavior::Forever { honor_term: true },
            ..FakeScript::default()
        },
    );
    h.service.submit(&ctx(), spec(6)).await.unwrap();
    h.service.submit(&ctx(), spec(7)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let cap = h.service.capacity().await;
    assert_eq!((cap.running, cap.queued, cap.limit), (1, 1, 1));
    h.service.cancel(&ctx(), &att(7)).await.unwrap();
    let queued = h.service.wait(&att(7)).await.unwrap();
    assert_eq!(queued.terminal.cause, TerminalCause::KilledCancel);
    assert!(queued.files.is_empty());
    // 排队中的 job 没有签发令牌、没有建容器。
    assert!(!h
        .egress
        .log()
        .iter()
        .any(|l| l.ends_with(&att(7).to_string())));
    assert_eq!(
        h.driver
            .calls()
            .iter()
            .filter(|c| matches!(c, DriverCall::Create { .. }))
            .count(),
        1
    );
    h.service.cancel(&ctx(), &att(6)).await.unwrap();
    h.service.wait(&att(6)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn queue_is_fifo_and_never_fails() {
    let h = harness(1);
    for n in 10..13 {
        h.driver.script(sbx(n), golden_script());
    }
    for n in 10..13 {
        h.service.submit(&ctx(), spec(n)).await.unwrap();
    }
    for n in 10..13 {
        assert_eq!(
            h.service.wait(&att(n)).await.unwrap().terminal.cause,
            TerminalCause::Exited
        );
    }
    let starts: Vec<String> = h
        .driver
        .calls()
        .iter()
        .filter_map(|c| match c {
            DriverCall::Create { name } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let expected: Vec<String> = (10..13).map(|n| format!("gantry-sbx-{}", sbx(n))).collect();
    assert_eq!(starts, expected);
}

#[tokio::test(start_paused = true)]
async fn submit_is_idempotent() {
    let h = harness(2);
    h.driver.script(sbx(14), golden_script());
    let first = h.service.submit(&ctx(), spec(14)).await.unwrap();
    let second = h.service.submit(&ctx(), spec(14)).await.unwrap();
    assert_eq!(first, second);
    let mut other = spec(14);
    other.limits.pids = 1;
    assert!(matches!(
        h.service.submit(&ctx(), other).await,
        Err(SandboxError::InvalidSpec(_))
    ));
    h.service.wait(&att(14)).await.unwrap();
    assert_eq!(
        h.driver
            .calls()
            .iter()
            .filter(|c| matches!(c, DriverCall::Create { .. }))
            .count(),
        1
    );
}

async fn expect_infra(h: &Harness, n: u128, detail: InfraDetail) {
    let err = h.service.wait(&att(n)).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::Infra { detail: d, .. } if d == detail),
        "{err:?}"
    );
    // 容器、staging、令牌全部清理。
    assert_eq!(h.driver.container_count(), 0);
    assert!(!h
        .root
        .path()
        .join(sbx(n).to_string())
        .join("inputs")
        .exists());
    let log = h.egress.log();
    let issued = log
        .iter()
        .filter(|l| *l == &format!("issue {}", att(n)))
        .count();
    let revoked = log
        .iter()
        .filter(|l| *l == &format!("revoke {}", att(n)))
        .count();
    assert_eq!(issued, revoked, "{log:?}");
}

#[tokio::test(start_paused = true)]
async fn stage_failures_clean_everything_up() {
    let h = harness(4);
    for (n, point) in [
        (20, FailPoint::Create),
        (21, FailPoint::Start),
        (22, FailPoint::PrepareDirs),
        (23, FailPoint::ReadOut),
    ] {
        h.driver.script(sbx(n), golden_script());
        h.driver.fail_next(point);
        h.service.submit(&ctx(), spec(n)).await.unwrap();
        let detail = match point {
            FailPoint::Create | FailPoint::Start => InfraDetail::SandboxCreate,
            FailPoint::PrepareDirs => InfraDetail::WorkspacePrep,
            _ => InfraDetail::Collect,
        };
        expect_infra(&h, n, detail).await;
    }

    // prepare_workspace 失败：base 不在镜像仓。
    let mut s = spec(24);
    s.workspace.base_sha = GitOid::parse("2222222222222222222222222222222222222222").unwrap();
    h.service.submit(&ctx(), s).await.unwrap();
    expect_infra(&h, 24, InfraDetail::WorkspacePrep).await;

    // Egress 不健康：不启动，Infra{Egress}。
    h.egress
        .fail_issue
        .store(true, std::sync::atomic::Ordering::SeqCst);
    h.service.submit(&ctx(), spec(25)).await.unwrap();
    let err = h.service.wait(&att(25)).await.unwrap_err();
    assert!(matches!(
        err,
        SandboxError::Infra {
            detail: InfraDetail::Egress,
            ..
        }
    ));
    assert!(!h
        .driver
        .calls()
        .iter()
        .any(|c| matches!(c, DriverCall::Create { name } if name.ends_with(&sbx(25).to_string()))));
}

#[tokio::test(start_paused = true)]
async fn engine_defects_are_invalid_spec_at_submit() {
    let h = harness(1);
    let mut s = spec(30);
    s.exec.env.insert(
        "GITHUB_TOKEN_X".into(),
        "ghp_0123456789abcdefABCDEF0123456789abcd".into(),
    );
    assert!(matches!(
        h.service.submit(&ctx(), s).await,
        Err(SandboxError::InvalidSpec(_))
    ));
    let mut s = spec(31);
    s.limits.mem_mb = 0;
    assert!(matches!(
        h.service.submit(&ctx(), s).await,
        Err(SandboxError::InvalidSpec(_))
    ));
    let mut s = spec(32);
    s.job.job_id = att(99);
    assert!(matches!(
        h.service.submit(&ctx(), s).await,
        Err(SandboxError::InvalidSpec(_))
    ));
    assert!(h.driver.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn oom_and_finish_lies_do_not_drive_terminal() {
    let h = harness(2);
    h.driver.script(
        sbx(40),
        FakeScript {
            behavior: FakeBehavior::OomAfter {
                after: Duration::from_secs(5),
            },
            ..FakeScript::default()
        },
    );
    // 自报 finish = succeeded，但退出码 3。
    h.driver.script(
        sbx(41),
        FakeScript {
            behavior: FakeBehavior::ExitAfter {
                after: Duration::from_secs(5),
                code: 3,
                honor_term: true,
            },
            log: vec![(
                Duration::from_secs(1),
                LogStream::Stdout,
                br#"{"v":1,"seq":1,"type":"finish","status":"succeeded"}"#.to_vec(),
            )],
            out: Vec::new(),
        },
    );
    h.service.submit(&ctx(), spec(40)).await.unwrap();
    h.service.submit(&ctx(), spec(41)).await.unwrap();
    let oom = h.service.wait(&att(40)).await.unwrap();
    assert_eq!(oom.terminal.cause, TerminalCause::KilledRuntime);
    assert!(oom.terminal.oom);
    let liar = h.service.wait(&att(41)).await.unwrap();
    assert_eq!(liar.terminal.cause, TerminalCause::Exited);
    assert_eq!(liar.terminal.exit_code, Some(3));
}

// ---------------------------------------------------------------------------
// §12.A 第 5 条：FakeSandboxService 与真实 SandboxService 的共享一致性用例
// ---------------------------------------------------------------------------

async fn engine_face_conformance(service: &dyn SandboxService, s: SandboxSpec) -> JobOutcome {
    let job = s.job_id;
    let first = service.submit(&ctx(), s.clone()).await.unwrap();
    let second = service.submit(&ctx(), s.clone()).await.unwrap();
    assert_eq!(first, second, "submit 按 job_id 幂等");
    let mut different = s.clone();
    different.limits.pids += 1;
    assert!(matches!(
        service.submit(&ctx(), different).await,
        Err(SandboxError::InvalidSpec(_))
    ));
    let a = service.wait(&job).await.unwrap();
    let b = service.wait(&job).await.unwrap();
    assert_eq!(a, b, "wait 对同一 job 恒返回同一结果");
    service.cancel(&ctx(), &job).await.unwrap();
    service.cancel(&ctx(), &job).await.unwrap();
    service.release(&ctx(), &job).await.unwrap();
    service.release(&ctx(), &job).await.unwrap();
    let unknown = att(999);
    assert_eq!(service.wait(&unknown).await, Err(SandboxError::NotFound));
    assert_eq!(
        service.cancel(&ctx(), &unknown).await,
        Err(SandboxError::NotFound)
    );
    assert_eq!(
        service.release(&ctx(), &unknown).await,
        Err(SandboxError::NotFound)
    );
    a
}

#[tokio::test(start_paused = true)]
async fn shared_conformance_real_service() {
    let h = harness(1);
    h.driver.script(sbx(50), golden_script());
    let outcome = engine_face_conformance(&h.service, spec(50)).await;
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
}

#[tokio::test]
async fn shared_conformance_fake_service() {
    let fake = FakeSandboxService::new();
    let s = spec(51);
    fake.script(
        s.clone(),
        FakeSandboxService::success_outcome(s.job_id, s.sandbox_id),
    );
    let outcome = engine_face_conformance(&fake, s).await;
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
}

fn heartbeat(at: u64, seq: u64) -> (Duration, LogStream, Vec<u8>) {
    (
        Duration::from_secs(at),
        LogStream::Stdout,
        format!(r#"{{"v":1,"seq":{seq},"type":"heartbeat","in_flight":0}}"#).into_bytes(),
    )
}

#[tokio::test(start_paused = true)]
async fn log_interruption_reconnects_from_cursor() {
    let h = harness(1);
    // 心跳间隔 90 秒 < 空闲 120 秒；读取在第一行后中断，重连后必须继续喂空闲计时器。
    h.driver.script(
        sbx(30),
        FakeScript {
            behavior: FakeBehavior::ExitAfter {
                after: Duration::from_secs(300),
                code: 0,
                honor_term: true,
            },
            log: vec![
                heartbeat(10, 1),
                heartbeat(100, 2),
                heartbeat(190, 3),
                heartbeat(280, 4),
            ],
            out: vec![(
                "result.json".into(),
                FakeOutEntry::File(br#"{"status":"succeeded"}"#.to_vec()),
            )],
        },
    );
    h.driver.fail_next(FailPoint::LogsInterrupt);
    h.service.submit(&ctx(), spec(30)).await.unwrap();
    let outcome = h.service.wait(&att(30)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    let cursors: Vec<_> = h
        .driver
        .calls()
        .iter()
        .filter_map(|c| match c {
            DriverCall::Logs(_, since) => Some(*since),
            _ => None,
        })
        .collect();
    assert_eq!(
        cursors,
        vec![None, Some(gantry_sandbox::driver::LogCursor(1))]
    );
}

#[tokio::test(start_paused = true)]
async fn progress_stream_ends_with_the_job() {
    let h = harness(1);
    h.driver.script(sbx(31), golden_script());
    h.service.submit(&ctx(), spec(31)).await.unwrap();
    let mut stream = h.service.progress(&att(31));
    h.service.wait(&att(31)).await.unwrap();
    let mut seen = 0;
    while std::future::poll_fn(|cx| stream.as_mut().poll_next(cx))
        .await
        .is_some()
    {
        seen += 1;
    }
    assert_eq!(seen, 2);
    // 结束后订阅：立即结束。
    let mut late = h.service.progress(&att(31));
    assert!(std::future::poll_fn(|cx| late.as_mut().poll_next(cx))
        .await
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn concurrent_submits_of_one_job_accept_once() {
    let h = harness(1);
    h.driver.script(sbx(32), golden_script());
    let c = ctx();
    let (a, b) = tokio::join!(
        h.service.submit(&c, spec(32)),
        h.service.submit(&c, spec(32))
    );
    assert_eq!(a.unwrap(), b.unwrap());
    h.service.wait(&att(32)).await.unwrap();
    assert_eq!(
        h.driver
            .calls()
            .iter()
            .filter(|c| matches!(c, DriverCall::Create { .. }))
            .count(),
        1
    );
}

// ---- §12.H 崩溃恢复与孤儿巡检 ----

fn unfinished(n: u128) -> UnfinishedAttempt {
    UnfinishedAttempt {
        job_id: att(n),
        sandbox_id: sbx(n),
        project_id: base_job().project_id,
    }
}

fn handle(n: u128) -> gantry_core::ports::sandbox::JobHandle {
    gantry_core::ports::sandbox::JobHandle {
        job_id: att(n),
        sandbox_id: sbx(n),
    }
}

/// 一直运行、每 60 秒一次心跳（空闲计时器不会触发）。
fn long_running() -> FakeScript {
    FakeScript {
        behavior: FakeBehavior::Forever { honor_term: true },
        log: (1..=15).map(|k| heartbeat(k * 60, k)).collect(),
        out: vec![(
            "result.json".into(),
            FakeOutEntry::File(br#"{"status":"failed"}"#.to_vec()),
        )],
    }
}

fn creates(h: &Harness) -> usize {
    h.driver
        .calls()
        .iter()
        .filter(|c| matches!(c, DriverCall::Create { .. }))
        .count()
}

fn spool_seqs(h: &Harness, n: u128) -> Vec<u64> {
    let bytes = std::fs::read(
        h.root
            .path()
            .join(sbx(n).to_string())
            .join("progress.spool.jsonl"),
    )
    .unwrap_or_default();
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_slice(line).unwrap();
            value["event"]["seq"].as_u64().unwrap()
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn recover_running_keeps_deadline_and_dedups_log_reread() {
    let mut h = harness(2);
    h.driver.script(sbx(40), long_running());
    h.service.submit(&ctx(), spec(40)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(150)).await;
    h.restart();
    // 崩溃留下半条 spool 记录：恢复先截掉它，重读到的事件接在完整记录后面。
    {
        use std::io::Write as _;
        let path = h
            .root
            .path()
            .join(sbx(40).to_string())
            .join("progress.spool.jsonl");
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(br#"{"received_at":"2023-"#).unwrap();
    }
    let recovered = h.service.recover(&ctx(), &[unfinished(40)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Running(handle(40))]);
    let outcome = h.service.wait(&att(40)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::KilledTimer);
    assert_eq!(outcome.terminal.timer, Some(TimerKind::Wall));
    // deadline 按持久化时刻重建：仍是启动后第 600 秒，不因重启而延长。
    let signals = h.driver.signals(&container_id(&h));
    assert_eq!(
        (signals[0].signal, signals[0].at),
        (SIGTERM, Duration::from_secs(600))
    );
    // 没有重复沙盒；重读日志不重复写 spool。
    assert_eq!(creates(&h), 1);
    let seqs = spool_seqs(&h, 40);
    assert_eq!(seqs, (1..=10).collect::<Vec<_>>());
    assert_eq!(
        h.egress.log(),
        vec![format!("issue {}", att(40)), format!("revoke {}", att(40))]
    );
    // 同一服务上再次 recover：按已知状态作答。
    assert_eq!(
        h.service.recover(&ctx(), &[unfinished(40)]).await.unwrap(),
        vec![RecoveredJob::Exited(handle(40))]
    );
}

#[tokio::test(start_paused = true)]
async fn recover_after_exit_collects_with_same_hashes() {
    let baseline = {
        let h = harness(1);
        h.driver.script(sbx(41), golden_script());
        h.service.submit(&ctx(), spec(41)).await.unwrap();
        h.service.wait(&att(41)).await.unwrap()
    };
    let mut h = harness(1);
    h.driver.script(sbx(41), golden_script());
    h.service.submit(&ctx(), spec(41)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    h.restart();
    // 无人监督期间容器自己退出（第 30 秒）。
    tokio::time::sleep(Duration::from_secs(30)).await;
    let recovered = h.service.recover(&ctx(), &[unfinished(41)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Exited(handle(41))]);
    let outcome = h.service.wait(&att(41)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    assert_eq!(outcome.terminal.exit_code, Some(0));
    assert_eq!(outcome.files, baseline.files);
    assert_eq!(outcome.stdout_tail, baseline.stdout_tail);
    assert_eq!(outcome.stderr_tail, baseline.stderr_tail);
    assert_eq!(spool_seqs(&h, 41), vec![1, 2]);
    assert_eq!(creates(&h), 1);
    assert_eq!(h.driver.container_count(), 0);
    let revokes = h
        .egress
        .log()
        .iter()
        .filter(|l| l.starts_with("revoke"))
        .count();
    assert_eq!(revokes, 1);
}

#[tokio::test(start_paused = true)]
async fn persisted_reason_is_resignalled_after_restart() {
    use gantry_sandbox::service::meta::{Meta, Phase, TerminalReason};
    let mut h = harness(1);
    h.driver.script(sbx(42), long_running());
    h.service.submit(&ctx(), spec(42)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(50)).await;
    h.restart();
    // 原因已写盘（fsync）但信号尚未发出时崩溃。
    let path = h.root.path().join(sbx(42).to_string()).join("meta.json");
    let mut meta = Meta::load(&path).unwrap().unwrap();
    meta.terminal_reason = Some(TerminalReason::Cancel);
    meta.cancel_requested = true;
    meta.phase = Phase::Stopping;
    meta.save(&path).unwrap();
    let recovered = h.service.recover(&ctx(), &[unfinished(42)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Running(handle(42))]);
    let outcome = h.service.wait(&att(42)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::KilledCancel);
    assert!(outcome.terminal.cancel_requested);
    let signals = h.driver.signals(&container_id(&h));
    assert_eq!(
        (signals[0].signal, signals[0].at),
        (SIGTERM, Duration::from_secs(50))
    );
}

#[tokio::test(start_paused = true)]
async fn vanished_container_with_readable_out_collects_as_lost() {
    let mut h = harness(1);
    h.driver.script(sbx(43), golden_script());
    h.service.submit(&ctx(), spec(43)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    h.restart();
    tokio::time::sleep(Duration::from_secs(30)).await;
    h.driver.vanish(&container_id(&h));
    let recovered = h.service.recover(&ctx(), &[unfinished(43)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Exited(handle(43))]);
    let outcome = h.service.wait(&att(43)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Lost);
    assert!(outcome.partial);
    assert_eq!(outcome.files.len(), 4);
    assert!(h.egress.log().contains(&format!("revoke {}", att(43))));
}

#[tokio::test(start_paused = true)]
async fn container_and_staging_gone_is_lost() {
    let mut h = harness(1);
    h.driver.script(sbx(44), long_running());
    h.service.submit(&ctx(), spec(44)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    h.restart();
    h.driver.vanish(&container_id(&h));
    {
        use gantry_sandbox::driver::SandboxDriver as _;
        let layout = gantry_sandbox::spec::StagingLayout::new(h.root.path(), sbx(44)).unwrap();
        h.driver.purge_staging(&layout).await.unwrap();
    }
    let recovered = h.service.recover(&ctx(), &[unfinished(44)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Lost(handle(44))]);
    let err = h.service.wait(&att(44)).await.unwrap_err();
    assert!(
        matches!(
            err,
            SandboxError::Infra {
                detail: InfraDetail::Lost,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(h.egress.log().contains(&format!("revoke {}", att(44))));
    let types = event_types(&h.store).await;
    assert_eq!(types.last().map(String::as_str), Some("sandbox.destroyed"));
}

#[tokio::test(start_paused = true)]
async fn never_accepted_is_unknown_and_builds_nothing() {
    let h = harness(1);
    let recovered = h.service.recover(&ctx(), &[unfinished(45)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Unknown { job_id: att(45) }]);
    assert_eq!(creates(&h), 0);
    assert!(!h.root.path().join(sbx(45).to_string()).exists());
    assert!(matches!(
        h.service.wait(&att(45)).await,
        Err(SandboxError::NotFound)
    ));
}

#[tokio::test(start_paused = true)]
async fn not_started_is_unknown_and_resubmit_runs_once() {
    let mut h = harness(1);
    h.driver.script(sbx(46), long_running());
    h.driver.script(sbx(47), golden_script());
    h.service.submit(&ctx(), spec(46)).await.unwrap();
    h.service.submit(&ctx(), spec(47)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    h.restart();
    let recovered = h
        .service
        .recover(&ctx(), &[unfinished(46), unfinished(47)])
        .await
        .unwrap();
    assert_eq!(
        recovered,
        vec![
            RecoveredJob::Running(handle(46)),
            RecoveredJob::Unknown { job_id: att(47) }
        ]
    );
    // engine 以同一 spec 重新提交；它排在恢复出的 46 后面。
    h.service.submit(&ctx(), spec(47)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(creates(&h), 1);
    h.service.cancel(&ctx(), &att(46)).await.unwrap();
    h.service.wait(&att(46)).await.unwrap();
    let outcome = h.service.wait(&att(47)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    assert_eq!(creates(&h), 2);
}

#[tokio::test(start_paused = true)]
async fn sweep_orphans_spares_live_and_recent_then_revokes_and_removes() {
    let mut h = harness(1);
    h.driver.script(sbx(48), long_running());
    h.service.submit(&ctx(), spec(48)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    h.orphan_grace = Duration::from_secs(3600);
    h.restart();
    // 有对应未终结 Attempt 的不误杀；未过宽限的也不动。
    let live: BTreeSet<AttemptId> = [att(48)].into_iter().collect();
    let report = h.service.sweep_orphans(&ctx(), &live).await.unwrap();
    assert_eq!((report.destroyed, report.staging_removed), (0, 0));
    let report = h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!((report.destroyed, report.staging_removed), (0, 0));
    assert_eq!(h.driver.container_count(), 1);

    h.orphan_grace = Duration::ZERO;
    h.restart();
    // 另一个没有 meta.json 的残留目录。
    std::fs::create_dir_all(h.root.path().join(sbx(49).to_string())).unwrap();
    let report = h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!((report.destroyed, report.staging_removed), (1, 2));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(h.driver.container_count(), 0);
    assert!(!h.root.path().join(sbx(48).to_string()).exists());
    assert!(!h.root.path().join(sbx(49).to_string()).exists());
    let log = h.egress.log();
    let revoke = log.iter().position(|l| *l == format!("revoke {}", att(48)));
    assert!(revoke.is_some(), "{log:?}");
}

#[tokio::test(start_paused = true)]
async fn finished_but_unrecorded_job_replays_the_same_outcome() {
    let mut h = harness(1);
    h.driver.script(sbx(50), golden_script());
    h.service.submit(&ctx(), spec(50)).await.unwrap();
    let before = h.service.wait(&att(50)).await.unwrap();
    // engine 写 attempt.finished 之前崩溃：沙盒已销毁，out/ 仍在（release 尚未调用）。
    h.restart();
    let recovered = h.service.recover(&ctx(), &[unfinished(50)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Exited(handle(50))]);
    let after = h.service.wait(&att(50)).await.unwrap();
    assert_eq!(after.terminal, before.terminal);
    assert_eq!(after.files, before.files);
    assert_eq!(after.stdout_tail, before.stdout_tail);
    assert_eq!(after.stderr_tail, before.stderr_tail);
    assert!(!after.partial);
    assert_eq!(creates(&h), 1);
}

#[tokio::test(start_paused = true)]
async fn exited_past_deadline_is_not_a_timer_kill() {
    let mut h = harness(1);
    h.driver.script(sbx(51), golden_script());
    h.service.submit(&ctx(), spec(51)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    h.restart();
    // 停机期间容器正常退出，恢复时已过持久化的 deadline（第 600 秒）。
    tokio::time::sleep(Duration::from_secs(700)).await;
    let recovered = h.service.recover(&ctx(), &[unfinished(51)]).await.unwrap();
    assert_eq!(recovered, vec![RecoveredJob::Exited(handle(51))]);
    let outcome = h.service.wait(&att(51)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    assert!(!outcome.partial);
    assert!(h.driver.signals(&container_id(&h)).is_empty());
}

#[tokio::test(start_paused = true)]
async fn recovered_overdraft_counts_against_capacity() {
    let mut h = harness(2);
    for n in [52, 53] {
        h.driver.script(sbx(n), long_running());
        h.service.submit(&ctx(), spec(n)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    // 重启后上限降为 1：两个容器都还在跑。
    h.max_concurrent = 1;
    h.restart();
    let recovered = h
        .service
        .recover(&ctx(), &[unfinished(52), unfinished(53)])
        .await
        .unwrap();
    assert_eq!(
        recovered,
        vec![
            RecoveredJob::Running(handle(52)),
            RecoveredJob::Running(handle(53))
        ]
    );
    h.driver.script(sbx(54), golden_script());
    h.service.submit(&ctx(), spec(54)).await.unwrap();
    h.service.cancel(&ctx(), &att(52)).await.unwrap();
    h.service.wait(&att(52)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    // 53 仍在运行，实际容器数没有降到上限以下：54 不能启动。
    assert_eq!(creates(&h), 2);
    h.service.cancel(&ctx(), &att(53)).await.unwrap();
    h.service.wait(&att(53)).await.unwrap();
    let outcome = h.service.wait(&att(54)).await.unwrap();
    assert_eq!(outcome.terminal.cause, TerminalCause::Exited);
    assert_eq!(creates(&h), 3);
}

#[tokio::test(start_paused = true)]
async fn sweep_keeps_orphans_whose_revoke_failed() {
    let mut h = harness(1);
    h.driver.script(sbx(55), long_running());
    h.service.submit(&ctx(), spec(55)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    h.restart();
    h.egress
        .fail_revoke
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let report = h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!((report.destroyed, report.staging_removed), (0, 0));
    assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
    assert_eq!(h.driver.container_count(), 1);
    assert!(h.root.path().join(sbx(55).to_string()).exists());
    // 下一轮 Egress 恢复：照常吊销并清理。
    h.egress
        .fail_revoke
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let report = h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!((report.destroyed, report.staging_removed), (1, 1));
    assert_eq!(h.driver.container_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn sweep_cleans_leftovers_of_released_jobs() {
    let h = harness(1);
    h.driver.script(sbx(56), golden_script());
    // 销毁失败：留给孤儿巡检（§9）。
    h.driver.fail_next(FailPoint::Remove);
    h.service.submit(&ctx(), spec(56)).await.unwrap();
    h.service.wait(&att(56)).await.unwrap();
    assert_eq!(h.driver.container_count(), 1);
    // 未 release：结果还要保留，不清理。
    let report = h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(report.destroyed, 0);
    h.service.release(&ctx(), &att(56)).await.unwrap();
    let report = h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .unwrap();
    assert_eq!(report.destroyed, 1);
    assert_eq!(h.driver.container_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn issued_token_carries_the_decision_time_config_rev() {
    let h = harness(1);
    // 决策之后 Egress 应用了新配置（修订与 spec 里的不同）。
    let applied = h.egress.status().await.unwrap().applied_rev.as_u64();
    assert_ne!(applied, DECIDED_REV);
    h.driver.script(sbx(60), golden_script());
    h.service.submit(&ctx(), spec(60)).await.unwrap();
    h.service.wait(&att(60)).await.unwrap();
    assert_eq!(*h.egress.issued_revs.lock().unwrap(), vec![DECIDED_REV]);
}
