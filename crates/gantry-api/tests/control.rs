//! 控制路由 #8 #43–#45、发消息 #12 与幂等键（本文 §12.2「控制路由」「分页 / 幂等」
//! 「统一错误 / 校验」中的 M1 条目）。

mod support;

use gantry_api::idempotency::{fingerprint, Begin, IdempotencyStore};
use gantry_core::entities::{
    Delivery, DevicePlatform, DeviceRole, Node, NodeStatus, Seq, Stage, StageStatus,
};
use gantry_core::ids::{ConversationId, DeliveryId, NodeId};
use gantry_core::testing::builders;
use gantry_store::EventFilter;
use http::Method;
use serde_json::{json, Value};
use support::*;

const KEY: &str = "01JB7P9Z3K5R8T2X6N1Q4V7WYA";
const KEY2: &str = "01JB7P9Z3K5R8T2X6N1Q4V7WYB";

async fn delivery(h: &Harness, stage: Stage, status: StageStatus) -> Delivery {
    let mut d = builders::delivery();
    d.id = DeliveryId::from_parts(
        T0 as u64,
        500 + h.engine.deliveries.lock().unwrap().len() as u128,
    )
    .unwrap();
    d.stage = stage;
    d.stage_status = status;
    h.engine.add_delivery(&d).await;
    d
}

fn node(h: &Harness, status: NodeStatus) -> Node {
    let mut n = builders::node();
    n.id = NodeId::from_parts(
        T0 as u64,
        700 + h.engine.nodes.lock().unwrap().len() as u128,
    )
    .unwrap();
    n.status = status;
    h.engine.nodes.lock().unwrap().insert(n.id, n.clone());
    n
}

fn conversation(h: &Harness) -> ConversationId {
    let id = ConversationId::from_parts(T0 as u64, 900).unwrap();
    h.conversation.known.lock().unwrap().push(id);
    id
}

async fn events(h: &Harness) -> Vec<(Seq, String)> {
    h.store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 200)
        .await
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.ty.as_str() != "delivery.created")
        .map(|e| (e.seq, e.ty.as_str().to_owned()))
        .collect()
}

fn ctl(d: &gantry_core::entities::Device, path: String) -> gantry_api::RequestHead {
    json_req(Method::POST, &path, d)
}

fn operator(h: &Harness) -> gantry_core::entities::Device {
    let d = device(1, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(d.clone());
    d
}

#[tokio::test]
async fn control_routes_require_key_role_and_desktop() {
    let h = harness(None);
    let op = operator(&h);
    let viewer = device(2, DevicePlatform::Macos, DeviceRole::Viewer);
    let ios = device(3, DevicePlatform::Ios, DeviceRole::Operator);
    h.devices.insert(viewer.clone());
    h.devices.insert(ios.clone());
    let d = delivery(&h, Stage::Implement, StageStatus::Running).await;
    let n = node(&h, NodeStatus::Failed);
    for path in [
        format!("/v1/deliveries/{}/pause", d.id),
        format!("/v1/deliveries/{}/resume", d.id),
        format!("/v1/nodes/{}/retry", n.id),
    ] {
        assert_problem(
            &send(&h, ctl(&op, path.clone()), "{}").await,
            400,
            "idempotency_key_required",
        );
        let b = assert_problem(
            &send(
                &h,
                ctl(&viewer, path.clone()).header("idempotency-key", KEY),
                "{}",
            )
            .await,
            403,
            "forbidden_role",
        );
        assert_eq!(b["required_role"], "operator");
        assert_problem(
            &send(
                &h,
                ctl(&ios, path.clone()).header("idempotency-key", KEY),
                "{}",
            )
            .await,
            403,
            "forbidden_profile",
        );
    }
    assert!(h.engine.ops().is_empty());
}

#[tokio::test]
async fn pause_resume_retry_report_the_written_event_seq() {
    let h = harness(None);
    let op = operator(&h);
    let d = delivery(&h, Stage::Implement, StageStatus::Running).await;
    let n = node(&h, NodeStatus::Failed);

    let resp = send(
        &h,
        ctl(&op, format!("/v1/deliveries/{}/pause", d.id)).header("idempotency-key", KEY),
        "{}",
    )
    .await;
    assert_eq!(resp.status(), 200);
    let b = body(&resp);
    validate("Delivery", &b);
    assert_eq!(b["stage_status"], "paused");
    let evs = events(&h).await;
    assert_eq!(evs.last().unwrap().1, "delivery.paused");
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        evs.last().unwrap().0.as_u64().to_string()
    );
    let dedup = format!("api:{}:{KEY}", op.id);
    assert_eq!(
        h.store.events().by_dedup_key(&dedup).await.unwrap(),
        Some(evs[0].0)
    );

    // 暂停后再暂停（新键）：目标状态幂等，200，不追加事件，X-Event-Seq = head
    let resp = send(
        &h,
        ctl(&op, format!("/v1/deliveries/{}/pause", d.id)).header("idempotency-key", KEY2),
        "",
    )
    .await;
    assert_eq!(resp.status(), 200);
    let evs = events(&h).await;
    assert_eq!(evs.len(), 1);
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        evs[0].0.as_u64().to_string()
    );

    let key3 = "resume-key-0000000001";
    let resp = send(
        &h,
        ctl(&op, format!("/v1/deliveries/{}/resume", d.id)).header("idempotency-key", key3),
        "{}",
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body(&resp)["stage_status"], "running");
    let evs = events(&h).await;
    assert_eq!(evs[1].1, "delivery.resumed");
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        evs[1].0.as_u64().to_string()
    );

    let key4 = "retry-key-00000000001";
    let resp = send(
        &h,
        ctl(&op, format!("/v1/nodes/{}/retry", n.id)).header("idempotency-key", key4),
        "{}",
    )
    .await;
    assert_eq!(resp.status(), 202);
    validate("Node", &body(&resp));
    let evs = events(&h).await;
    assert_eq!(evs[2].1, "node.retry_requested");
    assert_eq!(
        header(&resp, "x-event-seq").unwrap(),
        evs[2].0.as_u64().to_string()
    );
}

#[tokio::test]
async fn invalid_states_and_missing_targets() {
    let h = harness(None);
    let op = operator(&h);
    let closed = delivery(&h, Stage::Closed, StageStatus::Done).await;
    let ok_node = node(&h, NodeStatus::Succeeded);
    let missing = DeliveryId::from_parts(T0 as u64, 1).unwrap();
    let cases = [
        (format!("/v1/deliveries/{}/pause", closed.id), 409),
        (format!("/v1/deliveries/{}/resume", closed.id), 409),
        (format!("/v1/nodes/{}/retry", ok_node.id), 409),
        (format!("/v1/deliveries/{missing}/pause"), 404),
    ];
    for (i, (path, status)) in cases.into_iter().enumerate() {
        let key = format!("invalid-state-key-{i:04}");
        let resp = send(&h, ctl(&op, path).header("idempotency-key", &key), "{}").await;
        if status == 409 {
            let b = assert_problem(&resp, 409, "conflict");
            assert_eq!(b["reason"], "invalid_state");
        } else {
            assert_problem(&resp, 404, "not_found");
        }
    }
    assert!(events(&h).await.is_empty());
}

#[tokio::test]
async fn abandon_takes_an_optional_key_and_a_bounded_reason() {
    let h = harness(None);
    let op = operator(&h);
    let d = delivery(&h, Stage::Review, StageStatus::Running).await;
    let path = format!("/v1/deliveries/{}/abandon", d.id);
    let too_long = json!({ "reason": "x".repeat(501) }).to_string();
    let b = assert_problem(
        &send(&h, ctl(&op, path.clone()), &too_long).await,
        422,
        "validation_failed",
    );
    assert_eq!(b["errors"][0]["pointer"], "/reason");
    assert_eq!(b["errors"][0]["code"], "range");

    let resp = send(&h, ctl(&op, path.clone()), r#"{"reason":"不做了"}"#).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body(&resp)["stage"], "abandoned");
    // 已放弃：原样 200（目标状态幂等），不追加事件
    let resp = send(&h, ctl(&op, path), "{}").await;
    assert_eq!(resp.status(), 200);
    let evs = events(&h).await;
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].1, "delivery.abandoned");
    // 没有 Idempotency-Key → 不带 dedup_key
    assert!(h
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(_, ctx)| ctx.dedup_key.is_none()));
}

#[tokio::test]
async fn control_bodies_only_accept_empty_objects() {
    let h = harness(None);
    let op = operator(&h);
    let d = delivery(&h, Stage::Implement, StageStatus::Running).await;
    let resp = send(
        &h,
        ctl(&op, format!("/v1/deliveries/{}/pause", d.id)).header("idempotency-key", KEY),
        r#"{"model":"opus","budget":1}"#,
    )
    .await;
    let b = assert_problem(&resp, 422, "validation_failed");
    let pointers: Vec<&str> = b["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["pointer"].as_str().unwrap())
        .collect();
    assert_eq!(pointers, ["/model", "/budget"]);
    assert!(b["errors"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["code"] == "unknown_field"));
    assert!(h.engine.ops().is_empty());
}

fn post(
    d: &gantry_core::entities::Device,
    cnv: ConversationId,
    key: &str,
) -> gantry_api::RequestHead {
    json_req(
        Method::POST,
        &format!("/v1/conversations/{cnv}/messages"),
        d,
    )
    .header("idempotency-key", key)
}

#[tokio::test]
async fn post_message_builds_actor_ctx_and_reports_message_posted_seq() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h);
    let req_body = r#"{"text":"把超时改成 30 秒","level_cap":"High","refs":[{"kind":"issue","repo":"acme/web","number":5}]}"#;
    let resp = send(&h, post(&op, cnv, KEY), req_body).await;
    assert_eq!(resp.status(), 201);
    let b = body(&resp);
    validate("Message", &b);
    assert_eq!(b["role"], "user");
    assert_eq!(b["level_cap"], "High");
    let evs = events(&h).await;
    assert_eq!(evs[0].1, "message.posted");
    assert_eq!(header(&resp, "x-event-seq"), Some("1"));

    let (ctx, _) = h.conversation.calls.lock().unwrap()[0].clone();
    assert_eq!(ctx.actor.kind, gantry_core::entities::ActorKind::Device);
    assert_eq!(ctx.actor.id, op.id.to_string());
    assert_eq!(ctx.device, Some(op.id));
    assert_eq!(ctx.role, Some(DeviceRole::Operator));
    assert_eq!(
        ctx.client_profile,
        Some(gantry_core::ports::ClientProfile::Desktop)
    );
    assert_eq!(
        ctx.request_id.to_string(),
        header(&resp, "x-request-id").unwrap()
    );
    assert_eq!(ctx.idempotency_key.as_deref(), Some(KEY));
    assert_eq!(ctx.dedup_key, Some(format!("api:{}:{KEY}", op.id)));
}

#[tokio::test]
async fn mobile_level_cap_is_accepted_but_ignored() {
    let h = harness(None);
    let phone = device(5, DevicePlatform::Ios, DeviceRole::Operator);
    h.devices.insert(phone.clone());
    let cnv = conversation(&h);
    let resp = send(
        &h,
        post(&phone, cnv, KEY),
        r#"{"text":"hi","level_cap":"Max"}"#,
    )
    .await;
    assert_eq!(resp.status(), 201);
    let (ctx, msg) = h.conversation.calls.lock().unwrap()[0].clone();
    assert_eq!(msg.level_cap, None);
    assert_eq!(
        ctx.client_profile,
        Some(gantry_core::ports::ClientProfile::Mobile)
    );
}

#[tokio::test]
async fn message_bodies_reject_configuration_fields_and_bad_refs() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h);
    let cases: [(&str, &str, &str); 5] = [
        (r#"{"text":"hi","model":"opus"}"#, "/model", "unknown_field"),
        (
            r#"{"text":"hi","budget":{"tokens":1}}"#,
            "/budget",
            "unknown_field",
        ),
        (r#"{"level_cap":"Low"}"#, "/text", "required"),
        (r#"{"text":"hi","level_cap":"Huge"}"#, "/level_cap", "enum"),
        (r#"{"text":""}"#, "/text", "range"),
    ];
    for (i, (payload, pointer, code)) in cases.into_iter().enumerate() {
        let key = format!("message-bad-body-{i:04}");
        let b = assert_problem(
            &send(&h, post(&op, cnv, &key), payload).await,
            422,
            "validation_failed",
        );
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
    let b = assert_problem(
        &send(
            &h,
            post(&op, cnv, "message-too-many-refs"),
            &json!({"text":"x","refs":too_many}).to_string(),
        )
        .await,
        422,
        "validation_failed",
    );
    assert_eq!(b["errors"][0]["pointer"], "/refs");
    // 服务端解析不到的引用 → 422，指向该引用
    let b = assert_problem(
        &send(
            &h,
            post(&op, cnv, "message-unresolved-ref"),
            r#"{"text":"x","refs":[{"kind":"pr","repo":"a/b","number":1},{"kind":"issue","repo":"a/b","number":404}]}"#,
        )
        .await,
        422,
        "validation_failed",
    );
    assert_eq!(b["errors"][0]["pointer"], "/refs/1");
    assert_eq!(b["errors"][0]["code"], "enum");
    // 未知对话
    let missing = ConversationId::from_parts(T0 as u64, 1).unwrap();
    assert_problem(
        &send(
            &h,
            post(&op, missing, "message-missing-cnv1"),
            r#"{"text":"x"}"#,
        )
        .await,
        404,
        "not_found",
    );
    // 注入探针不回显（I5）：错误只给字段指针，不回显请求里的值
    let probe = "PROBE-<script>-7f3a";
    let resp = send(
        &h,
        post(&op, cnv, "message-probe-000001"),
        &json!({"text": "x", "level_cap": probe}).to_string(),
    )
    .await;
    assert_eq!(resp.status(), 422);
    let raw = String::from_utf8(resp.body().to_vec()).unwrap();
    assert!(!raw.contains("<script>"), "{raw}");
    assert!(events(&h).await.is_empty());
}

#[tokio::test]
async fn idempotent_replay_is_byte_identical_and_appends_once() {
    let h = harness(None);
    let op = operator(&h);
    let other = device(2, DevicePlatform::Macos, DeviceRole::Operator);
    h.devices.insert(other.clone());
    let cnv = conversation(&h);
    let first = send(&h, post(&op, cnv, KEY), r#"{"text":"hi"}"#).await;
    // 同一 JSON 的不同空白 / 键序：指纹按 JCS，视为同一请求
    let second = send(&h, post(&op, cnv, KEY), r#"{ "text" : "hi" }"#).await;
    assert_eq!(first.status(), 201);
    assert_eq!(second.status(), 201);
    assert_eq!(first.body(), second.body());
    assert_eq!(header(&second, "idempotent-replayed"), Some("true"));
    assert!(header(&first, "idempotent-replayed").is_none());
    assert_eq!(
        header(&first, "x-event-seq"),
        header(&second, "x-event-seq")
    );
    assert_eq!(events(&h).await.len(), 1);
    assert_eq!(h.conversation.calls.lock().unwrap().len(), 1);

    // 同键不同内容 → 422 idempotency_key_reuse
    assert_problem(
        &send(&h, post(&op, cnv, KEY), r#"{"text":"bye"}"#).await,
        422,
        "idempotency_key_reuse",
    );
    // 同键不同路径（具体路径参与指纹）→ 422
    let other_cnv = ConversationId::from_parts(T0 as u64, 901).unwrap();
    assert_problem(
        &send(&h, post(&op, other_cnv, KEY), r#"{"text":"hi"}"#).await,
        422,
        "idempotency_key_reuse",
    );
    // 不同设备同键互不影响
    let resp = send(&h, post(&other, cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(resp.status(), 201);
    assert!(header(&resp, "idempotent-replayed").is_none());
    assert_eq!(events(&h).await.len(), 2);
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
    assert_eq!(first.body(), again.body());
    assert_eq!(header(&again, "idempotent-replayed"), Some("true"));
    assert_eq!(
        header(&again, "x-request-id"),
        header(&first, "x-request-id")
    );

    let d = delivery(&h, Stage::Implement, StageStatus::Running).await;
    *h.engine.fail_next.lock().unwrap() = true;
    let path = format!("/v1/deliveries/{}/pause", d.id);
    let resp = send(
        &h,
        ctl(&op, path.clone()).header("idempotency-key", KEY2),
        "{}",
    )
    .await;
    assert_problem(&resp, 503, "unavailable");
    let resp = send(&h, ctl(&op, path).header("idempotency-key", KEY2), "{}").await;
    assert_eq!(resp.status(), 200);
    assert!(header(&resp, "idempotent-replayed").is_none());
}

#[tokio::test]
async fn in_progress_and_retention() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h);
    let path = format!("/v1/conversations/{cnv}/messages");
    let payload = json!({"text": "hi"});
    // 首次请求仍在处理（租约内）→ 409
    let print = fingerprint("POST", &path, Some(&payload));
    assert_eq!(h.idempotency.begin(&op.id, KEY, print), Begin::Proceed);
    assert_problem(
        &send(&h, post(&op, cnv, KEY), &payload.to_string()).await,
        409,
        "idempotency_in_progress",
    );
    // 租约过期（崩溃）→ 接管
    h.clock.advance_secs(31);
    assert_eq!(
        send(&h, post(&op, cnv, KEY), &payload.to_string())
            .await
            .status(),
        201
    );
    // 24 h 后 API 层记录失效：同键新内容不再是 idempotency_key_reuse。
    // 业务事件的 dedup_key 由 (设备, 键) 派生且长期有效，store 层仍去重（本文 §7.6）：
    // 客户端必须为每次用户意图生成新键，这里只断言 API 层不再拦截。
    h.clock.advance_secs(24 * 3600);
    let resp = send(&h, post(&op, cnv, KEY), r#"{"text":"new"}"#).await;
    assert_eq!(resp.status(), 201);
    assert!(header(&resp, "idempotent-replayed").is_none());
    assert_eq!(events(&h).await.len(), 1);
}

#[tokio::test]
async fn lost_completion_record_is_covered_by_dedup_key() {
    let h = harness(None);
    let op = operator(&h);
    let cnv = conversation(&h);
    let first = send(&h, post(&op, cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(first.status(), 201);
    // 模拟"业务事务已提交、幂等完成记录未写"后崩溃重启
    h.idempotency.abandon(&op.id, KEY);
    let retry = send(&h, post(&op, cnv, KEY), r#"{"text":"hi"}"#).await;
    assert_eq!(retry.status(), 201);
    assert_eq!(
        events(&h).await.len(),
        1,
        "store dedup_key prevents a second message.posted"
    );
    assert_eq!(body(&retry)["id"], body(&first)["id"]);
    assert_eq!(header(&retry, "x-event-seq"), header(&first, "x-event-seq"));
}
