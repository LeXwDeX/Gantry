//! 控制路由 #8 #43–#45、发消息 #12 与幂等键（本文 §12.2「控制路由」「分页 / 幂等」
//! 「统一错误 / 校验」中的 M1 条目）。
//!
//! 下游一律用 `gantry-fakes` 的指定替身按脚本返回（docs/milestones.md「替身」）；
//! 所属模块按 `ctx.dedup_key` 追加的事件用存储夹具预置，以断言 `X-Event-Seq`。

mod support;

use std::sync::Arc;

use gantry_api::idempotency::{fingerprint, Begin, IdempotencyStore, MemoryIdempotency};
use gantry_core::entities::{
    Delivery, Device, DevicePlatform, DeviceRole, Message, MessageRole, Node, Ref, Seq, Stage,
    StageStatus,
};
use gantry_core::events::EventType;
use gantry_core::ids::{ConversationId, DeliveryId, NodeId, ProjectId, Ulid};
use gantry_core::ports::context::{ActorCtx, ModuleError, StoreError};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_core::ports::ClientProfile;
use gantry_core::testing::builders;
use gantry_fakes::conversation::ConversationCall;
use gantry_fakes::engine::{DeliveryOp, EngineCall};
use http::Method;
use serde_json::{json, Value};
use support::doubles::{fixture_data, record};
use support::*;

const KEY: &str = "01JB7P9Z3K5R8T2X6N1Q4V7WYA";
const KEY2: &str = "01JB7P9Z3K5R8T2X6N1Q4V7WYB";

fn operator(h: &Harness) -> Device {
    let d = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(d.clone());
    d
}

fn dedup(d: &Device, key: &str) -> String {
    format!("api:{}:{key}", d.id)
}

fn delivery_with(status: StageStatus) -> Delivery {
    let mut d = builders::delivery();
    d.stage = Stage::Implement;
    d.stage_status = status;
    d
}

/// 存储夹具：交付单元的创建事件（delivery.* 事件要求先有投影行）。
async fn created(h: &Harness, d: &Delivery) {
    let id = d.id.to_string();
    let data = fixture_data("delivery.created");
    record(
        &h.store,
        EventType::DeliveryCreated,
        &id,
        Some(d.project_id),
        data,
        None,
    )
    .await;
}

fn engine_ctx(h: &Harness, n: usize) -> ActorCtx {
    match &h.engine.calls()[n] {
        EngineCall::Pause { ctx, .. }
        | EngineCall::Resume { ctx, .. }
        | EngineCall::Abandon { ctx, .. }
        | EngineCall::RetryNode { ctx, .. } => ctx.clone(),
        other => panic!("unexpected call {other:?}"),
    }
}

fn post_calls(h: &Harness) -> Vec<(ActorCtx, NewUserMessage)> {
    h.conversation
        .inner
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            ConversationCall::PostUserMessage { ctx, message, .. } => Some((ctx, message)),
            _ => None,
        })
        .collect()
}

fn ctl(d: &Device, path: &str, key: Option<&str>) -> gantry_api::RequestHead {
    let head = json_req(Method::POST, path, d);
    match key {
        Some(key) => head.header("idempotency-key", key),
        None => head,
    }
}

#[tokio::test]
async fn control_routes_require_key_role_and_desktop() {
    let h = harness(None);
    let op = operator(&h);
    let viewer = device(2, DevicePlatform::Macos, DeviceRole::Viewer);
    let ios = device(3, DevicePlatform::Ios, DeviceRole::Operator);
    h.devices.insert(viewer.clone());
    h.devices.insert(ios.clone());
    let d = builders::delivery();
    let n = builders::node();
    for path in [
        format!("/v1/deliveries/{}/pause", d.id),
        format!("/v1/deliveries/{}/resume", d.id),
        format!("/v1/nodes/{}/retry", n.id),
    ] {
        let resp = send(&h, ctl(&op, &path, None), "{}").await;
        assert_problem(&resp, 400, "idempotency_key_required");
        let resp = send(&h, ctl(&viewer, &path, Some(KEY)), "{}").await;
        let b = assert_problem(&resp, 403, "forbidden_role");
        assert_eq!(b["required_role"], "operator");
        let resp = send(&h, ctl(&ios, &path, Some(KEY)), "{}").await;
        assert_problem(&resp, 403, "forbidden_profile");
    }
    assert!(h.engine.calls().is_empty());
}

#[tokio::test]
async fn pause_passes_actor_ctx_and_reports_the_written_event_seq() {
    let h = harness(None);
    let op = operator(&h);
    let d = delivery_with(StageStatus::Paused);
    created(&h, &d).await;
    h.engine.script_delivery(DeliveryOp::Pause(d.id), d.clone());
    let paused = record(
        &h.store,
        EventType::DeliveryPaused,
        &d.id.to_string(),
        Some(d.project_id),
        json!({"by": op.id, "status_before": "running"}),
        Some(dedup(&op, KEY)),
    )
    .await;
    // 夹具之后还有别的事件：X-Event-Seq 必须指向本次写入，而不是 head
    let later = builders::message().id.to_string();
    let data = fixture_data("message.posted");
    record(&h.store, EventType::MessagePosted, &later, None, data, None).await;

    let path = format!("/v1/deliveries/{}/pause", d.id);
    let resp = send(&h, ctl(&op, &path, Some(KEY)), "{}").await;
    assert_eq!(resp.status(), 200);
    let b = body(&resp);
    validate("Delivery", &b);
    assert_eq!(b["stage_status"], "paused");
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        paused.as_u64().to_string()
    );

    let ctx = engine_ctx(&h, 0);
    assert_eq!(ctx.actor.kind, gantry_core::entities::ActorKind::Device);
    assert_eq!(ctx.actor.id, op.id.to_string());
    assert_eq!(ctx.device, Some(op.id));
    assert_eq!(ctx.role, Some(DeviceRole::Operator));
    assert_eq!(ctx.client_profile, Some(ClientProfile::Desktop));
    let request_id: Ulid = header(&resp, "x-request-id").unwrap().parse().unwrap();
    assert_eq!(ctx.request_id, request_id);
    assert_eq!(ctx.idempotency_key.as_deref(), Some(KEY));
    assert_eq!(ctx.dedup_key, Some(dedup(&op, KEY)));
    assert_eq!(ctx.causation_id, None);
}

#[tokio::test]
async fn target_state_noop_reports_head_and_other_controls_route_to_the_engine() {
    let h = harness(None);
    let op = operator(&h);
    let d = delivery_with(StageStatus::Paused);
    created(&h, &d).await;
    let head = record(
        &h.store,
        EventType::DeliveryPaused,
        &d.id.to_string(),
        Some(d.project_id),
        json!({"by": "x", "status_before": "running"}),
        None,
    )
    .await;
    // 已在目标状态：没有该 dedup_key 的事件，X-Event-Seq = head
    h.engine.script_delivery(DeliveryOp::Pause(d.id), d.clone());
    let path = format!("/v1/deliveries/{}/pause", d.id);
    let resp = send(&h, ctl(&op, &path, Some(KEY)), "").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        head.as_u64().to_string()
    );

    h.engine.script_delivery(
        DeliveryOp::Resume(d.id),
        delivery_with(StageStatus::Running),
    );
    let path = format!("/v1/deliveries/{}/resume", d.id);
    let resp = send(&h, ctl(&op, &path, Some(KEY2)), "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body(&resp)["stage_status"], "running");

    let node: Node = builders::node();
    h.engine.script_retry_node(node.clone());
    let path = format!("/v1/nodes/{}/retry", node.id);
    let resp = send(&h, ctl(&op, &path, Some("retry-key-00000000001")), "{}").await;
    assert_eq!(resp.status(), 202);
    validate("Node", &body(&resp));
    let ops: Vec<&str> = h.engine.calls().iter().map(EngineCall::op).collect();
    assert_eq!(ops, ["pause", "resume", "retry_node"]);
}

#[tokio::test]
async fn engine_errors_map_to_problems() {
    let h = harness(None);
    let op = operator(&h);
    let d = builders::delivery();
    let node_id = NodeId::from_parts(T0 as u64, 77).unwrap();
    let invalid = |m: &str| ModuleError::InvalidState { message: m.into() };
    h.engine
        .script_delivery_error(DeliveryOp::Pause(d.id), invalid("terminal"));
    h.engine.script_retry_node_error(invalid("not failed"));
    let path = format!("/v1/deliveries/{}/pause", d.id);
    let resp = send(&h, ctl(&op, &path, Some(KEY)), "{}").await;
    assert_eq!(
        assert_problem(&resp, 409, "conflict")["reason"],
        "invalid_state"
    );
    let path = format!("/v1/nodes/{node_id}/retry");
    let resp = send(&h, ctl(&op, &path, Some(KEY2)), "{}").await;
    assert_eq!(
        assert_problem(&resp, 409, "conflict")["reason"],
        "invalid_state"
    );
    // 未脚本化的交付单元：FakeEngine 返回 NotFound
    let missing = DeliveryId::from_parts(T0 as u64, 1).unwrap();
    let path = format!("/v1/deliveries/{missing}/resume");
    let resp = send(&h, ctl(&op, &path, Some("missing-key-0000001")), "{}").await;
    assert_problem(&resp, 404, "not_found");
}

#[tokio::test]
async fn abandon_takes_an_optional_key_and_a_bounded_reason() {
    let h = harness(None);
    let op = operator(&h);
    let mut d = builders::delivery();
    d.stage = Stage::Abandoned;
    d.stage_status = StageStatus::Done;
    h.engine
        .script_delivery(DeliveryOp::Abandon(d.id), d.clone());
    let path = format!("/v1/deliveries/{}/abandon", d.id);
    let too_long = json!({ "reason": "x".repeat(501) }).to_string();
    let resp = send(&h, ctl(&op, &path, None), &too_long).await;
    let b = assert_problem(&resp, 422, "validation_failed");
    assert_eq!(b["errors"][0]["pointer"], "/reason");
    assert_eq!(b["errors"][0]["code"], "range");

    let resp = send(&h, ctl(&op, &path, None), r#"{"reason":"不做了"}"#).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body(&resp)["stage"], "abandoned");
    match &h.engine.calls()[0] {
        EngineCall::Abandon { ctx, reason, .. } => {
            assert_eq!(reason.as_deref(), Some("不做了"));
            // 没有 Idempotency-Key → 不带 dedup_key
            assert_eq!(ctx.idempotency_key, None);
            assert_eq!(ctx.dedup_key, None);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn control_bodies_only_accept_empty_objects() {
    let h = harness(None);
    let op = operator(&h);
    let d = builders::delivery();
    let path = format!("/v1/deliveries/{}/pause", d.id);
    let resp = send(
        &h,
        ctl(&op, &path, Some(KEY)),
        r#"{"model":"opus","budget":1}"#,
    )
    .await;
    let b = assert_problem(&resp, 422, "validation_failed");
    let mut pointers: Vec<&str> = b["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["pointer"].as_str().unwrap())
        .collect();
    pointers.sort_unstable();
    assert_eq!(pointers, ["/budget", "/model"]);
    assert!(b["errors"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["code"] == "unknown_field"));
    assert!(h.engine.calls().is_empty());
}

async fn conversation(h: &Harness) -> ConversationId {
    let project = ProjectId::from_parts(T0 as u64, 5).unwrap();
    let ctx = ActorCtx {
        actor: gantry_core::entities::Actor::new(gantry_core::entities::ActorKind::System, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(0, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    };
    h.conversation
        .ensure_conversation(&project, &ctx)
        .await
        .unwrap()
}

fn post(d: &Device, cnv: ConversationId, key: &str) -> gantry_api::RequestHead {
    let path = format!("/v1/conversations/{cnv}/messages");
    ctl(d, &path, Some(key))
}

fn user_message(cnv: ConversationId, n: u128) -> Message {
    let mut m = builders::message();
    m.id = gantry_core::ids::MessageId::from_parts(T0 as u64, n).unwrap();
    m.conversation_id = cnv;
    m.role = MessageRole::User;
    m.agent_kind = None;
    m.tone = None;
    m.cards = Vec::new();
    m.delivery_id = None;
    m
}

async fn posted_fixture(h: &Harness, msg: &Message, dedup_key: String) -> Seq {
    let data = fixture_data("message.posted");
    let subject = msg.id.to_string();
    record(
        &h.store,
        EventType::MessagePosted,
        &subject,
        None,
        data,
        Some(dedup_key),
    )
    .await
}

#[tokio::test]
async fn post_message_reports_message_posted_seq_and_builds_ctx() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h).await;
    let msg = user_message(cnv, 40);
    h.conversation.inner.push_scripted_message(cnv, msg.clone());
    let posted = posted_fixture(&h, &msg, dedup(&op, KEY)).await;

    let req_body = r#"{"text":"把超时改成 30 秒","level_cap":"High","refs":[{"kind":"issue","repo":"acme/web","number":5}]}"#;
    let resp = send(&h, post(&op, cnv, KEY), req_body).await;
    assert_eq!(resp.status(), 201);
    validate("Message", &body(&resp));
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        posted.as_u64().to_string()
    );

    let (ctx, sent) = post_calls(&h)[0].clone();
    assert_eq!(ctx.dedup_key, Some(dedup(&op, KEY)));
    assert_eq!(ctx.client_profile, Some(ClientProfile::Desktop));
    assert_eq!(sent.text, "把超时改成 30 秒");
    assert_eq!(sent.level_cap, Some(gantry_core::level::Level::High));
    assert!(matches!(&sent.refs[..], [Ref::Issue { number: 5, .. }]));
}

#[tokio::test]
async fn mobile_level_cap_is_accepted_but_ignored() {
    let h = harness(None);
    let phone = device(5, DevicePlatform::Ios, DeviceRole::Operator);
    h.devices.insert(phone.clone());
    let cnv = conversation(&h).await;
    let resp = send(
        &h,
        post(&phone, cnv, KEY),
        r#"{"text":"hi","level_cap":"Max"}"#,
    )
    .await;
    assert_eq!(resp.status(), 201);
    let (ctx, msg) = post_calls(&h)[0].clone();
    assert_eq!(msg.level_cap, None);
    assert_eq!(ctx.client_profile, Some(ClientProfile::Mobile));
}

#[tokio::test]
async fn message_bodies_reject_configuration_fields_and_bad_refs() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h).await;
    let cases: [(&str, &str, &str); 7] = [
        (r#"{"text":"hi","model":"opus"}"#, "/model", "unknown_field"),
        (
            r#"{"text":"hi","budget":{"tokens":1}}"#,
            "/budget",
            "unknown_field",
        ),
        (r#"{"level_cap":"Low"}"#, "/text", "required"),
        (r#"{"text":"hi","level_cap":"Huge"}"#, "/level_cap", "enum"),
        (r#"{"text":""}"#, "/text", "range"),
        // oneOf 分支按 kind 选中：保留分支内的具体字段与指针
        (
            r#"{"text":"x","refs":[{"kind":"issue","repo":"a/b","number":1,"model":"x"}]}"#,
            "/refs/0/model",
            "unknown_field",
        ),
        (
            r#"{"text":"x","refs":[{"kind":"pr","repo":"a/b"}]}"#,
            "/refs/0/number",
            "required",
        ),
    ];
    for (i, (payload, pointer, code)) in cases.into_iter().enumerate() {
        let key = format!("message-bad-body-{i:04}");
        let resp = send(&h, post(&op, cnv, &key), payload).await;
        let b = assert_problem(&resp, 422, "validation_failed");
        assert!(
            b["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["pointer"] == pointer && e["code"] == code),
            "{payload}: {b}"
        );
    }
    let too_many: Vec<Value> = (1..=21)
        .map(|n| json!({"kind":"issue","repo":"a/b","number":n}))
        .collect();
    let payload = json!({"text":"x","refs":too_many}).to_string();
    let resp = send(&h, post(&op, cnv, "message-too-many-refs"), &payload).await;
    let b = assert_problem(&resp, 422, "validation_failed");
    assert_eq!(b["errors"][0]["pointer"], "/refs");
    // 服务端解析不到的引用 → 422，指向该引用
    h.conversation.fail_next(ConvError::UnresolvedRef {
        reference: Ref::Issue {
            repo: "a/b".parse().unwrap(),
            number: 404,
        },
    });
    let payload = r#"{"text":"x","refs":[{"kind":"pr","repo":"a/b","number":1},{"kind":"issue","repo":"a/b","number":404}]}"#;
    let resp = send(&h, post(&op, cnv, "message-unresolved-ref"), payload).await;
    let b = assert_problem(&resp, 422, "validation_failed");
    assert_eq!(b["errors"][0]["pointer"], "/refs/1");
    assert_eq!(b["errors"][0]["code"], "enum");
    // 未知对话
    let missing = ConversationId::from_parts(T0 as u64, 1).unwrap();
    let resp = send(
        &h,
        post(&op, missing, "message-missing-cnv1"),
        r#"{"text":"x"}"#,
    )
    .await;
    assert_problem(&resp, 404, "not_found");
    // 注入探针不回显（I5）：错误只给字段指针，不回显请求里的值
    let probe = json!({"text": "x", "level_cap": "PROBE-<script>-7f3a"}).to_string();
    let resp = send(&h, post(&op, cnv, "message-probe-000001"), &probe).await;
    assert_eq!(resp.status(), 422);
    let raw = String::from_utf8(resp.body().as_bytes().to_vec()).unwrap();
    assert!(!raw.contains("<script>"), "{raw}");
}

#[tokio::test]
async fn idempotent_replay_is_byte_identical_and_calls_once() {
    let h = harness(None);
    let op = operator(&h);
    let other = device(2, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(other.clone());
    let cnv = conversation(&h).await;
    let first = send(&h, post(&op, cnv, KEY), r#"{"text":"hi"}"#).await;
    // 同一 JSON 的不同空白 / 键序：指纹按 JCS，视为同一请求
    let second = send(&h, post(&op, cnv, KEY), r#"{ "text" : "hi" }"#).await;
    assert_eq!(first.status(), 201);
    assert_eq!(second.status(), 201);
    assert_eq!(first.body().as_bytes(), second.body().as_bytes());
    assert_eq!(header(&second, "idempotent-replayed"), Some("true"));
    assert!(header(&first, "idempotent-replayed").is_none());
    assert_eq!(
        header(&first, "x-event-seq"),
        header(&second, "x-event-seq")
    );
    assert_eq!(post_calls(&h).len(), 1);

    // 同键不同内容 → 422 idempotency_key_reuse
    let resp = send(&h, post(&op, cnv, KEY), r#"{"text":"bye"}"#).await;
    assert_problem(&resp, 422, "idempotency_key_reuse");
    // 同键不同路径（具体路径参与指纹）→ 422
    let other_cnv = ConversationId::from_parts(T0 as u64, 901).unwrap();
    let resp = send(&h, post(&op, other_cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_problem(&resp, 422, "idempotency_key_reuse");
    // 不同设备同键互不影响
    let resp = send(&h, post(&other, cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(resp.status(), 201);
    assert!(header(&resp, "idempotent-replayed").is_none());
    assert_eq!(post_calls(&h).len(), 2);
}

#[tokio::test]
async fn four_xx_is_registered_five_xx_is_not() {
    let h = harness(None);
    let op = operator(&h);
    let missing = ConversationId::from_parts(T0 as u64, 1).unwrap();
    let first = send(&h, post(&op, missing, KEY), r#"{"text":"hi"}"#).await;
    assert_problem(&first, 404, "not_found");
    let again = send(&h, post(&op, missing, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(again.status(), 404);
    assert_eq!(first.body().as_bytes(), again.body().as_bytes());
    assert_eq!(header(&again, "idempotent-replayed"), Some("true"));
    assert_eq!(
        header(&again, "x-request-id"),
        header(&first, "x-request-id")
    );
    assert_eq!(post_calls(&h).len(), 1);

    let d = builders::delivery();
    let path = format!("/v1/deliveries/{}/pause", d.id);
    let unavailable = ModuleError::Store(StoreError::Unavailable { retry_after: None });
    h.engine
        .script_delivery_error(DeliveryOp::Pause(d.id), unavailable);
    let resp = send(&h, ctl(&op, &path, Some(KEY2)), "{}").await;
    assert_problem(&resp, 503, "unavailable");
    h.engine.script_delivery(DeliveryOp::Pause(d.id), d.clone());
    let resp = send(&h, ctl(&op, &path, Some(KEY2)), "{}").await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp, "idempotent-replayed").is_none());
    assert_eq!(h.engine.calls().len(), 2);
}

#[tokio::test]
async fn in_progress_and_retention() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h).await;
    let path = format!("/v1/conversations/{cnv}/messages");
    let payload = json!({"text": "hi"});
    // 首次请求仍在处理（租约内）→ 409
    let print = fingerprint("POST", &path, Some(&payload));
    assert!(matches!(
        h.idempotency.begin(&op.id, KEY, print),
        Begin::Proceed(_)
    ));
    let resp = send(&h, post(&op, cnv, KEY), &payload.to_string()).await;
    assert_problem(&resp, 409, "idempotency_in_progress");
    assert!(post_calls(&h).is_empty());
    // 租约过期（崩溃）→ 接管
    h.clock.advance_secs(31);
    let resp = send(&h, post(&op, cnv, KEY), &payload.to_string()).await;
    assert_eq!(resp.status(), 201);
    // 24 h 后 API 层记录失效：同键新内容不再是 idempotency_key_reuse
    // （业务事件的 dedup_key 仍由所属模块在 store 层去重，本文 §7.6）
    h.clock.advance_secs(24 * 3600);
    let resp = send(&h, post(&op, cnv, KEY), r#"{"text":"new"}"#).await;
    assert_eq!(resp.status(), 201);
    assert!(header(&resp, "idempotent-replayed").is_none());
    assert_eq!(post_calls(&h).len(), 2);
}

#[tokio::test]
async fn lost_completion_record_retries_with_the_same_dedup_key() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h).await;
    let msg = user_message(cnv, 41);
    // 所属模块对同一 dedup_key 的重试返回同一条消息（store 层 Duplicate）
    h.conversation.inner.push_scripted_message(cnv, msg.clone());
    h.conversation.inner.push_scripted_message(cnv, msg.clone());
    let posted = posted_fixture(&h, &msg, dedup(&op, KEY)).await;
    let first = send(&h, post(&op, cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(first.status(), 201);
    // 模拟"业务事务已提交、幂等完成记录未写"后崩溃重启：换一个空的幂等存储
    let restarted = Arc::new(MemoryIdempotency::new(h.clock.clone()));
    let h2 = rebuild_with_idempotency(&h, restarted);
    let retry = send(&h2, post(&op, cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(retry.status(), 201);
    // 重试带同一 dedup_key 进入所属模块，响应指向同一事件
    let calls = post_calls(&h2);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0.dedup_key, calls[1].0.dedup_key);
    assert_eq!(body(&retry)["id"], body(&first)["id"]);
    assert_eq!(
        header(&retry, "x-event-seq").unwrap(),
        posted.as_u64().to_string()
    );
}
