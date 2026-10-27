//! 消息列表路由的分页、隔离与错误映射。
mod support;

use std::sync::Arc;

use async_trait::async_trait;
use gantry_api::{ApiConfig, ApiCore};
use gantry_core::entities::{Conversation, DevicePlatform, DeviceRole, Message};
use gantry_core::ids::{ConversationId, MessageId, ProjectId, Ulid};
use gantry_core::ports::context::{ActorCtx, PageOrder, PageQuery, PageWithSeq, StoreError};
use gantry_core::ports::conversation::{ConvError, ConversationService, NewUserMessage};
use gantry_core::testing::builders;
use gantry_fakes::conversation::ConversationCall;
use http::Method;
use support::*;

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
