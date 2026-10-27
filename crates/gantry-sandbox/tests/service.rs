//! sandbox.md §12.A 第 5 条、§12.D、§12.E 末条、§12.F「先停后取」、§12.G：`SandboxManager`
//! 跑在 `FakeDriver`、`FakeRepo`、`EgressStub`、`Store::open_in_memory` 之上。

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::TempRoot;
use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, SandboxLimits, SandboxTrust, TokenWeights};
use gantry_core::ids::{AttemptId, Id, IdGen, SandboxId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::{
    AppliedConfig, CallClosed, CallStatement, CallUsage, EgressConfigView, EgressControl,
    EgressEndpoints, EgressError, EgressStatus, FinalUsage, IssueJobToken, JobTokenGrant, JobUsage,
    StageSecret, StagedInfo, StagedSecret, SvcTokenGrant, UsageCursor,
};
use gantry_core::ports::models::CallId;
use gantry_core::ports::repo::CommitInfo;
use gantry_core::ports::sandbox::{
    EgressRequest, ExecSpec, GroupName, InfraDetail, JobOutcome, ProfileName, SandboxError,
    SandboxService, SandboxSpec, TerminalCause, TimerKind, Timers, TokenEnvSpec, WorkspaceMode,
    WorkspaceRequest,
};
use gantry_core::secret::SecretName;
use gantry_core::settings::resource::{
    ArtifactIo, MaterialIo, ProgressIo, SandboxDriver, SandboxIo,
};
use gantry_core::testing::{FakeClock, SeqEntropy};
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
    }
}

/// 包一层 EgressStub：记录签发 / 吊销的顺序，可让签发失败。
struct RecordingEgress {
    inner: EgressStub,
    log: std::sync::Mutex<Vec<String>>,
    fail_issue: std::sync::atomic::AtomicBool,
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

struct Harness {
    root: TempRoot,
    driver: Arc<FakeDriver>,
    egress: Arc<RecordingEgress>,
    store: Store,
    service: SandboxManager<FakeDriver>,
}

fn harness(max_concurrent: usize) -> Harness {
    let root = TempRoot::new();
    let driver = Arc::new(FakeDriver::new(root.path()));
    let clock = Arc::new(FakeClock::new(MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(9))));
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids,
    })
    .unwrap();
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
    let egress = Arc::new(RecordingEgress {
        inner: EgressStub::with_default_config(),
        log: Default::default(),
        fail_issue: Default::default(),
    });
    let mut schemas = BTreeMap::new();
    schemas.insert(job.output.structured_schema.id.clone(), SCHEMA.to_vec());
    let service = SandboxManager::new(
        SandboxDeps {
            driver: Arc::clone(&driver),
            repo: Arc::new(repo),
            egress: egress.clone(),
            store: store.clone(),
            clock,
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
        },
    );
    Harness {
        root,
        driver,
        egress,
        store,
        service,
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

#[tokio::test(start_paused = true)]
async fn recover_and_sweep_fail_closed_in_this_slice() {
    let h = harness(1);
    assert!(h.service.recover(&ctx(), &[]).await.is_err());
    assert!(h
        .service
        .sweep_orphans(&ctx(), &BTreeSet::new())
        .await
        .is_err());
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
