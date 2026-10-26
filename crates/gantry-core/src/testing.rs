//! 测试支撑（cargo feature `testing`）：假时钟、确定性熵源、实体工厂与契约夹具。
//! 这些替身不是生产实现，不得用于真实业务。

use std::sync::Mutex;

use serde_json::Value;

use crate::canon::{GitOid, RepoSlug, Sha256Digest, SubjectDigest};
use crate::entities::*;
use crate::forge::{ActionPayload, CreatePrPayload};
use crate::ids::*;
use crate::level::Level;
use crate::text::UntrustedText;
use crate::time::Timestamp;

const BASE_MS: i64 = 1_700_000_000_000;

/// 可手动推进的时钟。
pub struct FakeClock {
    now: Mutex<Timestamp>,
}

impl FakeClock {
    pub fn new(ms: i64) -> Self {
        Self {
            now: Mutex::new(Timestamp::from_millis(ms)),
        }
    }

    pub fn now_timestamp(&self) -> Timestamp {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn set(&self, ts: Timestamp) {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ts;
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now = Timestamp::from_millis(now.as_millis().saturating_add(ms));
    }

    pub fn advance_secs(&self, secs: u64) {
        self.advance_ms((secs as i64).saturating_mul(1000));
    }
}

impl crate::ids::Clock for FakeClock {
    fn now(&self) -> Timestamp {
        self.now_timestamp()
    }
}

/// 确定性熵源：每次 `fill` 用内部计数器填充可预测字节。
pub struct SeqEntropy {
    state: Mutex<u64>,
}

impl SeqEntropy {
    pub fn new(seed: u64) -> Self {
        Self {
            state: Mutex::new(seed),
        }
    }
}

impl crate::ids::Entropy for SeqEntropy {
    fn fill(&self, buf: &mut [u8]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for slot in buf {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            *slot = (*state >> 33) as u8;
        }
    }
}

/// 契约示例夹具，原样保存，供所有泳道测试共用。
pub mod fixtures {
    use super::*;

    pub fn load(name: &str) -> Value {
        let text = match name {
            "approval_payload" => include_str!("../fixtures/core/approval_payload.json"),
            "idempotency_intent" => include_str!("../fixtures/core/idempotency_intent.json"),
            "stage_changed_event" => include_str!("../fixtures/core/stage_changed_event.json"),
            other => panic!("unknown core fixture: {other}"),
        };
        serde_json::from_str(text).expect("fixture must be valid JSON")
    }
}

pub mod builders {
    use super::*;

    pub fn timestamp() -> Timestamp {
        Timestamp::from_millis(BASE_MS)
    }

    pub fn later(seconds: i64) -> Timestamp {
        Timestamp::from_millis(BASE_MS + seconds * 1000)
    }

    fn id<K: IdKind>(random: u128) -> Id<K> {
        Id::from_parts(BASE_MS as u64, random).expect("builder id parts must be valid")
    }

    pub fn untrusted(text: &str) -> UntrustedText {
        UntrustedText::new(text.to_owned(), "sandbox:report".into(), false)
    }

    pub fn repo() -> RepoSlug {
        RepoSlug::parse("acme/widgets").unwrap()
    }

    pub fn git_oid() -> GitOid {
        GitOid::parse("9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345").unwrap()
    }

    pub fn sha() -> Sha256Digest {
        Sha256Digest::parse(
            "sha256:5b1f0c2a9e7d4f3b8a6c1d0e2f4a5b7c9d1e3f50a2b4c6d8e0f1a3b5c7d9e1f3",
        )
        .unwrap()
    }

    pub fn project() -> Project {
        Project {
            id: id(1),
            repo: repo(),
            installation_id: 42,
            bot_user_id: 9988776,
            default_branch: "main".into(),
            allowed_merge_methods: vec![MergeMethod::Merge, MergeMethod::Squash],
            merge_queue_enabled: false,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn issue() -> IssueRef {
        IssueRef {
            repo: repo(),
            number: 42,
            node_id: "I_kwDOabc123".into(),
            title: untrusted("Pagination skips the last item"),
            state: IssueState::Open,
            state_reason: None,
            synced_at: timestamp(),
        }
    }

    pub fn pull_request() -> PullRequestRef {
        PullRequestRef {
            repo: repo(),
            number: 57,
            node_id: "PR_kwDOabc123".into(),
            title: untrusted("Fix pagination"),
            state: PullRequestState::Open,
            head_sha: git_oid(),
            base_ref: "main".into(),
            base_sha: git_oid(),
            draft: false,
            mergeable_state: "clean".into(),
            in_merge_queue: false,
            checks: CheckSummary {
                passed: 4,
                failed: 0,
                pending: 1,
                required_ok: false,
            },
            review_state: ReviewState::None,
            synced_at: timestamp(),
        }
    }

    pub fn delivery() -> Delivery {
        Delivery {
            id: id(2),
            project_id: id(1),
            title: "Fix pagination".into(),
            level_cap: Level::Low,
            level_effective: Level::Low,
            stage: Stage::Implement,
            stage_status: StageStatus::Running,
            issues: vec![issue()],
            pr: None,
            worktree_id: None,
            parent_id: None,
            depends_on: Vec::new(),
            origin: DeliveryOrigin::Conversation,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn workflow() -> Workflow {
        Workflow {
            id: id(3),
            delivery_id: Some(id(2)),
            conversation_id: None,
            stage: Some(Stage::Implement),
            graph_rev: 1,
            status: WorkflowStatus::Running,
            template: "low.single".into(),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn node() -> Node {
        Node {
            id: id(4),
            workflow_id: id(3),
            role: Role::Build,
            tier: Tier::Worker,
            exec: Exec::Sandbox,
            depends_on: Vec::new(),
            required: true,
            status: NodeStatus::Pending,
            verdict: None,
            superseded_by: None,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn attempt() -> Attempt {
        Attempt {
            id: id(5),
            node_id: id(4),
            n: 1,
            sandbox_id: None,
            status: AttemptStatus::Preparing,
            started_at: None,
            ended_at: None,
            result_ref: None,
            failure_class: None,
            failure_detail: None,
            usage: TokenUsage::default(),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn worktree() -> Worktree {
        Worktree {
            id: id(6),
            project_id: id(1),
            branch: "gantry/42-fix-pagination".into(),
            base_ref: "main".into(),
            path: "/var/lib/gantry/worktrees/acme-widgets/42".into(),
            lock: None,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn sandbox() -> Sandbox {
        Sandbox {
            id: id(7),
            driver: "podman".into(),
            trust: SandboxTrust::Untrusted,
            image_digest: sha(),
            limits: SandboxLimits {
                cpu_cores: 2.0,
                mem_mb: 4096,
                disk_mb: 10240,
                pids: 512,
                wall_time_s: 3600,
            },
            status: SandboxStatus::Created,
            worktree_snapshot: WorktreeSnapshot {
                mirror: "acme/widgets".into(),
                base_sha: git_oid(),
                pr_base_sha: None,
            },
            egress_profile: "worker".into(),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn conversation() -> Conversation {
        Conversation {
            id: id(8),
            project_id: id(1),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn message() -> Message {
        Message {
            id: id(9),
            conversation_id: id(8),
            role: MessageRole::Agent,
            agent_kind: Some(AgentKind::Worker),
            text: "PR 已创建".into(),
            tone: Some(MessageTone::Ok),
            level_cap: Some(Level::Low),
            refs: vec![Ref::Pr {
                repo: repo(),
                number: 57,
            }],
            cards: vec![Card::Approval {
                approval_id: id(11),
            }],
            delivery_id: Some(id(2)),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn inbox_item() -> InboxItem {
        InboxItem {
            id: id(10),
            project_id: Some(id(1)),
            delivery_id: Some(id(2)),
            kind: InboxKind::DecisionNeeded,
            severity: Severity::Warn,
            summary: "评审要求重新规划".into(),
            payload: InboxPayload::DecisionNeeded {
                kind: DecisionKind::GateReject,
                reason_code: ReasonCode::parse("gate_reject").unwrap(),
                options: vec![
                    InboxOption::new("replan", "重新规划", true),
                    InboxOption::new("abandon", "放弃", false),
                ],
                node_id: Some(id(4)),
                patch: None,
            },
            status: InboxStatus::Open,
            dedup_key: "dlv:2:gate_reject".into(),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn approval() -> Approval {
        Approval {
            id: id(11),
            delivery_id: Some(id(2)),
            action: ApprovalAction::Forge(ForgeActionKind::MergePr),
            target: ApprovalTarget::Pr {
                repo: repo(),
                number: 57,
            },
            subject_digest: SubjectDigest::Git(git_oid()),
            device_id: Some(id(13)),
            signature: None,
            status: ApprovalStatus::Pending,
            expires_at: later(86_400),
            consumed_by: None,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn device() -> Device {
        Device {
            id: id(13),
            name: "MacBook".into(),
            platform: DevicePlatform::Macos,
            cert_serial: "01".into(),
            pubkey_fpr: sha(),
            approval_pubkey: Some("BHL4aSHzHatNyrq8TX4bJNAxH7no8LM4qb5TVlHTT7Za".into()),
            approval_pubkey_fpr: Some(sha()),
            role: DeviceRole::Approver,
            status: DeviceStatus::Active,
            last_seen: Some(timestamp()),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn outbox_action() -> OutboxAction {
        OutboxAction {
            id: id(14),
            project_id: id(1),
            delivery_id: id(2),
            idempotency_key: crate::canon::IdempotencyKey::derive(
                &id(1),
                &id(2),
                ForgeActionKind::CreatePr,
                "pr:gantry/42-fix-pagination->main",
                &serde_json::json!({"base":"main"}),
                0,
            )
            .unwrap(),
            kind: ForgeActionKind::CreatePr,
            payload: ActionPayload::CreatePr(CreatePrPayload {
                repo: "acme/widgets".parse().unwrap(),
                head: "gantry/fix".into(),
                base: "main".into(),
                title: "fix".into(),
                body: "body".into(),
                draft: false,
                closes: Vec::new(),
                expected_head_oid: GitOid::parse("d7d018f867d6c5b78e82906386420d45a4d84486")
                    .unwrap(),
                maintainer_can_modify: false,
            }),
            requires_approval: None,
            depends_on: Vec::new(),
            priority: Priority::Normal,
            not_before: None,
            epoch: 0,
            status: OutboxStatus::Queued,
            attempts: 0,
            lease_until: None,
            last_error: None,
            expected_effects: Vec::new(),
            github_ref: None,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    pub fn budget_ledger() -> BudgetLedger {
        BudgetLedger {
            id: id(15),
            subject: BudgetSubject::Delivery,
            delivery_id: Some(id(2)),
            conversation_id: None,
            project_id: id(1),
            day: None,
            template: Some("low.single".into()),
            limits: BudgetLimits::delivery(
                [
                    (Tier::Utility, 100_000),
                    (Tier::Planner, 50_000),
                    (Tier::Worker, 1_000_000),
                ]
                .into_iter()
                .collect(),
                6,
                2,
                1_800,
                1,
                30,
            ),
            used: BudgetUsed::default(),
            config_rev: Some(Seq::new(1).unwrap()),
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_are_valid() {
        assert!(builders::workflow().validate().is_ok());
        assert!(builders::node().validate().is_ok());
        assert!(builders::approval().validate().is_ok());
        assert!(builders::inbox_item().validate().is_ok());
        assert!(builders::budget_ledger().validate().is_ok());
    }

    #[test]
    fn fake_clock_and_entropy_are_deterministic() {
        let clock = std::sync::Arc::new(FakeClock::new(BASE_MS));
        let gen = IdGen::new(clock.clone(), std::sync::Arc::new(SeqEntropy::new(7)));
        let first: ProjectId = gen.next().unwrap();
        let second: ProjectId = gen.next().unwrap();
        assert!(first < second);
        clock.advance_ms(1);
        let third: ProjectId = gen.next().unwrap();
        assert!(second < third);
    }

    #[test]
    fn fixture_loads() {
        assert!(fixtures::load("approval_payload").is_object());
    }
}
