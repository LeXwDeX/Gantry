//! 替身装配专用的控制命令 `fake.*`：进程级测试从进程外驱动替身、观察事件日志。
//!
//! 这些命令替代还没接线的入口（api 的 `POST /v1/deliveries` 等），只注册在
//! 测试进程里，服务端二进制里没有。

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, SettingsResource};
use gantry_core::ids::{DeliveryId, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::defaults::builtin_resolved;
use gantry_core::settings::resource::ResolvedSettings;
use gantry_engine::{budget_from_level, NewDelivery};
use gantry_store::EventFilter;
use serde::Deserialize;
use serde_json::{json, Value};

use std::sync::{Arc, Mutex};

use gantry_core::ids::{Clock, Entropy};
use gantry_deploy::manifest::Stage;
use gantryd::assembly::{OsEntropy, SystemClock};

use gantryd::assembly::Assembly;
use gantryd::control::{CommandSet, Response};

/// `fake.*` 命令的固定执行者；与真实设备 / 主机命令区分开。
const FAKE_ACTOR: &str = "gantryd_fake";

pub struct FakeCommands {
    pub assembly: Arc<Assembly>,
    setup: Mutex<SetupState>,
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
    pub fn new(assembly: Arc<Assembly>) -> Self {
        Self {
            assembly,
            setup: Mutex::new(SetupState::default()),
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
        let permissions: serde_json::Map<String, Value> = Stage::M1
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
                "events": Stage::M1.events(),
            },
        }))
    }

    fn bootstrap_set(&self, args: &Value) -> Response {
        if self.setup().bootstrap_over {
            return not_bootstrap_period();
        }
        let resource = args.get("resource").and_then(Value::as_str);
        let values = args.get("values").and_then(Value::as_object);
        match (resource, values) {
            (Some(resource), Some(values)) => Response::ok(json!({
                "resource": resource,
                "applied": values.keys().collect::<Vec<_>>(),
            })),
            _ => Response::err("bad_request", "expected {resource, values}"),
        }
    }
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
        budget: budget_from_level(&levels.low),
        config_rev,
        actor: Actor::new(ActorKind::System, FAKE_ACTOR),
    };
    match assembly.engine.create_delivery(delivery).await {
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
