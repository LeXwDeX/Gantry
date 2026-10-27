#![cfg(feature = "testing")]

use std::sync::Arc;

use gantry_core::canon::Sha256Digest;
use gantry_core::entities::{
    Actor, ActorKind, DevicePlatform, DeviceRole, InboxItem, InboxKind, InboxPayload, InboxStatus,
    Severity,
};
use gantry_core::events::{DevicePairedData, EventType, InboxRaisedData, NewEvent, Subject};
use gantry_core::ids::{ApprovalId, DeviceId, Id, InboxId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_store::{InboxQuery, Order, Page, PageCursor, Store, StoreDeps, StoreError};

const NOW_MS: i64 = 1_700_000_000_000;

#[derive(Clone, Copy, Debug)]
enum Resource {
    Inbox,
    Devices,
}

fn inbox_id(n: u128) -> InboxId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn device_id(n: u128) -> DeviceId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn id(resource: Resource, n: u128) -> String {
    match resource {
        Resource::Inbox => inbox_id(n).to_string(),
        Resource::Devices => device_id(n).to_string(),
    }
}

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(7)),
    ));
    Store::open_temp(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn event(resource: Resource, n: u128) -> NewEvent {
    let (ty, subject, actor, data) = match resource {
        Resource::Inbox => {
            let id = inbox_id(n);
            let approval_id: ApprovalId = Id::from_parts(NOW_MS as u64, n).unwrap();
            let item = InboxItem {
                id,
                project_id: None,
                delivery_id: None,
                kind: InboxKind::ApprovalNeeded,
                severity: Severity::Info,
                summary: "需要审批".into(),
                payload: InboxPayload::ApprovalNeeded { approval_id },
                status: InboxStatus::Open,
                dedup_key: format!("approval:{n}"),
                created_at: Timestamp::from_millis(NOW_MS),
                updated_at: Timestamp::from_millis(NOW_MS),
            };
            (
                EventType::InboxRaised,
                id.to_string(),
                Actor::new(ActorKind::Engine, "engine"),
                serde_json::to_value(InboxRaisedData {
                    item,
                    supersedes: vec![],
                })
                .unwrap(),
            )
        }
        Resource::Devices => {
            let id = device_id(n);
            (
                EventType::DevicePaired,
                id.to_string(),
                Actor::new(ActorKind::System, "pki"),
                serde_json::to_value(DevicePairedData {
                    device_id: id,
                    name: format!("设备 {n}"),
                    platform: DevicePlatform::Macos,
                    role: DeviceRole::Approver,
                    cert_serial: format!("{n:04x}"),
                    pubkey_fpr: Sha256Digest::from_bytes([n as u8; 32]),
                    approval_pubkey: Some("BAAA".into()),
                    approval_pubkey_fpr: Some(Sha256Digest::from_bytes([100 + n as u8; 32])),
                })
                .unwrap(),
            )
        }
    };
    NewEvent {
        ty,
        subject: Subject::parse(&subject).unwrap(),
        project_id: None,
        actor,
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

async fn insert(store: &Store, resource: Resource, numbers: &[u128]) {
    let mut tx = store.begin_write().await.unwrap();
    for &n in numbers {
        tx.append(event(resource, n)).await.unwrap();
    }
    tx.commit().await.unwrap();
}

async fn list(
    store: &Store,
    resource: Resource,
    page: &Page,
) -> Result<(Vec<String>, Option<PageCursor>), StoreError> {
    let mut snapshot = store.snapshot().await?;
    match resource {
        Resource::Inbox => {
            let result = snapshot.inbox(&InboxQuery::default(), page).await?;
            Ok((
                result
                    .items
                    .into_iter()
                    .map(|item| item.id.to_string())
                    .collect(),
                result.next_cursor,
            ))
        }
        Resource::Devices => {
            let result = snapshot.devices(page).await?;
            Ok((
                result
                    .items
                    .into_iter()
                    .map(|item| item.id.to_string())
                    .collect(),
                result.next_cursor,
            ))
        }
    }
}

async fn assert_invalid(store: &Store, resource: Resource, order: Order, cursor: PageCursor) {
    let result = list(
        store,
        resource,
        &Page {
            limit: 1,
            order,
            cursor: Some(cursor.clone()),
        },
    )
    .await;
    assert!(
        matches!(result, Err(StoreError::InvalidCursor)),
        "{resource:?} {order:?} cursor {cursor}: {result:?}"
    );
}

#[tokio::test]
async fn real_cursors_reject_cross_resource_direction_and_malformed_values() {
    let store = store().await;
    for resource in [Resource::Inbox, Resource::Devices] {
        insert(&store, resource, &[1, 2]).await;
    }

    for order in [Order::Asc, Order::Desc] {
        let page = Page {
            limit: 1,
            order,
            cursor: None,
        };
        let inbox = list(&store, Resource::Inbox, &page)
            .await
            .unwrap()
            .1
            .unwrap();
        let devices = list(&store, Resource::Devices, &page)
            .await
            .unwrap()
            .1
            .unwrap();
        assert_invalid(&store, Resource::Inbox, order, devices.clone()).await;
        assert_invalid(&store, Resource::Devices, order, inbox.clone()).await;

        for (resource, cursor, other) in [
            (Resource::Inbox, inbox, Resource::Devices),
            (Resource::Devices, devices, Resource::Inbox),
        ] {
            let opposite = match order {
                Order::Asc => Order::Desc,
                Order::Desc => Order::Asc,
            };
            assert_invalid(&store, resource, opposite, cursor.clone()).await;
            let parts: Vec<_> = cursor.as_str().split(':').collect();
            assert_eq!(parts.len(), 3);
            let wrong_id = format!("{}:{}:{}", parts[0], parts[1], id(other, 1));
            assert_invalid(&store, resource, order, PageCursor::new(wrong_id)).await;

            // 固定种子的有限变异只破坏结构或标签，不假设合法 ID 受到签名保护。
            let mut seed = 0x267_u64;
            for i in 0..64 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let value = match i % 4 {
                    0 => {
                        cursor.as_str()[..(seed as usize % (cursor.as_str().len() - 1))].to_string()
                    }
                    1 => format!("{}#:{}:{}", parts[0], parts[1], parts[2]),
                    2 => format!("{}:{}#:{}", parts[0], parts[1], parts[2]),
                    _ => {
                        let mut bytes = cursor.as_str().as_bytes().to_vec();
                        let key_start = parts[0].len() + parts[1].len() + 2;
                        let pos = key_start + seed as usize % (bytes.len() - key_start);
                        bytes[pos] = b'#';
                        String::from_utf8(bytes).unwrap()
                    }
                };
                assert_invalid(&store, resource, order, PageCursor::new(value)).await;
            }
        }
    }
}

#[tokio::test]
async fn keyset_pages_handle_new_larger_ids_in_both_directions() {
    for resource in [Resource::Inbox, Resource::Devices] {
        for order in [Order::Asc, Order::Desc] {
            for limit in [1, 2] {
                let store = store().await;
                insert(&store, resource, &[1, 2, 3]).await;
                let mut page = Page {
                    limit,
                    order,
                    cursor: None,
                };
                let (first, next) = list(&store, resource, &page).await.unwrap();
                assert!(next.is_some(), "{resource:?} {order:?} {limit}");
                insert(&store, resource, &[4]).await;

                let expected_numbers: Vec<_> = match order {
                    Order::Asc => vec![1, 2, 3, 4],
                    Order::Desc => vec![3, 2, 1],
                };
                let expected: Vec<_> = expected_numbers
                    .into_iter()
                    .map(|n| id(resource, n))
                    .collect();
                assert_eq!(first, expected[..limit as usize]);
                let mut seen = first;
                page.cursor = next;
                while page.cursor.is_some() {
                    let (items, next) = list(&store, resource, &page).await.unwrap();
                    let remaining = expected.len() - seen.len();
                    assert_eq!(items.len(), remaining.min(limit as usize));
                    assert_eq!(next.is_some(), remaining > limit as usize);
                    seen.extend(items);
                    page.cursor = next;
                }
                assert_eq!(seen, expected, "{resource:?} {order:?} {limit}");
            }
        }
    }
}
