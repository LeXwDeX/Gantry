//! 替身装配专用的控制命令 `fake.*`：进程级测试从进程外驱动替身、观察事件日志。
//!
//! 这些命令替代还没接线的入口（api 的 `POST /v1/deliveries` 等），只注册在
//! 测试进程里，服务端二进制里没有。

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, SettingsResource, Tier};
use gantry_core::ids::{ActionId, DeliveryId, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::bootstrap_writable;
use gantry_core::settings::defaults::builtin_resolved;
use gantry_core::settings::resource::ResolvedSettings;
use gantry_engine::{budget_from_level, NewDelivery};
use gantry_store::EventFilter;
use serde::Deserialize;
use serde_json::{json, Value};

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, SubjectDigest};
use gantry_core::entities::{
    ApprovalAction, ApprovalStatus, ForgeActionKind, GithubPrRef, GithubRef, PolicyActionId, Seq,
    Stage,
};
use gantry_core::events::{ApprovalGrantedData, EventType, NewEvent, Subject};
use gantry_core::ids::ApprovalId;
use gantry_core::ports::policy::{ActionDecision, ApprovalSpec};
use gantry_engine::decide::{Input, InputIdentity, TickReason};
use gantry_engine::engine::load_delivery_state;
use gantry_fakes::policy::FakePolicy;
use gantry_store::ClaimOpts;

use super::engine_cfg;

use gantry_core::ids::{Clock, Entropy};
use gantry_deploy::manifest::Stage as ManifestStage;
use gantryd::assembly::{OsEntropy, SystemClock};

use super::FakeHandles;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use gantry_core::entities::{DeviceRole, DeviceStatus};
use gantry_core::ports::{ActorCtx, ClientProfile};
use gantry_policy::{
    service::{DeviceKey, GrantRequest},
    ApprovalService,
};
use gantryd::assembly::Assembly;
use gantryd::control::{CommandSet, Response};
use p256::ecdsa::{signature::Signer as _, Signature, SigningKey};

/// `fake.*` 命令的固定执行者；与真实设备 / 主机命令区分开。
const FAKE_ACTOR: &str = "gantryd_fake";

pub struct FakeCommands {
    pub assembly: Arc<Assembly>,
    fakes: FakeHandles,
    startup_cursor: Seq,
    setup: Mutex<SetupState>,
    drive: Mutex<DriveState>,
}

/// `github.setup.*` 与 `settings.bootstrap_set` 的替身状态（deploy/README.md
/// 「与 gantry CLI 的接线」）。真实实现分别归 forge（forge.md §7.14）与设置模块。
#[derive(Default)]
struct SetupState {
    /// 第一台设备配对后为 `true`（P-66）；`fake.end_bootstrap` 置位。
    bootstrap_over: bool,
    /// 进行中的会话：`state` 与已轮询次数。
    session: Option<(String, u32)>,
}

impl FakeCommands {
    pub async fn new(assembly: Arc<Assembly>, fakes: FakeHandles) -> Self {
        // 在运行时主循环启动前读取，避免重放后重新推进游标掩盖持久化丢失。
        let startup_cursor = assembly
            .engine
            .status()
            .await
            .expect("runtime status")
            .cursor;
        Self {
            assembly,
            fakes,
            startup_cursor,
            setup: Mutex::new(SetupState::default()),
            drive: Mutex::new(DriveState::default()),
        }
    }

    fn setup(&self) -> std::sync::MutexGuard<'_, SetupState> {
        self.setup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn setup_begin(&self, args: Value) -> Response {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Begin {
            webhook_url: String,
            redirect_url: String,
            ttl_secs: u64,
        }
        let begin: Begin = match serde_json::from_value(args) {
            Ok(begin) => begin,
            Err(error) => return Response::err("bad_request", error.to_string()),
        };
        let mut setup = self.setup();
        if setup.bootstrap_over {
            return not_bootstrap_period();
        }
        if setup.session.is_some() {
            return Response::err("setup_in_progress", "a setup session is already open");
        }
        let mut bytes = [0u8; 16];
        OsEntropy.fill(&mut bytes);
        let state: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let expires_at = SystemClock
            .now()
            .checked_add_secs(begin.ttl_secs)
            .map(|at| at.to_canonical_string());
        setup.session = Some((state.clone(), 0));
        Response::ok(json!({ "state": state, "expires_at": expires_at }))
    }

    /// 第一次轮询 `pending`，之后 `completed`；App 的权限与事件取 M1 Manifest。
    fn setup_status(&self, args: &Value) -> Response {
        let state = args.get("state").and_then(Value::as_str);
        let mut setup = self.setup();
        let Some((open, polls)) = setup.session.as_mut() else {
            return Response::ok(json!({ "status": "expired" }));
        };
        if state != Some(open.as_str()) {
            return Response::ok(json!({ "status": "expired" }));
        }
        *polls += 1;
        if *polls == 1 {
            return Response::ok(json!({ "status": "pending" }));
        }
        let permissions: serde_json::Map<String, Value> = ManifestStage::M1
            .permissions()
            .into_iter()
            .map(|(key, level)| (key.to_owned(), Value::from(level)))
            .collect();
        Response::ok(json!({
            "status": "completed",
            "app": {
                "app_id": 4242,
                "slug": "gantry-test",
                "html_url": "https://github.com/apps/gantry-test",
                "permissions": permissions,
                "events": ManifestStage::M1.events(),
            },
        }))
    }

    fn bootstrap_set(&self, args: &Value) -> Response {
        if self.setup().bootstrap_over {
            return not_bootstrap_period();
        }
        let Some(resource) = args.get("resource").and_then(Value::as_str) else {
            return Response::err("bad_request", "expected {resource, values}");
        };
        let Ok(resource_id) = serde_json::from_value::<SettingsResource>(json!(resource)) else {
            return Response::err("bad_request", format!("unknown resource {resource}"));
        };
        let Some(values) = args.get("values").and_then(Value::as_object) else {
            return Response::err("bad_request", "expected {resource, values}");
        };
        if values.is_empty() {
            return Response::err("bad_request", "values must not be empty");
        }
        let mut applied = Vec::new();
        for (key, value) in values {
            if !bootstrap_key(key) {
                return Response::err("bad_request", format!("invalid key {key}"));
            }
            if !value.is_string() {
                return Response::err("bad_request", format!("value for {key} must be a string"));
            }
            let path = format!("{resource}.{key}");
            if !bootstrap_writable(resource_id, &path) {
                return Response::err(
                    "not_writable",
                    format!("{path} is not writable during bootstrap"),
                );
            }
            // M1 只接受字符串类型的引导键；对象/数值键走 github setup 回调（deploy.md §7.9）。
            if path != "sandbox.base_image" {
                return Response::err(
                    "not_writable",
                    format!("{path} is not a string setting; use github setup"),
                );
            }
            applied.push(key.clone());
        }
        Response::ok(json!({ "resource": resource, "applied": applied }))
    }
}

/// 引导期写入的资源内相对键：小写字母、数字、`_`、`.`，长度 ≤ 64。
fn bootstrap_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

fn not_bootstrap_period() -> Response {
    Response::err(
        "not_bootstrap_period",
        "a device has been paired; use the desktop settings page",
    )
}

#[async_trait::async_trait]
impl CommandSet for FakeCommands {
    async fn handle(&self, cmd: &str, args: &Value) -> Option<Response> {
        let args = args.clone();
        Some(match cmd {
            "fake.create_delivery" => create_delivery(&self.assembly, args).await,
            "fake.events" => events(&self.assembly, args).await,
            "fake.outbox_action" => {
                match args
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::parse::<ActionId>)
                {
                    Some(Ok(id)) => match self.assembly.store.snapshot().await {
                        Ok(snapshot) => Response::ok(json!(snapshot.outbox_action(&id))),
                        Err(error) => Response::err("store", error.to_string()),
                    },
                    _ => Response::err("bad_request", "expected action id"),
                }
            }
            "fake.runtime_status" => match self.assembly.engine.status().await {
                Ok(status) => {
                    let mut result = json!(status);
                    result["startup_cursor"] = json!(self.startup_cursor);
                    Response::ok(result)
                }
                Err(error) => Response::err("engine", error.to_string()),
            },
            "fake.drive" => self.drive(args).await,
            "fake.real_grant" => self.real_grant(args).await,
            "fake.real_execute" => self.real_execute(args).await,
            "fake.enqueue_merge_probe" => self.enqueue_merge_probe(args).await,
            "fake.real_approval" => {
                match (
                    self.fakes.approvals.as_ref(),
                    args.get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse::<ApprovalId>().ok()),
                ) {
                    (Some(approvals), Some(id)) => match approvals.get(&id).await {
                        Ok(approval) => Response::ok(json!(approval)),
                        Err(error) => Response::err("policy", error.to_string()),
                    },
                    _ => Response::err("bad_request", "expected real policy and approval id"),
                }
            }
            "fake.end_bootstrap" => {
                self.setup().bootstrap_over = true;
                Response::ok(json!({}))
            }
            "github.setup.begin" => self.setup_begin(args),
            "github.setup.status" => self.setup_status(&args),
            "settings.bootstrap_set" => self.bootstrap_set(&args),
            _ => return None,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    project_id: ProjectId,
    source_message_id: MessageId,
    title: String,
    user_text: String,
    /// 测试用：覆盖 worker 层级的 token 预算（0 表示预算耗尽）。
    #[serde(default)]
    budget_worker_tokens: Option<u64>,
}

/// Low 档交付单元；台账取 `levels.low` 的内置默认（尚无 settings 写入路径）。
async fn create_delivery(assembly: &Assembly, args: Value) -> Response {
    let args: CreateArgs = match serde_json::from_value(args) {
        Ok(args) => args,
        Err(error) => return Response::err("bad_request", error.to_string()),
    };
    let ResolvedSettings::Levels(levels) = builtin_resolved(SettingsResource::Levels) else {
        return Response::err("internal", "levels defaults have the wrong shape");
    };
    let config_rev = match assembly.store.snapshot().await {
        Ok(snapshot) => snapshot.config_rev(Some(&args.project_id)),
        Err(error) => return Response::err("store", error.to_string()),
    };
    let id = match assembly.ids.next() {
        Ok(id) => id,
        Err(error) => return Response::err("internal", error.to_string()),
    };
    let mut budget = budget_from_level(&levels.low);
    if let Some(tokens) = args.budget_worker_tokens {
        budget.limits.tokens_by_tier.insert(Tier::Worker, tokens);
    }
    let delivery = NewDelivery {
        id,
        project_id: args.project_id,
        title: args.title,
        level_cap: Level::Low,
        level_effective: Level::Low,
        origin: DeliveryOrigin::Conversation,
        source_message_id: args.source_message_id,
        user_text: args.user_text,
        issues: Vec::new(),
        budget,
        config_rev,
        actor: Actor::new(ActorKind::System, FAKE_ACTOR),
    };
    match assembly.engine.engine().create_delivery(delivery).await {
        Ok(id) => Response::ok(json!({ "delivery_id": DeliveryId::to_string(&id) })),
        Err(error) => Response::err("engine", error.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsArgs {
    #[serde(default)]
    after: u64,
}

/// 从 `after` 之后读至多 200 个事件（store `scan` 的单页上限）。
async fn events(assembly: &Assembly, args: Value) -> Response {
    let args: EventsArgs = match serde_json::from_value(args) {
        Ok(args) => args,
        Err(error) => return Response::err("bad_request", error.to_string()),
    };
    let after = match gantry_core::entities::Seq::from_wire(args.after) {
        Ok(seq) => seq,
        Err(error) => return Response::err("bad_request", error.to_string()),
    };
    let filter = EventFilter {
        project_ids: None,
        include: Vec::new(),
        exclude: Vec::new(),
    };
    match assembly.store.events().scan(after, &filter, 200).await {
        Ok(page) => Response::ok(json!({
            "head": page.head,
            "events": page.events,
        })),
        Err(error) => Response::err("store", error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// fake.drive：扮演 forge 执行器、GitHub 与审批设备（engine.md §12.2「端到端（替身）」）
// ---------------------------------------------------------------------------

/// 跨多次 `fake.drive` 调用保留的驱动状态（单个交付单元）。
#[derive(Default)]
struct DriveState {
    merged: bool,
    granted: Option<ApprovalId>,
}

impl FakeCommands {
    /// 推进一步，返回当前阶段：
    /// 1. 补丁提交后，按审批状态给 `merge_pr` 脚本化 policy 的判定；
    /// 2. 认领并完成排队的 Outbox 动作（`create_pr` 绑定 PR [`PR_NUMBER`]）；
    /// 3. 有待决审批时以设备身份授予；
    /// 4. 有 PR 后写入 PR 快照（合并后为 `merged`），并给执行体一个 `Tick(Settle)`。
    ///
    /// 引擎本身只经端口与事件被驱动，这里不调用执行体的内部入口。
    async fn drive(&self, args: Value) -> Response {
        let delivery: DeliveryId = match args
            .get("delivery_id")
            .and_then(Value::as_str)
            .map(str::parse)
        {
            Some(Ok(id)) => id,
            _ => return Response::err("bad_request", "expected {delivery_id}"),
        };
        let store = &self.fakes.store;
        let head = self
            .fakes
            .sandbox
            .jobs()
            .iter()
            .find_map(|job| self.fakes.repo.commit_record(job))
            .map(|record| record.commit_oid);
        let granted = self.drive_state().granted;
        if let (Some(head), Some(policy)) = (head, &self.fakes.policy) {
            let tx = match store.begin_write().await {
                Ok(tx) => tx,
                Err(error) => return Response::err("store", error.to_string()),
            };
            script_merge(policy, delivery, head, granted);
            if let Err(error) = tx.rollback().await {
                return Response::err("store", error.to_string());
            }
        }
        let done = match execute_outbox(store, head, self.fakes.real_executor.is_some()).await {
            Ok(done) => done,
            Err(error) => return Response::err("store", error),
        };
        if done.contains(&ForgeActionKind::MergePr) {
            self.drive_state().merged = true;
        }
        let state = match load_delivery_state(store, &delivery).await {
            Ok(Some(state)) => state,
            Ok(None) => return Response::err("not_found", "no such delivery"),
            Err(error) => return Response::err("engine", error.to_string()),
        };
        if state.delivery.stage == Stage::Closed && state.worktree.is_none() {
            return Response::ok(json!({ "stage": state.delivery.stage, "done": true }));
        }
        if granted.is_none() && self.fakes.policy.is_some() {
            let pending = state
                .approvals
                .iter()
                .find(|(_, approval)| approval.status == ApprovalStatus::Pending)
                .map(|(id, _)| *id);
            if let (Some(id), Some(head)) = (pending, head) {
                if let Err(error) = self.grant(delivery, id, head).await {
                    return Response::err("policy", error);
                }
                self.drive_state().granted = Some(id);
            }
        }
        if let (Some(_), Some(head)) = (&state.pr, head) {
            let merged = self.drive_state().merged;
            let pr = engine_cfg::snapshot(if merged { "merged" } else { "open" }, head);
            if self.fakes.approvals.is_some() {
                if let Err(error) = upsert_pr(store, &self.fakes.forge, pr.clone()).await {
                    return Response::err("store", error.to_string());
                }
            }
            if let Some((_, github)) = &self.fakes.real_executor {
                // FakeForge 和 HTTP FakeGitHub 是独立事实源；create_pr 仍由替身完成，
                // 显式同步同一个 PR 号和 head，真实执行器只处理 merge_pr。
                if github.pull("acme/web", engine_cfg::PR_NUMBER).is_none() {
                    for _ in 1..engine_cfg::PR_NUMBER {
                        github.insert_issue("acme/web", "number placeholder");
                    }
                    github.set_branch("acme/web", "gantry/test", &head.to_string());
                    let (login, id) = github.bot();
                    assert_eq!(
                        github.insert_pull(
                            "acme/web",
                            "gantry/test",
                            "main",
                            (&login, id),
                            "fixture"
                        ),
                        engine_cfg::PR_NUMBER
                    );
                }
            }
            *self
                .fakes
                .snapshots
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pr);
            // 生产里快照变化随 forge.* 事件路由进来；替身直接投一个 Settle。
            (self.assembly.engine.mailbox())(
                delivery,
                Input::Tick(TickReason::Settle),
                InputIdentity::Tick(TickReason::Settle),
            );
        }
        Response::ok(json!({ "stage": state.delivery.stage, "done": false }))
    }

    fn drive_state(&self) -> std::sync::MutexGuard<'_, DriveState> {
        self.drive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn real_execute(&self, args: Value) -> Response {
        let Some((executor, github)) = &self.fakes.real_executor else {
            return Response::err("forge", "real executor is not enabled");
        };
        if args.get("move_head").and_then(Value::as_bool) == Some(true) {
            github.set_pull_head("acme/web", engine_cfg::PR_NUMBER, engine_cfg::BASE);
        }
        let result = executor.run_once().await;
        let calls = github
            .recorded_calls()
            .iter()
            .filter(|call| call.method == "PUT" && call.path.ends_with("/merge"))
            .count();
        Response::ok(
            json!({"result": format!("{result:?}"), "merge_calls": calls, "pull": github.pull("acme/web", engine_cfg::PR_NUMBER)}),
        )
    }

    async fn enqueue_merge_probe(&self, args: Value) -> Response {
        use gantry_core::entities::{MergeMethod, Priority};
        use gantry_core::forge::{ActionPayload, MergeMode, MergePrPayload};
        use gantry_core::ports::{EnqueueRequest, EnqueuedBy, EnqueuedByModule, OutboxPort};
        if self.fakes.real_executor.is_none() {
            return Response::err("forge", "real executor is not enabled");
        }
        let Some(id) = args
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<ApprovalId>().ok())
        else {
            return Response::err("bad_request", "expected approval id");
        };
        let Some(head) = self
            .fakes
            .sandbox
            .jobs()
            .iter()
            .find_map(|job| self.fakes.repo.commit_record(job))
            .map(|record| record.commit_oid)
        else {
            return Response::err("forge", "head not ready");
        };
        let ctx = ActorCtx {
            actor: Actor::new(ActorKind::Engine, "test-probe"),
            device: None,
            role: None,
            client_profile: None,
            request_id: "01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap(),
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        };
        let req = EnqueueRequest {
            project_id: super::project_id(),
            delivery_id: "dlv_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap(),
            payload: ActionPayload::MergePr(MergePrPayload {
                repo: engine_cfg::repo_slug(),
                pull_number: engine_cfg::PR_NUMBER,
                expected_head_sha: head,
                merge_method: MergeMethod::Squash,
                mode: MergeMode::Auto,
                commit_title: None,
                commit_message: None,
            }),
            requires_approval: Some(id),
            depends_on: vec![],
            priority: Priority::High,
            not_before: None,
            epoch: if args.get("second").and_then(Value::as_bool) == Some(true) {
                2
            } else {
                1
            },
            enqueued_by: EnqueuedBy {
                module: EnqueuedByModule::Engine,
                causation_id: None,
            },
        };
        match self.fakes.forge.enqueue(req, &ctx).await {
            Ok(action) => Response::ok(json!({"id": action.action.id})),
            Err(error) => Response::err("forge", error.to_string()),
        }
    }

    async fn real_grant(&self, args: Value) -> Response {
        let Some(approvals) = &self.fakes.approvals else {
            return Response::err("policy", "real policy is not enabled");
        };
        let Some(Ok(id)) = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::parse::<ApprovalId>)
        else {
            return Response::err("bad_request", "expected approval id");
        };
        // 固定公开测试密钥，只在测试进程的内存中使用。
        let key = SigningKey::from_slice(&[7; 32]).expect("test key");
        let device = DeviceKey {
            device_id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().expect("fixture"),
            role: DeviceRole::Approver,
            profile: ClientProfile::Desktop,
            status: DeviceStatus::Active,
            approval_pubkey_fpr: sha256(key.verifying_key().to_encoded_point(false).as_bytes()),
            verifying_key: *key.verifying_key(),
        };
        let challenge = match approvals.challenge(&id, &device).await {
            Ok(challenge) => challenge,
            Err(error) => return Response::err("challenge", error.to_string()),
        };
        let signer = if args.get("invalid").and_then(Value::as_bool) == Some(true) {
            SigningKey::from_slice(&[8; 32]).expect("test key")
        } else {
            key
        };
        let signature: Signature =
            signer.sign(&challenge.payload.signing_bytes().expect("payload"));
        let ctx = ActorCtx {
            actor: Actor::new(ActorKind::Device, device.device_id.to_string()),
            device: Some(device.device_id),
            role: Some(device.role),
            client_profile: Some(device.profile),
            request_id: "01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().expect("fixture"),
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        };
        match approvals
            .grant(
                GrantRequest {
                    approval_id: id,
                    device,
                    nonce: challenge.payload.nonce.to_string(),
                    signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
                    current_subject: None,
                },
                &ctx,
            )
            .await
        {
            Ok(approval) => Response::ok(json!({"id": approval.id, "status": approval.status})),
            Err(error) => Response::err("policy", error.to_string()),
        }
    }

    /// 设备签名授予：policy 状态置 granted，并写 `approval.granted`（生产里由 policy 在同一事务写）。
    async fn grant(
        &self,
        delivery: DeliveryId,
        id: ApprovalId,
        head: GitOid,
    ) -> Result<(), String> {
        let gate = self
            .fakes
            .store
            .begin_write()
            .await
            .map_err(|error| error.to_string())?;
        let policy = self.fakes.policy.as_ref().expect("fake policy");
        policy.grant(&id).map_err(|error| error.to_string())?;
        script_merge(policy, delivery, head, Some(id));
        gate.rollback().await.map_err(|error| error.to_string())?;
        let digest = sha256(b"approval");
        let data = ApprovalGrantedData {
            approval_id: id,
            device_id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().expect("fixture"),
            approval_pubkey_fpr: digest,
            payload_sha256: digest,
        };
        let event = NewEvent {
            ty: EventType::ApprovalGranted,
            subject: Subject::parse(&id.to_string()).map_err(|error| error.to_string())?,
            project_id: Some(super::project_id()),
            actor: Actor::new(ActorKind::System, "policy"),
            causation_id: None,
            correlation_id: Some(delivery.to_string()),
            data: serde_json::to_value(data).map_err(|error| error.to_string())?,
            dedup_key: None,
        };
        event.validate().map_err(|error| error.to_string())?;
        let store = &self.fakes.store;
        let mut tx = store
            .begin_write()
            .await
            .map_err(|error| error.to_string())?;
        tx.append(event).await.map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// FakeForge 的直接写绕过 Store 写门；持租约期间不得开始另一个写事务。
pub(crate) async fn upsert_pr(
    store: &gantry_store::Store,
    forge: &gantry_fakes::forge::FakeForge,
    pr: gantry_core::entities::PullRequestRef,
) -> Result<(), gantry_store::StoreError> {
    let gate = store.begin_write().await?;
    forge.upsert_pr(super::project_id(), "gantry/test", pr);
    gate.rollback().await
}

/// merge_pr 需要审批（I8）：未授予时 `NeedsApproval`，授予后 `Allow` 带审批 ID。
fn script_merge(
    policy: &FakePolicy,
    delivery: DeliveryId,
    head: GitOid,
    granted: Option<ApprovalId>,
) {
    let config_rev = Seq::new(1).expect("seq 1");
    let decision = match granted {
        None => ActionDecision::NeedsApproval {
            spec: ApprovalSpec {
                action: ApprovalAction::Forge(ForgeActionKind::MergePr),
                target: serde_json::from_value(json!({
                    "kind": "pr", "repo": "acme/web", "number": engine_cfg::PR_NUMBER,
                }))
                .expect("fixture"),
                subject_digest: SubjectDigest::git(head),
                delivery_id: Some(delivery),
                ttl: Duration::from_secs(3600),
            },
            pending: None,
            config_rev,
        },
        Some(id) => ActionDecision::Allow {
            action_id: PolicyActionId::from(ForgeActionKind::MergePr),
            approval: Some(id),
            config_rev,
        },
    };
    policy.script_decision(ForgeActionKind::MergePr, decision);
}

/// 扮演 forge 执行器：认领并完成全部排队动作，返回完成的动作种类。
async fn execute_outbox(
    store: &gantry_store::Store,
    head: Option<GitOid>,
    leave_merge: bool,
) -> Result<Vec<ForgeActionKind>, String> {
    let text = |error: gantry_store::StoreError| error.to_string();
    let mut done = Vec::new();
    loop {
        let mut tx = store.begin_write().await.map_err(text)?;
        let claimed = tx
            .outbox()
            .claim(ClaimOpts {
                lease: Duration::from_secs(60),
                exclude_deliveries: BTreeSet::new(),
            })
            .await
            .map_err(text)?;
        let Some(claimed) = claimed else {
            tx.rollback().await.map_err(text)?;
            return Ok(done);
        };
        let action = claimed.action;
        if leave_merge && action.kind == ForgeActionKind::MergePr {
            tx.rollback().await.map_err(text)?;
            return Ok(done);
        }
        let pr = match (action.kind, head) {
            (ForgeActionKind::CreatePr, Some(head)) => Some(GithubPrRef {
                number: engine_cfg::PR_NUMBER,
                node_id: "PR_x".into(),
                head_sha: head,
            }),
            _ => None,
        };
        let github_ref = GithubRef {
            bot: None,
            pr,
            marker: None,
            recorded_at: None,
        };
        tx.outbox()
            .complete(&action.id, github_ref)
            .await
            .map_err(text)?;
        tx.commit().await.map_err(text)?;
        done.push(action.kind);
    }
}
