//! 消息列表路由的分页、隔离与错误映射。
mod support;

use std::sync::Arc;

use async_trait::async_trait;
use gantry_api::{ApiConfig, ApiCore};
use gantry_conversation::{ConversationDeps, DeliveryHandle, EngineApi, Limits, ModelsPort};
use gantry_core::entities::Ref;
use gantry_core::entities::{Conversation, DevicePlatform, DeviceRole, Message};
use gantry_core::events::EventType;
use gantry_core::ids::{ConversationId, DeliveryId, MessageId, ProjectId, Ulid};
use gantry_core::ports::context::{ActorCtx, PageOrder, PageQuery, PageWithSeq, StoreError};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_core::ports::engine::{CreateDelivery, EngineError};
use gantry_core::testing::builders;
use gantry_fakes::conversation::ConversationCall;
use gantry_models::{DirectCall, DirectOutcome, ModelsError};
use http::Method;
use serde_json::json;
use support::*;
use tokio::sync::Notify;

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: gantry_core::entities::Actor::new(gantry_core::entities::ActorKind::System, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(0, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

async fn seed(h: &Harness, project: u128, count: u128) -> (ConversationId, Vec<String>) {
    let project_id = ProjectId::from_parts(T0 as u64, project).unwrap();
    let cnv = h
        .conversation
        .ensure_conversation(&project_id, &ctx())
        .await
        .unwrap();
    let mut ids = Vec::new();
    for n in 1..=count {
        let mut message = builders::message();
        message.id = MessageId::from_parts(T0 as u64, n + project).unwrap();
        message.conversation_id = cnv;
        h.conversation.inner.push_scripted_message(cnv, message);
        let msg = h
            .conversation
            .post_user_message(
                ctx(),
                cnv,
                NewUserMessage {
                    text: format!("message {n}"),
                    level_cap: None,
                    refs: vec![],
                },
            )
            .await
            .unwrap();
        ids.push(msg.id.to_string());
    }
    (cnv, ids)
}

fn path(id: ConversationId) -> String {
    format!("/v1/conversations/{id}/messages")
}

fn ids(response: &gantry_api::ApiResponse) -> Vec<String> {
    body(response)["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn messages_paginate_by_id_and_bind_cursors() {
    let h = harness(None);
    let viewer = device(1, DevicePlatform::Ios, DeviceRole::Viewer);
    h.devices.insert(viewer.clone());
    let (cnv, expected) = seed(&h, 100, 3).await;
    let (other, other_ids) = seed(&h, 200, 1).await;
    let base = path(cnv);
    let first = send(
        &h,
        req(Method::GET, &format!("{base}?limit=2"), &viewer),
        "",
    )
    .await;
    assert_eq!(first.status(), 200);
    validate("MessageList", &body(&first));
    assert_eq!(ids(&first), vec![expected[2].clone(), expected[1].clone()]);
    let cursor = body(&first)["next_cursor"].as_str().unwrap().to_owned();
    assert!(!cursor.contains(&cnv.to_string()));
    let inner =
        gantry_api::reads::unwrap_cursor(&format!("v1:messages:{cnv}:desc"), &cursor).unwrap();
    let mut inserted = builders::message();
    inserted.id = MessageId::from_parts(T0 as u64, 150).unwrap();
    inserted.conversation_id = cnv;
    h.conversation.inner.push_scripted_message(cnv, inserted);
    h.conversation
        .post_user_message(
            ctx(),
            cnv,
            NewUserMessage {
                text: "inserted during paging".into(),
                level_cap: None,
                refs: vec![],
            },
        )
        .await
        .unwrap();
    let second = send(
        &h,
        req(
            Method::GET,
            &format!("{base}?limit=2&order=desc&cursor={cursor}"),
            &viewer,
        ),
        "",
    )
    .await;
    assert_eq!(ids(&second), vec![expected[0].clone()]);
    assert!(body(&second)["next_cursor"].is_null());

    let asc = send(
        &h,
        req(Method::GET, &format!("{base}?order=asc&limit=2"), &viewer),
        "",
    )
    .await;
    assert_eq!(ids(&asc), expected[..2]);
    let asc_cursor = body(&asc)["next_cursor"].as_str().unwrap().to_owned();
    let last = send(
        &h,
        req(
            Method::GET,
            &format!("{base}?order=asc&cursor={asc_cursor}"),
            &viewer,
        ),
        "",
    )
    .await;
    assert_eq!(
        ids(&last),
        vec![
            expected[2].clone(),
            MessageId::from_parts(T0 as u64, 150).unwrap().to_string()
        ]
    );
    assert_eq!(
        ids(&send(&h, req(Method::GET, &path(other), &viewer), "").await),
        other_ids
    );

    for url in [
        format!("{base}?order=asc&cursor={cursor}"),
        format!("{}?cursor={cursor}", path(other)),
        format!("{base}?cursor=!!"),
        format!("{base}?cursor=seq:1"),
        format!(
            "{base}?cursor={}",
            gantry_api::reads::wrap_cursor(&format!("v1:messages:{cnv}:desc"), "seq:1")
        ),
        format!(
            "{base}?cursor={}",
            gantry_api::reads::wrap_cursor(&format!("v1:messages:{cnv}:desc"), "invalid")
        ),
        format!(
            "{base}?cursor={}",
            gantry_api::reads::wrap_cursor("inbox", "something")
        ),
    ] {
        assert_problem(
            &send(&h, req(Method::GET, &url, &viewer), "").await,
            400,
            "invalid_cursor",
        );
    }
    for query in [
        "limit=0",
        "limit=201",
        "limit=abc",
        "order=up",
        "unexpected=1",
        "limit=1&limit=2",
    ] {
        assert_problem(
            &send(
                &h,
                req(Method::GET, &format!("{base}?{query}"), &viewer),
                "",
            )
            .await,
            400,
            "invalid_request",
        );
    }
    assert_problem(
        &send(
            &h,
            req(Method::GET, "/v1/conversations/cnv_bad/messages", &viewer),
            "",
        )
        .await,
        400,
        "invalid_request",
    );
    let missing = ConversationId::from_parts(T0 as u64, 999).unwrap();
    assert_problem(
        &send(&h, req(Method::GET, &path(missing), &viewer), "").await,
        404,
        "not_found",
    );
    let (empty, _) = seed(&h, 300, 0).await;
    assert!(ids(&send(&h, req(Method::GET, &path(empty), &viewer), "").await).is_empty());

    let calls = h.conversation.inner.calls();
    assert!(calls.iter().any(|call| matches!(call, ConversationCall::ListMessages { conversation_id, page } if *conversation_id == cnv && page.order == Some(PageOrder::Desc) && page.limit == 2 && page.cursor.is_none())));
    assert!(calls.iter().any(|call| matches!(call, ConversationCall::ListMessages { conversation_id, page } if *conversation_id == cnv && page.order == Some(PageOrder::Desc) && page.cursor.as_deref() == Some(inner.as_str()))));
    assert!(calls.iter().any(|call| matches!(call, ConversationCall::ListMessages { conversation_id, page } if *conversation_id == cnv && page.order == Some(PageOrder::Asc) && page.limit == 2)));
    assert!(calls.iter().any(|call| matches!(call, ConversationCall::ListMessages { conversation_id, page } if *conversation_id == other && page.order == Some(PageOrder::Desc) && page.limit == 50)));
}

#[tokio::test]
async fn response_uses_downstream_watermark_not_store_head_and_roles_are_live() {
    let h = harness(None);
    let d = device(4, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(d.clone());
    let (cnv, _) = seed(&h, 4, 2).await;
    let snapshot = h
        .conversation
        .list_messages(
            &cnv,
            PageQuery {
                limit: 50,
                order: Some(PageOrder::Desc),
                cursor: None,
            },
        )
        .await
        .unwrap();
    let head = h.store.events().head().await.unwrap();
    assert_ne!(snapshot.last_seq, head);
    for role in [
        DeviceRole::Viewer,
        DeviceRole::Operator,
        DeviceRole::Approver,
    ] {
        h.devices
            .update(&d.cert_serial, |device| device.role = role);
        let resp = send(&h, req(Method::GET, &path(cnv), &d), "").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            header(&resp, "x-event-seq"),
            Some(snapshot.last_seq.as_u64().to_string().as_str())
        );
    }
    h.devices.update(&d.cert_serial, |device| {
        device.status = gantry_core::entities::DeviceStatus::Revoked
    });
    assert_problem(
        &send(&h, req(Method::GET, &path(cnv), &d), "").await,
        401,
        "device_revoked",
    );
}

struct Fault(ConvError);
#[async_trait]
impl ConversationService for Fault {
    async fn ensure_conversation(
        &self,
        _: &ProjectId,
        _: &ActorCtx,
    ) -> Result<ConversationId, ConvError> {
        unreachable!()
    }
    async fn post_user_message(
        &self,
        _: ActorCtx,
        _: ConversationId,
        _: NewUserMessage,
    ) -> Result<Message, ConvError> {
        unreachable!()
    }
    async fn list_conversations(
        &self,
        _: &ProjectId,
        _: PageQuery,
    ) -> Result<PageWithSeq<Conversation>, ConvError> {
        unreachable!()
    }
    async fn list_messages(
        &self,
        _: &ConversationId,
        _: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        Err(self.0.clone())
    }
}

#[tokio::test]
async fn downstream_errors_are_not_swallowed() {
    let h = harness(None);
    let d = device(5, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(d.clone());
    let id = ConversationId::from_parts(T0 as u64, 1).unwrap();
    let cases = [
        (ConvError::NotFound, 404, "not_found"),
        (
            ConvError::InvalidRequest {
                pointer: "/cursor".into(),
                message: "bad".into(),
            },
            400,
            "invalid_cursor",
        ),
        (
            ConvError::InvalidRequest {
                pointer: "/other".into(),
                message: "bad".into(),
            },
            500,
            "internal",
        ),
        (
            ConvError::Store(StoreError::Unavailable { retry_after: None }),
            503,
            "unavailable",
        ),
        (
            ConvError::Store(StoreError::Rejected {
                message: "bad".into(),
            }),
            500,
            "internal",
        ),
    ];
    for (error, status, code) in cases {
        let mut svc = h.core.services().clone();
        svc.conversation = Arc::new(Fault(error));
        let core = ApiCore::new(ApiConfig::default(), svc);
        let resp = core
            .handle(req(Method::GET, &path(id), &d), bytes::Bytes::new())
            .await;
        assert_problem(&resp, status, code);
    }
}

// 同步发消息和读取不调用引擎或模型；任何意外调用都使测试失败。
struct UnusedDependencies;

#[async_trait]
impl EngineApi for UnusedDependencies {
    async fn create_delivery(&self, _: CreateDelivery) -> Result<DeliveryHandle, EngineError> {
        panic!("message reads must not call the engine")
    }
    async fn locate_deliveries(
        &self,
        _: &ProjectId,
        _: &[Ref],
    ) -> Result<Vec<DeliveryHandle>, EngineError> {
        panic!("message reads must not call the engine")
    }
}

#[async_trait]
impl ModelsPort for UnusedDependencies {
    async fn configured(&self, _: &ProjectId) -> Result<bool, ModelsError> {
        panic!("message reads must not call models")
    }
    async fn invoke_direct(&self, _: DirectCall) -> Result<DirectOutcome, ModelsError> {
        panic!("message reads must not call models")
    }
}

fn real_conversation(h: &Harness) -> Arc<gantry_conversation::Conversation> {
    Arc::new(gantry_conversation::Conversation::new(ConversationDeps {
        store: h.store.clone(),
        ids: h.core.services().ids.clone(),
        engine: Arc::new(UnusedDependencies),
        models: Arc::new(UnusedDependencies),
        limits: Limits::default(),
    }))
}

fn api_with_conversation(h: &Harness, conversation: Arc<dyn ConversationService>) -> ApiCore {
    let mut services = h.core.services().clone();
    services.conversation = conversation;
    ApiCore::new(ApiConfig::default(), services)
}

async fn real_project(
    h: &Harness,
    conversation: &gantry_conversation::Conversation,
    n: u128,
) -> (ProjectId, ConversationId) {
    let project = ProjectId::from_parts(T0 as u64, n).unwrap();
    let mut data = doubles::fixture_data("project.added");
    data["id"] = json!(project);
    doubles::record(
        &h.store,
        EventType::ProjectAdded,
        &project.to_string(),
        Some(project),
        data,
        None,
    )
    .await;
    let cnv = conversation
        .ensure_conversation(&project, &ctx())
        .await
        .unwrap();
    (project, cnv)
}

async fn real_post(
    conversation: &gantry_conversation::Conversation,
    cnv: ConversationId,
    text: &str,
) -> Message {
    conversation
        .post_user_message(
            ctx(),
            cnv,
            NewUserMessage {
                text: text.into(),
                level_cap: None,
                refs: vec![],
            },
        )
        .await
        .unwrap()
}

async fn api_get(
    core: &ApiCore,
    viewer: &gantry_core::entities::Device,
    url: &str,
) -> gantry_api::ApiResponse {
    core.handle(req(Method::GET, url, viewer), bytes::Bytes::new())
        .await
}

#[tokio::test]
async fn real_service_api_paginates_and_rejects_foreign_cursors() {
    let h = harness(None);
    let viewer = device(10, DevicePlatform::Ios, DeviceRole::Viewer);
    h.devices.insert(viewer.clone());
    let real = real_conversation(&h);
    let (_, cnv) = real_project(&h, &real, 10).await;
    let (_, other) = real_project(&h, &real, 11).await;
    let core = api_with_conversation(&h, real.clone());
    let empty = api_get(&core, &viewer, &path(other)).await;
    assert_eq!(empty.status(), 200);
    assert!(ids(&empty).is_empty());
    let mut expected = Vec::new();
    for text in ["one", "two", "three"] {
        expected.push(real_post(&real, cnv, text).await.id.to_string());
    }
    expected.sort();
    let other_message = real_post(&real, other, "other project").await;
    let base = path(cnv);
    let first = api_get(&core, &viewer, &format!("{base}?limit=2")).await;
    assert_eq!(first.status(), 200);
    validate("MessageList", &body(&first));
    assert_eq!(ids(&first), vec![expected[2].clone(), expected[1].clone()]);
    assert_eq!(
        header(&first, "x-event-seq").unwrap(),
        h.store.events().head().await.unwrap().as_u64().to_string()
    );
    let cursor = body(&first)["next_cursor"].as_str().unwrap().to_owned();
    let second = api_get(
        &core,
        &viewer,
        &format!("{base}?limit=2&order=desc&cursor={cursor}"),
    )
    .await;
    assert_eq!(ids(&second), vec![expected[0].clone()]);
    assert!(body(&second)["next_cursor"].is_null());
    let asc = api_get(&core, &viewer, &format!("{base}?limit=2&order=asc")).await;
    assert_eq!(ids(&asc), expected[..2]);
    let asc_cursor = body(&asc)["next_cursor"].as_str().unwrap().to_owned();
    let last = api_get(
        &core,
        &viewer,
        &format!("{base}?order=asc&cursor={asc_cursor}"),
    )
    .await;
    assert_eq!(ids(&last), vec![expected[2].clone()]);
    assert_eq!(
        ids(&api_get(&core, &viewer, &path(other)).await),
        vec![other_message.id.to_string()]
    );
    for url in [
        format!("{base}?order=asc&cursor={cursor}"),
        format!("{}?cursor={cursor}", path(other)),
        format!("{base}?cursor=seq:1"),
        format!("{base}?cursor=!!"),
        format!(
            "{base}?cursor={}",
            gantry_api::reads::wrap_cursor(&format!("v1:messages:{cnv}:desc"), "invalid")
        ),
        format!(
            "{base}?cursor={}",
            gantry_api::reads::wrap_cursor(&format!("v1:messages:{cnv}:desc"), "seq:1")
        ),
    ] {
        assert_problem(&api_get(&core, &viewer, &url).await, 400, "invalid_cursor");
    }
    let missing = ConversationId::from_parts(T0 as u64, 9999).unwrap();
    assert_problem(
        &api_get(&core, &viewer, &path(missing)).await,
        404,
        "not_found",
    );
    // 外层无效在 API 被拒；外层合法但内层无效由真实服务先判不存在。
    assert_problem(
        &api_get(&core, &viewer, &format!("{}?cursor=!!", path(missing))).await,
        400,
        "invalid_cursor",
    );
    let inner_bad =
        gantry_api::reads::wrap_cursor(&format!("v1:messages:{missing}:desc"), "invalid");
    assert_problem(
        &api_get(
            &core,
            &viewer,
            &format!("{}?cursor={inner_bad}", path(missing)),
        )
        .await,
        404,
        "not_found",
    );
}

/// 在真实服务完成读取后暂停 HTTP 响应，确保后续写入发生在同一请求进行中。
struct PauseAfterRead {
    real: Arc<gantry_conversation::Conversation>,
    read: Notify,
    resume: Notify,
}

#[async_trait]
impl ConversationService for PauseAfterRead {
    async fn ensure_conversation(
        &self,
        project: &ProjectId,
        ctx: &ActorCtx,
    ) -> Result<ConversationId, ConvError> {
        self.real.ensure_conversation(project, ctx).await
    }
    async fn post_user_message(
        &self,
        ctx: ActorCtx,
        cnv: ConversationId,
        msg: NewUserMessage,
    ) -> Result<Message, ConvError> {
        self.real.post_user_message(ctx, cnv, msg).await
    }
    async fn list_conversations(
        &self,
        project: &ProjectId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Conversation>, ConvError> {
        self.real.list_conversations(project, page).await
    }
    async fn list_messages(
        &self,
        cnv: &ConversationId,
        page: PageQuery,
    ) -> Result<PageWithSeq<Message>, ConvError> {
        let result = self.real.list_messages(cnv, page).await;
        self.read.notify_one();
        self.resume.notified().await;
        result
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_service_response_keeps_snapshot_when_writes_commit_before_response() {
    let h = harness(None);
    let viewer = device(12, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(viewer.clone());
    let real = real_conversation(&h);
    let (project, cnv) = real_project(&h, &real, 12).await;
    let initial = real_post(&real, cnv, "before").await;
    let old_head = h.store.events().head().await.unwrap();
    let pause = Arc::new(PauseAfterRead {
        real: real.clone(),
        read: Notify::new(),
        resume: Notify::new(),
    });
    let core = Arc::new(api_with_conversation(&h, pause.clone()));
    let request = {
        let core = core.clone();
        let viewer = viewer.clone();
        tokio::spawn(async move { api_get(&core, &viewer, &path(cnv)).await })
    };
    pause.read.notified().await;
    let new_message = real_post(&real, cnv, "after").await;
    // 直接追加真实 store 事件，验证 delivery_id 回填也受同一水位约束。
    let delivery = DeliveryId::from_parts(T0 as u64, 999).unwrap();
    let mut data = doubles::fixture_data("delivery.created");
    data["source_message_id"] = json!(initial.id);
    doubles::record(
        &h.store,
        EventType::DeliveryCreated,
        &delivery.to_string(),
        Some(project),
        data,
        None,
    )
    .await;
    let new_head = h.store.events().head().await.unwrap();
    assert!(new_head > old_head);
    pause.resume.notify_one();
    let before = request.await.unwrap();
    assert_eq!(before.status(), 200);
    assert_eq!(
        header(&before, "x-event-seq").unwrap(),
        old_head.as_u64().to_string()
    );
    assert_eq!(ids(&before), vec![initial.id.to_string()]);
    assert!(body(&before)["items"][0]["delivery_id"].is_null());
    let after = api_get(&api_with_conversation(&h, real), &viewer, &path(cnv)).await;
    assert_eq!(
        header(&after, "x-event-seq").unwrap(),
        new_head.as_u64().to_string()
    );
    let items = body(&after)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    assert!(items
        .iter()
        .any(|item| item["id"] == initial.id.to_string()
            && item["delivery_id"] == delivery.to_string()));
    assert!(items
        .iter()
        .any(|item| item["id"] == new_message.id.to_string() && item["delivery_id"].is_null()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_service_concurrent_responses_match_their_log_watermarks() {
    let h = harness(None);
    let viewer = device(13, DevicePlatform::Macos, DeviceRole::Viewer);
    h.devices.insert(viewer.clone());
    let real = real_conversation(&h);
    let (project, cnv) = real_project(&h, &real, 13).await;
    let first = real_post(&real, cnv, "initial").await;
    let first_seq = h.store.events().head().await.unwrap().as_u64();
    let core = Arc::new(api_with_conversation(&h, real.clone()));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let store = h.store.clone();
    let writer = {
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let mut messages = vec![(first.id, first_seq)];
            let delivery = DeliveryId::from_parts(T0 as u64, 1000).unwrap();
            let mut link_seq = 0;
            for n in 0..24 {
                let message = real_post(&real, cnv, &format!("concurrent {n}")).await;
                let seq = store.events().head().await.unwrap().as_u64();
                messages.push((message.id, seq));
                if n == 12 {
                    let mut data = doubles::fixture_data("delivery.created");
                    data["source_message_id"] = json!(first.id);
                    link_seq = doubles::record(
                        &store,
                        EventType::DeliveryCreated,
                        &delivery.to_string(),
                        Some(project),
                        data,
                        None,
                    )
                    .await
                    .as_u64();
                }
                tokio::task::yield_now().await;
            }
            (messages, delivery, link_seq)
        })
    };
    barrier.wait().await;
    let mut responses = Vec::new();
    for _ in 0..100 {
        let response = api_get(&core, &viewer, &path(cnv)).await;
        assert_eq!(response.status(), 200);
        let seq = header(&response, "x-event-seq")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        responses.push((seq, body(&response)["items"].as_array().unwrap().clone()));
        tokio::task::yield_now().await;
    }
    let (messages, delivery, link_seq) = writer.await.unwrap();
    assert!(link_seq > first_seq);
    for (seq, items) in responses {
        let expected = messages.iter().filter(|(_, posted)| *posted <= seq).count();
        assert_eq!(items.len(), expected, "response at seq {seq}");
        for (id, posted) in &messages {
            let actual = items.iter().find(|item| item["id"] == id.to_string());
            assert_eq!(
                actual.is_some(),
                *posted <= seq,
                "message {id} at seq {seq}"
            );
            if let Some(item) = actual {
                if *id == first.id && seq >= link_seq {
                    assert_eq!(item["delivery_id"], delivery.to_string());
                } else {
                    assert!(item["delivery_id"].is_null(), "message {id} at seq {seq}");
                }
            }
        }
    }
}
