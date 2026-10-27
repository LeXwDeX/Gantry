//! 引擎测试的公共装配：内存 Store、`FakeClock`、共享 Store 的 `FakeForge`。
//!
//! 审批签名属于 `ApprovalService`（后续切片）；这里的 `grant` / `deny` 直接追加
//! `approval.granted` / `approval.denied` 事件，模拟 `api` 在验签后写入的结果。

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, IdempotencyKey, RepoSlug, Sha256Digest};
use gantry_core::entities::{
    Actor, ActorKind, Approval, CheckSummary, MergeMethod, Project, PullRequestRef,
    PullRequestState, ReviewState, Seq,
};
use gantry_core::events::{
    ApprovalGrantedData, Event, EventType, NewEvent, PatchValidatedData, Subject,
};
use gantry_core::forge::ForgeActionKind;
use gantry_core::ids::{ActionId, Dev, Dlv, Id, IdGen, IdKind, ProjectId, Ulid};
use gantry_core::ports::{ActorCtx, ForgeError, ForgeReader};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_core::worker::{FileMode, FileStatus, PatchFile, PatchVerdict, RepoPath};
use gantry_fakes::forge::FakeForge;
use gantry_policy::{PolicyBuild, PolicyDeps, PolicyEngine, PolicyRules};
use gantry_store::{AppendOpts, EventFilter, Store, StoreDeps, TypePattern};
use serde_json::{json, Value};

pub const MS: u64 = 1_700_000_000_000;

pub fn ts() -> Timestamp {
    Timestamp::from_millis(MS as i64)
}

pub fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS, n).expect("test id parts")
}

pub fn oid(seed: &str) -> GitOid {
    let hex = sha256(seed.as_bytes()).to_hex();
    GitOid::parse(&hex[..40]).expect("test git oid")
}

pub fn sha(seed: &str) -> Sha256Digest {
    sha256(seed.as_bytes())
}

pub fn slug() -> RepoSlug {
    RepoSlug::parse("acme/widgets").expect("test repo slug")
}

pub fn project_id() -> ProjectId {
    id(1)
}

pub fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::Engine, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(MS, 1).expect("test request id"),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

pub struct Harness {
    pub store: Store,
    pub clock: Arc<FakeClock>,
    pub forge: Arc<FakeForge>,
    pub engine: PolicyEngine,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_rules(PolicyBuild::M1, PolicyRules::builtin())
    }

    pub fn with_rules(build: PolicyBuild, rules: PolicyRules) -> Self {
        Self::assemble(build, rules, false)
    }

    /// `ForgeReader` 一律返回不可用：模拟审批链路读不到 PR 快照 / 项目。
    pub fn with_failing_forge() -> Self {
        Self::assemble(PolicyBuild::M1, PolicyRules::builtin(), true)
    }

    fn assemble(build: PolicyBuild, rules: PolicyRules, failing: bool) -> Self {
        let clock = Arc::new(FakeClock::new(MS as i64));
        let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
        let store = Store::open_in_memory_sync(StoreDeps {
            clock: clock.clone(),
            ids: ids.clone(),
        })
        .expect("in-memory store");
        let forge = Arc::new(FakeForge::new_in(store.clone()));
        forge.insert_project(project());
        forge.upsert_pr(project_id(), "gantry/feature", pull_request(oid("head")));
        let reader: Arc<dyn ForgeReader> = if failing {
            Arc::new(FailingForge)
        } else {
            forge.clone()
        };
        let engine = PolicyEngine::with_rules(
            PolicyDeps {
                store: store.clone(),
                forge: reader,
                clock: clock.clone(),
                ids,
            },
            build,
            rules,
        );
        Self {
            store,
            clock,
            forge,
            engine,
        }
    }

    pub fn advance(&self, duration: Duration) {
        self.clock
            .advance_ms(i64::try_from(duration.as_millis()).expect("fits"));
    }

    pub async fn events(&self, ty: EventType) -> Vec<Event<Value>> {
        let filter = EventFilter {
            project_ids: None,
            include: vec![TypePattern::Exact(ty)],
            exclude: Vec::new(),
        };
        self.store
            .events()
            .scan(Seq::ZERO, &filter, 200)
            .await
            .expect("scan")
            .events
    }

    async fn append_for(&self, approval: &Approval, ty: EventType, data: Value) {
        let subject = Subject::parse(&approval.id.to_string()).expect("subject");
        let last = self
            .store
            .events()
            .by_subject(&subject)
            .await
            .expect("by_subject")
            .last()
            .map(|event| event.seq);
        let mut tx = self.store.begin_write().await.expect("begin");
        tx.append_with(
            NewEvent {
                ty,
                subject,
                project_id: None,
                actor: Actor::new(ActorKind::Device, "dev_test"),
                causation_id: None,
                correlation_id: None,
                data,
                dedup_key: None,
            },
            AppendOpts {
                expect_subject_seq: Some(last),
            },
        )
        .await
        .expect("append");
        tx.commit().await.expect("commit");
    }

    /// 模拟 `api` 验签后的 `approval.granted`。
    pub async fn grant(&self, approval: &Approval) {
        let data = ApprovalGrantedData {
            approval_id: approval.id,
            device_id: id::<Dev>(9),
            approval_pubkey_fpr: sha("pubkey"),
            payload_sha256: sha("payload"),
        };
        self.append_for(
            approval,
            EventType::ApprovalGranted,
            serde_json::to_value(data).expect("data"),
        )
        .await;
    }

    pub async fn deny(&self, approval: &Approval) {
        self.append_for(approval, EventType::ApprovalDenied, serde_json::json!({}))
            .await;
    }

    /// engine 入队 `merge_pr`（PR 7、`head`）写的 outbox.enqueued，带给定 `epoch`。
    pub async fn enqueued_merge(&self, action: ActionId, head: &str, epoch: u32) {
        let delivery = id::<Dlv>(3);
        let intent =
            json!({"expected_head_sha": oid(head), "merge_method": "squash", "mode": "auto"});
        let key = IdempotencyKey::derive(
            &project_id(),
            &delivery,
            ForgeActionKind::MergePr,
            "pr:7:merge",
            &intent,
            epoch,
        )
        .expect("idempotency key");
        let data = json!({"action": {
            "delivery_id": delivery,
            "kind": "merge_pr",
            "payload": {"repo": slug(), "pull_number": 7, "expected_head_sha": oid(head), "merge_method": "squash", "mode": "auto"},
            "target": "pr:7:merge",
            "intent": intent,
            "idempotency_key": key,
            "requires_approval": null,
            "depends_on": [],
            "priority": "normal",
            "not_before": null,
            "epoch": epoch,
            "coalesce_key": null,
            "enqueued_by": {"module": "engine", "causation_id": null}
        }});
        let mut tx = self.store.begin_write().await.expect("begin");
        tx.append(NewEvent {
            ty: EventType::OutboxEnqueued,
            subject: Subject::parse(&action.to_string()).expect("subject"),
            project_id: Some(project_id()),
            actor: Actor::new(ActorKind::Engine, "test"),
            causation_id: None,
            correlation_id: Some(delivery.to_string()),
            data,
            dedup_key: None,
        })
        .await
        .expect("append outbox.enqueued");
        tx.commit().await.expect("commit");
    }

    /// `repo` 写的 patch.validated（结论 allow）。
    pub async fn validated_patch(&self, patch: Sha256Digest, tree: GitOid) {
        let attempt = id::<gantry_core::ids::Att>(77);
        let data = PatchValidatedData {
            job_id: attempt,
            sha256: patch,
            base_sha: oid("base"),
            result_tree: tree,
            verdict: PatchVerdict::Allow,
            files: vec![PatchFile {
                path: RepoPath::parse("src/lib.rs").expect("path"),
                status: FileStatus::Modified,
                old_mode: Some(FileMode::Mode100644),
                new_mode: Some(FileMode::Mode100644),
                added: Some(1),
                deleted: Some(0),
                is_binary: false,
                is_symlink: false,
                is_submodule: false,
                size_after: Some(10),
            }],
            reasons: Vec::new(),
            config_rev: Seq::ZERO,
        };
        let mut tx = self.store.begin_write().await.expect("begin");
        tx.append(NewEvent {
            ty: EventType::PatchValidated,
            subject: Subject::parse(&attempt.to_string()).expect("subject"),
            project_id: Some(project_id()),
            actor: Actor::new(ActorKind::System, "repo"),
            causation_id: None,
            correlation_id: None,
            data: serde_json::to_value(data).expect("data"),
            dedup_key: None,
        })
        .await
        .expect("append patch.validated");
        tx.commit().await.expect("commit");
    }
}

pub fn project() -> Project {
    Project {
        id: project_id(),
        repo: slug(),
        installation_id: 42,
        bot_user_id: 9,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Merge],
        merge_queue_enabled: false,
        created_at: ts(),
        updated_at: ts(),
    }
}

pub fn pull_request(head: GitOid) -> PullRequestRef {
    PullRequestRef {
        repo: slug(),
        number: 7,
        node_id: "PR_test".into(),
        title: gantry_core::text::UntrustedText::new("title".into(), "test".into(), false),
        state: PullRequestState::Open,
        head_sha: head,
        base_ref: "main".into(),
        base_sha: oid("base"),
        draft: false,
        mergeable_state: "clean".into(),
        in_merge_queue: false,
        checks: CheckSummary {
            passed: 1,
            failed: 0,
            pending: 0,
            required_ok: true,
        },
        review_state: ReviewState::None,
        synced_at: ts(),
    }
}

pub struct FailingForge;

fn unavailable<T>() -> Result<T, ForgeError> {
    Err(ForgeError::Unavailable {
        message: "injected".into(),
    })
}

#[async_trait::async_trait]
impl ForgeReader for FailingForge {
    async fn pull_request(
        &self,
        _: &ProjectId,
        _: u64,
    ) -> Result<Option<PullRequestRef>, ForgeError> {
        unavailable()
    }
    async fn issue(
        &self,
        _: &ProjectId,
        _: u64,
    ) -> Result<Option<gantry_core::entities::IssueRef>, ForgeError> {
        unavailable()
    }
    async fn project_snapshot(
        &self,
        _: &ProjectId,
    ) -> Result<gantry_core::ports::ProjectSnapshot, ForgeError> {
        unavailable()
    }
    async fn discussion(
        &self,
        _: &ProjectId,
        _: gantry_core::ports::DiscussionTarget,
        _: gantry_core::ports::DiscussionQuery,
    ) -> Result<gantry_core::ports::Discussion, ForgeError> {
        unavailable()
    }
    async fn ci_log(
        &self,
        _: &ProjectId,
        _: gantry_core::ports::CiLogQuery,
    ) -> Result<gantry_core::ports::CiLog, ForgeError> {
        unavailable()
    }
    async fn changed_files(
        &self,
        _: &ProjectId,
        _: u64,
        _: &GitOid,
    ) -> Result<Vec<gantry_core::ports::ChangedFile>, ForgeError> {
        unavailable()
    }
    async fn review_thread(
        &self,
        _: &ProjectId,
        _: &gantry_core::ports::ThreadNodeId,
    ) -> Result<gantry_core::ports::ReviewThreadView, ForgeError> {
        unavailable()
    }
    async fn check_annotations(
        &self,
        _: &ProjectId,
        _: u64,
    ) -> Result<Vec<gantry_core::ports::Annotation>, ForgeError> {
        unavailable()
    }
    async fn reconcile_now(
        &self,
        _: gantry_core::ports::ReconcileTarget,
        _: &ActorCtx,
    ) -> Result<gantry_core::ports::ReconcileReport, ForgeError> {
        unavailable()
    }
    async fn sync_mirror(
        &self,
        _: &ProjectId,
        _: gantry_core::ports::MirrorWant,
        _: &ActorCtx,
    ) -> Result<(), ForgeError> {
        unavailable()
    }
}
