//! `fakes` 装配专用的控制命令：进程级测试从进程外驱动替身、观察事件日志。
//!
//! 这些命令替代尚不存在的入口（api 的 `POST /v1/deliveries` 等），只在
//! `--profile fakes` 下可用；真实装配不编译、不注册它们。

use gantry_core::entities::{Actor, ActorKind, DeliveryOrigin, SettingsResource};
use gantry_core::ids::{DeliveryId, MessageId, ProjectId};
use gantry_core::level::Level;
use gantry_core::settings::defaults::builtin_resolved;
use gantry_core::settings::resource::ResolvedSettings;
use gantry_engine::{budget_from_level, NewDelivery};
use gantry_store::EventFilter;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::assembly::Assembly;
use crate::control::Response;

/// `fake.*` 命令的固定执行者；与真实设备 / 主机命令区分开。
const FAKE_ACTOR: &str = "gantryd_fake";

pub async fn handle(assembly: &Assembly, cmd: &str, args: Value) -> Response {
    match cmd {
        "fake.create_delivery" => create_delivery(assembly, args).await,
        "fake.events" => events(assembly, args).await,
        other => Response::err("unknown_command", format!("unknown command {other:?}")),
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
