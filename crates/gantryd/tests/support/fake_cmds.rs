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

use gantry_core::entities::{Approval, ApprovalStatus, ForgeActionKind, Seq, Stage};
use gantry_core::ids::ApprovalId;
use gantry_engine::decide::{Input, InputIdentity, TickReason};
use gantry_engine::engine::load_delivery_state;

use super::engine_cfg;

use gantry_core::ids::{Clock, Entropy};
use gantry_deploy::manifest::Stage as ManifestStage;
use gantryd::assembly::{OsEntropy, SystemClock};

use super::FakeHandles;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use gantry_core::entities::DevicePlatform;
use gantry_core::ports::{ActorCtx, ClientProfile};
use gantry_pki::admin::{DeviceAdmin, RevokeOutcome};
use gantry_pki::directory::DeviceDirectory;
use gantry_policy::{
    service::{DeviceKey, GrantRequest},
    ApprovalService,
};
use gantry_store::Page;
use gantryd::assembly::Assembly;
use gantryd::control::{CommandSet, Response};
use p256::ecdsa::{signature::Signer as _, Signature, SigningKey};

/// `fake.*` 命令的固定执行者；与真实设备 / 主机命令区分开。
const FAKE_ACTOR: &str = "gantryd_fake";
/// 未登记的设备 id（格式合法、目录里不存在），用于负向对照。
const UNREGISTERED_DEVICE_ID: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";

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
            "fake.challenge" => self.challenge(args).await,
            "fake.grant_submission" => self.grant_submission(args).await,
            "fake.device_info" => self.device_info().await,
            "fake.revoke_device" => self.revoke_device().await,
            "fake.real_execute" => self.real_execute(args).await,
            "fake.enqueue_merge_probe" => self.enqueue_merge_probe(args).await,
            "fake.real_approval" => {
                match args
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<ApprovalId>().ok())
                {
                    Some(id) => match self.fakes.approvals.get(&id).await {
                        Ok(approval) => Response::ok(json!(approval)),
                        Err(error) => Response::err("policy", error.to_string()),
                    },
                    None => Response::err("bad_request", "expected approval id"),
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
    granted: Option<ApprovalId>,
    /// 已为哪些 head 注入过成功的 CI 检查（测试 GitHub 的完成事件）。
    ci: BTreeSet<String>,
}

impl FakeCommands {
    /// 推进一步，返回当前阶段：
    /// 1. 真实 forge 执行器处理排队的 Outbox 动作（含真实 git push 与 GitHub REST）；
    /// 2. 有 PR 后由真实 Dispatcher 回读 FakeGitHub：写入快照并发布 `forge.synced`；
    /// 3. `auto_approve` 时以测试设备密钥授予待决审批（challenge + 签名 + grant）；
    /// 4. 给执行体一个 `Tick(Settle)`。
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
        // 手动审批模式下把 merge_pr 留给测试显式驱动（`fake.real_execute`），
        // 这样可以在执行前改变 live head 验证 precondition。
        // 真实执行器一次处理一个动作；遇到重试 / 失败 / 取消就停下，等下一次驱动。
        loop {
            if !self.fakes.auto_approve && self.merge_pending(&delivery).await {
                break;
            }
            // Git 与 REST 夹具只从裸远端的实际 ref 同步（S3a 决定）。
            self.sync_bare_refs();
            match self.fakes.executor.run_once().await {
                Ok(Some(step)) => match step.outcome {
                    gantry_forge::StepOutcome::Succeeded => continue,
                    gantry_forge::StepOutcome::Requeued
                    | gantry_forge::StepOutcome::Failed { .. }
                    | gantry_forge::StepOutcome::Cancelled => break,
                },
                Ok(None) => break,
                Err(error) => return Response::err("forge", error.to_string()),
            }
        }
        self.ensure_ci_green();
        let state = match load_delivery_state(&self.fakes.store, &delivery).await {
            Ok(Some(state)) => state,
            Ok(None) => return Response::err("not_found", "no such delivery"),
            Err(error) => return Response::err("engine", error.to_string()),
        };
        if state.delivery.stage == Stage::Closed && state.worktree.is_none() {
            return Response::ok(json!({ "stage": state.delivery.stage, "done": true }));
        }
        if let Some(pr) = &state.pr {
            let report = self
                .fakes
                .dispatcher
                .reconcile(
                    gantry_core::ports::ReconcileTarget::Pr {
                        project: super::project_id(),
                        number: pr.number,
                    },
                    gantry_core::forge::DriftDetectedBy::OnDemand,
                )
                .await;
            if let Err(error) = report {
                return Response::err("forge", error.to_string());
            }
            let granted = self.drive_state().granted;
            if self.fakes.auto_approve && granted.is_none() {
                let pending = state
                    .approvals
                    .iter()
                    .find(|(_, approval)| approval.status == ApprovalStatus::Pending)
                    .map(|(id, _)| *id);
                if let Some(id) = pending {
                    if let Err(error) = self.grant(&id, false).await {
                        return Response::err("policy", error);
                    }
                    self.drive_state().granted = Some(id);
                }
            }
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

    /// 是否有待执行的 `merge_pr`（手动审批模式下留给显式驱动）。
    async fn merge_pending(&self, delivery: &DeliveryId) -> bool {
        load_delivery_state(&self.fakes.store, delivery)
            .await
            .ok()
            .flatten()
            .is_some_and(|state| {
                let queued = state.outbox.values().any(|row| {
                    row.kind == ForgeActionKind::MergePr
                        && row.status == gantry_core::entities::OutboxStatus::Queued
                });
                // 审批已授予但引擎尚未入队 merge：也先不执行，避免抢占后续 merge。
                let granted = state.approvals.values().any(|approval| {
                    approval.action
                        == gantry_core::entities::ApprovalAction::Forge(ForgeActionKind::MergePr)
                        && approval.status == gantry_core::entities::ApprovalStatus::Granted
                });
                queued || granted
            })
    }

    /// 模拟 CI 完成：每个非默认分支的 head 注入一条成功的 check run（一次）。
    fn ensure_ci_green(&self) {
        let bare = self.fakes.bare.to_str().expect("bare path");
        let output = std::process::Command::new("git")
            .args([
                "--git-dir",
                bare,
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                "refs/heads",
            ])
            .output()
            .expect("read bare remote refs");
        assert!(output.status.success(), "git for-each-ref failed");
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((branch, sha)) = line.split_once(' ') else {
                continue;
            };
            if branch == "main" {
                continue;
            }
            if !self.drive_state().ci.insert(sha.to_owned()) {
                continue;
            }
            self.fakes.github.add_check_run(
                "acme/web",
                sha,
                json!({
                    "id": 1,
                    "name": "ci",
                    "status": "completed",
                    "conclusion": "success",
                    "head_sha": sha,
                    "check_suite": {"id": 1},
                    "app": {"id": 1, "slug": "ci"},
                }),
            );
        }
    }

    /// 把裸远端的实际分支 ref 同步到 FakeGitHub；不按 Outbox 期望 payload 伪造成功。
    fn sync_bare_refs(&self) {
        let bare = self.fakes.bare.to_str().expect("bare path");
        let output = std::process::Command::new("git")
            .args([
                "--git-dir",
                bare,
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                "refs/heads",
            ])
            .output()
            .expect("read bare remote refs");
        assert!(
            output.status.success(),
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some((branch, sha)) = line.split_once(' ') {
                self.fakes.github.set_branch("acme/web", branch, sha);
            }
        }
    }

    async fn real_execute(&self, args: Value) -> Response {
        self.sync_bare_refs();
        let number = match args
            .get("delivery_id")
            .and_then(Value::as_str)
            .map(str::parse::<DeliveryId>)
        {
            Some(Ok(delivery)) => match load_delivery_state(&self.fakes.store, &delivery)
                .await
                .ok()
                .flatten()
                .and_then(|state| state.pr)
            {
                Some(pr) => pr.number,
                None => return Response::err("forge", "PR is not bound"),
            },
            _ => engine_cfg::PR_NUMBER,
        };
        if args.get("move_head").and_then(Value::as_bool) == Some(true) {
            self.fakes
                .github
                .set_pull_head("acme/web", number, &engine_cfg::base().to_string());
        }
        let result = self.fakes.executor.run_once().await;
        let calls = self
            .fakes
            .github
            .recorded_calls()
            .iter()
            .filter(|call| {
                call["method"] == "PUT"
                    && call["path"]
                        .as_str()
                        .is_some_and(|path| path.ends_with("/merge"))
            })
            .count();
        Response::ok(
            json!({"result": format!("{result:?}"), "merge_calls": calls, "pull": self.fakes.github.pull("acme/web", number)}),
        )
    }

    async fn enqueue_merge_probe(&self, args: Value) -> Response {
        use gantry_core::entities::{MergeMethod, Priority};
        use gantry_core::forge::{ActionPayload, MergeMode, MergePrPayload};
        use gantry_core::ports::{EnqueueRequest, EnqueuedBy, EnqueuedByModule, OutboxPort};
        let Some(id) = args
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<ApprovalId>().ok())
        else {
            return Response::err("bad_request", "expected approval id");
        };
        let Some(Ok(delivery)) = args
            .get("delivery_id")
            .and_then(Value::as_str)
            .map(str::parse::<DeliveryId>)
        else {
            return Response::err("bad_request", "expected delivery_id");
        };
        let state = match load_delivery_state(&self.fakes.store, &delivery).await {
            Ok(Some(state)) => state,
            Ok(None) => return Response::err("not_found", "no such delivery"),
            Err(error) => return Response::err("engine", error.to_string()),
        };
        let Some(pr) = state.pr else {
            return Response::err("forge", "PR is not bound");
        };
        // 探针前强制回读一次：不依赖引擎是否已经驱动过这一轮快照。
        if let Err(error) = self
            .fakes
            .dispatcher
            .reconcile(
                gantry_core::ports::ReconcileTarget::Pr {
                    project: super::project_id(),
                    number: pr.number,
                },
                gantry_core::forge::DriftDetectedBy::OnDemand,
            )
            .await
        {
            return Response::err("forge", error.to_string());
        }
        let snapshot = match self
            .assembly
            .forge
            .pull_request(&super::project_id(), pr.number)
            .await
        {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Response::err("forge", "PR snapshot not ready"),
            Err(error) => return Response::err("forge", error.to_string()),
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
                pull_number: snapshot.number,
                expected_head_sha: snapshot.head_sha,
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
        let Some(Ok(id)) = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::parse::<ApprovalId>)
        else {
            return Response::err("bad_request", "expected approval id");
        };
        let invalid = args.get("invalid").and_then(Value::as_bool) == Some(true);
        let device = if args.get("device").and_then(Value::as_str) == Some("unregistered") {
            UNREGISTERED_DEVICE_ID.parse().expect("fixture device id")
        } else {
            self.fakes.test_device
        };
        match self.grant_for(&device, &id, invalid).await {
            Ok(approval) => Response::ok(json!({"id": approval.id, "status": approval.status})),
            Err(error) => Response::err("policy", error),
        }
    }

    /// 实时读取设备目录与登记公钥；不缓存角色、状态或公钥，吊销立即生效。
    async fn device_key(&self, id: &gantry_core::ids::DeviceId) -> Result<DeviceKey, String> {
        let directory = self.fakes.pki.directory();
        let device = directory
            .get(id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("device not found")?;
        let verifying_key = directory
            .verifying_key(id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("device approval key is not available")?;
        let approval_pubkey_fpr = device
            .approval_pubkey_fpr
            .ok_or("device has no approval key")?;
        Ok(DeviceKey {
            device_id: device.id,
            role: device.role,
            profile: match device.platform {
                DevicePlatform::Windows | DevicePlatform::Macos => ClientProfile::Desktop,
                DevicePlatform::Ios => ClientProfile::Mobile,
            },
            status: device.status,
            approval_pubkey_fpr,
            verifying_key,
        })
    }

    /// 返回真实 `approvals.challenge` 的 payload，供测试用自己的审批私钥签名。
    async fn challenge(&self, args: Value) -> Response {
        let Some(Ok(id)) = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::parse::<ApprovalId>)
        else {
            return Response::err("bad_request", "expected approval id");
        };
        let device = self.device_key(&self.fakes.test_device).await;
        let device = match device {
            Ok(device) => device,
            Err(error) => return Response::err("policy", error),
        };
        match self.fakes.approvals.challenge(&id, &device).await {
            Ok(challenge) => Response::ok(json!({
                "payload": challenge.payload,
                "nonce": challenge.payload.nonce.to_string(),
            })),
            Err(error) => Response::err("policy", error.to_string()),
        }
    }

    /// 提交测试已经签好的 challenge：提交时重新读取设备，不复用 challenge 时的快照。
    async fn grant_submission(&self, args: Value) -> Response {
        let Some(Ok(id)) = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::parse::<ApprovalId>)
        else {
            return Response::err("bad_request", "expected approval id");
        };
        let Some(nonce) = args.get("nonce").and_then(Value::as_str) else {
            return Response::err("bad_request", "expected nonce");
        };
        let Some(signature) = args.get("signature").and_then(Value::as_str) else {
            return Response::err("bad_request", "expected signature");
        };
        match self
            .submit_grant(&self.fakes.test_device, &id, nonce, signature)
            .await
        {
            Ok(approval) => Response::ok(json!({"id": approval.id, "status": approval.status})),
            Err(error) => Response::err("policy", error),
        }
    }

    /// 用测试设备自己的审批私钥完成 challenge + 签名 + grant；`invalid` 用另一把密钥，
    /// 走真实 `policy` 验签失败。
    async fn grant_for(
        &self,
        device_id: &gantry_core::ids::DeviceId,
        id: &ApprovalId,
        invalid: bool,
    ) -> Result<Approval, String> {
        let device = self.device_key(device_id).await?;
        let challenge = self
            .fakes
            .approvals
            .challenge(id, &device)
            .await
            .map_err(|error| error.to_string())?;
        let signer = if invalid {
            SigningKey::from_slice(&[8; 32]).expect("wrong test key")
        } else {
            super::test_signing_key()
        };
        let signature: Signature =
            signer.sign(&challenge.payload.signing_bytes().expect("payload"));
        let signature = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        // 新请求重新读取设备：challenge 之后被吊销时，这里拿不到审批公钥。
        self.submit_grant(
            device_id,
            id,
            &challenge.payload.nonce.to_string(),
            &signature,
        )
        .await
    }

    /// 真正提交 grant 的唯一路径：每次都重新从 `DeviceDirectory` 读取设备与公钥。
    async fn submit_grant(
        &self,
        device_id: &gantry_core::ids::DeviceId,
        id: &ApprovalId,
        nonce: &str,
        signature: &str,
    ) -> Result<Approval, String> {
        let device = self.device_key(device_id).await?;
        let ctx = ActorCtx {
            actor: Actor::new(ActorKind::Device, device.device_id.to_string()),
            device: Some(device.device_id),
            role: Some(device.role),
            client_profile: Some(device.profile),
            request_id: self
                .assembly
                .ids
                .next_ulid()
                .map_err(|error| error.to_string())?,
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        };
        self.fakes
            .approvals
            .grant(
                GrantRequest {
                    approval_id: *id,
                    device,
                    nonce: nonce.to_owned(),
                    signature: signature.to_owned(),
                    current_subject: None,
                },
                &ctx,
            )
            .await
            .map_err(|error| error.to_string())
    }

    async fn grant(&self, id: &ApprovalId, invalid: bool) -> Result<Approval, String> {
        self.grant_for(&self.fakes.test_device, id, invalid).await
    }

    /// 测试设备的实时身份与登记指纹，以及目录里的设备总数（防止第二次配对冒充 approver）。
    async fn device_info(&self) -> Response {
        let directory = self.fakes.pki.directory();
        let device = match directory.get(&self.fakes.test_device).await {
            Ok(Some(device)) => device,
            Ok(None) => return Response::err("pki", "test device not found"),
            Err(error) => return Response::err("pki", error.to_string()),
        };
        let mut count = 0;
        let mut cursor = None;
        loop {
            let page = match directory
                .list(&Page {
                    limit: 200,
                    cursor,
                    ..Page::default()
                })
                .await
            {
                Ok(page) => page,
                Err(error) => return Response::err("pki", error.to_string()),
            };
            count += page.items.len();
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Response::ok(json!({
            "device": device,
            "count": count,
            "signing_pubkey_fpr": super::test_signing_fingerprint(),
        }))
    }

    /// 用真实的 `StoreAdmin::revoke` 吊销测试设备；吊销日志写到 scratch 内的本地与备份目标。
    async fn revoke_device(&self) -> Response {
        let mut tx = match self.fakes.store.begin_write().await {
            Ok(tx) => tx,
            Err(error) => return Response::err("store", error.to_string()),
        };
        let actor = Actor::new(ActorKind::System, "gantryd_test");
        let outcome = self
            .fakes
            .pki
            .admin()
            .revoke(&mut tx, &self.fakes.test_device, &actor)
            .await;
        match outcome {
            Ok(outcome) => match tx.commit().await {
                Ok(_) => Response::ok(json!({
                    "device_id": self.fakes.test_device.to_string(),
                    "revoked": matches!(outcome, RevokeOutcome::Revoked(_)),
                })),
                Err(error) => Response::err("store", error.to_string()),
            },
            Err(error) => {
                let _ = tx.rollback().await;
                Response::err("pki", error.to_string())
            }
        }
    }
}
