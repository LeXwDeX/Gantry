#![cfg(feature = "testing")]

//! 内置 `devices` 投影与设备读取（store.md §6.2、§7.2）。

use std::sync::Arc;

use gantry_core::canon::Sha256Digest;
use gantry_core::entities::{Actor, ActorKind, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::events::{
    DeviceChanged, DevicePairedData, DeviceRevokedData, DeviceUpdatedData, EventType, NewEvent,
    RoleChange, Subject,
};
use gantry_core::ids::{DeviceId, Id, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_store::{Order, Page, PageCursor, Store, StoreDeps, StoreError};

const NOW_MS: i64 = 1_700_000_000_000;

async fn store() -> Store {
    let clock = Arc::new(FakeClock::new(NOW_MS));
    let ids = Arc::new(gantry_core::ids::IdGen::new(
        clock.clone(),
        Arc::new(SeqEntropy::new(7)),
    ));
    Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap()
}

fn device_id(n: u128) -> DeviceId {
    Id::from_parts(NOW_MS as u64, n).unwrap()
}

fn digest(n: u8) -> Sha256Digest {
    Sha256Digest::from_bytes([n; 32])
}

fn device_event(ty: EventType, id: DeviceId, data: impl serde::Serialize) -> NewEvent {
    NewEvent {
        ty,
        subject: Subject::parse(&id.to_string()).unwrap(),
        project_id: None,
        actor: Actor::new(ActorKind::System, "pki"),
        causation_id: None,
        correlation_id: None,
        data: serde_json::to_value(data).unwrap(),
        dedup_key: None,
    }
}

fn paired(n: u128, serial: &str, role: DeviceRole) -> NewEvent {
    let id = device_id(n);
    device_event(
        EventType::DevicePaired,
        id,
        DevicePairedData {
            device_id: id,
            name: format!("设备 {n}"),
            platform: DevicePlatform::Macos,
            role,
            cert_serial: serial.into(),
            pubkey_fpr: digest(n as u8),
            approval_pubkey: Some("BAAA".into()),
            approval_pubkey_fpr: Some(digest(100 + n as u8)),
        },
    )
}

fn updated(n: u128, changed: DeviceChanged) -> NewEvent {
    let id = device_id(n);
    device_event(
        EventType::DeviceUpdated,
        id,
        DeviceUpdatedData {
            device_id: Some(id),
            changed,
            via: None,
            host_uid: None,
            sudo_user: None,
        },
    )
}

fn revoked(n: u128, serials: &[&str]) -> NewEvent {
    let id = device_id(n);
    device_event(
        EventType::DeviceRevoked,
        id,
        DeviceRevokedData {
            device_id: id,
            cert_serials: serials.iter().map(|s| s.to_string()).collect(),
            remaining_approvers: 0,
            via: None,
        },
    )
}

async fn commit(store: &Store, events: Vec<NewEvent>) -> Result<(), StoreError> {
    let mut tx = store.begin_write().await?;
    for event in events {
        tx.append(event).await?;
    }
    tx.commit().await?;
    Ok(())
}

fn assert_precondition(result: Result<(), StoreError>) {
    let error = result.unwrap_err();
    assert!(
        matches!(error, StoreError::PreconditionFailed(_)),
        "{error}"
    );
}

#[tokio::test]
async fn paired_device_is_projected_and_readable_by_id_and_serial() {
    let store = store().await;
    commit(&store, vec![paired(1, "0a01", DeviceRole::Approver)])
        .await
        .unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    let device = snapshot.device(&device_id(1)).await.unwrap().unwrap();
    assert_eq!(device.name, "设备 1");
    assert_eq!(device.platform, DevicePlatform::Macos);
    assert_eq!(device.role, DeviceRole::Approver);
    assert_eq!(device.status, DeviceStatus::Active);
    assert_eq!(device.cert_serial, "0a01");
    assert_eq!(device.pubkey_fpr, digest(1));
    assert_eq!(device.approval_pubkey.as_deref(), Some("BAAA"));
    assert_eq!(device.approval_pubkey_fpr, Some(digest(101)));
    assert_eq!(device.last_seen, None);
    assert_eq!(device.created_at, Timestamp::from_millis(NOW_MS));
    assert_eq!(
        snapshot.device_by_cert_serial("0a01").await.unwrap(),
        Some(device)
    );
    assert_eq!(snapshot.device_by_cert_serial("0a02").await.unwrap(), None);
    assert!(snapshot.unprojected().is_empty());
}

#[tokio::test]
async fn pairing_twice_or_reusing_a_serial_is_rejected_atomically() {
    let store = store().await;
    commit(&store, vec![paired(1, "0a01", DeviceRole::Approver)])
        .await
        .unwrap();

    assert_precondition(commit(&store, vec![paired(1, "0a09", DeviceRole::Operator)]).await);
    // 同一事务里第一条合法、第二条与已有序列号冲突：整个事务回滚。
    assert_precondition(
        commit(
            &store,
            vec![
                paired(2, "0a02", DeviceRole::Operator),
                paired(3, "0a01", DeviceRole::Operator),
            ],
        )
        .await,
    );
    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.device(&device_id(2)).await.unwrap(), None);
    assert_eq!(snapshot.device_by_cert_serial("0a09").await.unwrap(), None);
    assert_eq!(
        snapshot
            .device_by_cert_serial("0a01")
            .await
            .unwrap()
            .map(|d| d.id),
        Some(device_id(1))
    );
}

#[tokio::test]
async fn updates_rename_change_role_and_move_the_serial_index() {
    let store = store().await;
    commit(
        &store,
        vec![
            paired(1, "0a01", DeviceRole::Approver),
            paired(2, "0a02", DeviceRole::Operator),
        ],
    )
    .await
    .unwrap();

    commit(
        &store,
        vec![
            updated(
                2,
                DeviceChanged {
                    name: Some("新名字".into()),
                    role: Some(RoleChange {
                        from: DeviceRole::Operator,
                        to: DeviceRole::Viewer,
                    }),
                    ..DeviceChanged::default()
                },
            ),
            updated(
                2,
                DeviceChanged {
                    cert_serial: Some("0b02".into()),
                    pubkey_fpr: Some(digest(42)),
                    ..DeviceChanged::default()
                },
            ),
        ],
    )
    .await
    .unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    let device = snapshot.device(&device_id(2)).await.unwrap().unwrap();
    assert_eq!(device.name, "新名字");
    assert_eq!(device.role, DeviceRole::Viewer);
    assert_eq!(device.cert_serial, "0b02");
    assert_eq!(device.pubkey_fpr, digest(42));
    // 旧序列号不再映射到设备（续期宽限由 pki 的证书表负责）。
    assert_eq!(snapshot.device_by_cert_serial("0a02").await.unwrap(), None);
    assert_eq!(
        snapshot.device_by_cert_serial("0b02").await.unwrap(),
        Some(device)
    );

    // 角色 from 与当前不符、换到别人正在用的序列号：都拒绝。
    assert_precondition(
        commit(
            &store,
            vec![updated(
                2,
                DeviceChanged {
                    role: Some(RoleChange {
                        from: DeviceRole::Operator,
                        to: DeviceRole::Approver,
                    }),
                    ..DeviceChanged::default()
                },
            )],
        )
        .await,
    );
    assert_precondition(
        commit(
            &store,
            vec![updated(
                2,
                DeviceChanged {
                    cert_serial: Some("0a01".into()),
                    ..DeviceChanged::default()
                },
            )],
        )
        .await,
    );
    // 未配对的设备不能更新。
    assert_precondition(
        commit(
            &store,
            vec![updated(
                9,
                DeviceChanged {
                    name: Some("x".into()),
                    ..DeviceChanged::default()
                },
            )],
        )
        .await,
    );
}

#[tokio::test]
async fn revocation_is_final_and_keeps_the_serial_taken() {
    let store = store().await;
    commit(&store, vec![paired(1, "0a01", DeviceRole::Approver)])
        .await
        .unwrap();
    commit(&store, vec![revoked(1, &["0a01"])]).await.unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    let device = snapshot
        .device_by_cert_serial("0a01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(device.status, DeviceStatus::Revoked);
    drop(snapshot);

    assert_precondition(commit(&store, vec![revoked(1, &["0a01"])]).await);
    assert_precondition(
        commit(
            &store,
            vec![updated(
                1,
                DeviceChanged {
                    name: Some("复活".into()),
                    ..DeviceChanged::default()
                },
            )],
        )
        .await,
    );
    // 已吊销设备的序列号仍占用 UNIQUE(cert_serial)。
    assert_precondition(commit(&store, vec![paired(2, "0a01", DeviceRole::Operator)]).await);
}

#[tokio::test]
async fn inconsistent_device_events_are_invalid() {
    let store = store().await;
    let mut wrong_subject = paired(1, "0a01", DeviceRole::Approver);
    wrong_subject.subject = Subject::parse(&device_id(2).to_string()).unwrap();
    let error = commit(&store, vec![wrong_subject]).await.unwrap_err();
    assert!(matches!(error, StoreError::InvalidEvent(_)), "{error}");

    let mut scoped = paired(1, "0a01", DeviceRole::Approver);
    let project: ProjectId = Id::from_parts(NOW_MS as u64, 5).unwrap();
    scoped.project_id = Some(project);
    let error = commit(&store, vec![scoped]).await.unwrap_err();
    assert!(matches!(error, StoreError::InvalidEvent(_)), "{error}");

    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.device(&device_id(1)).await.unwrap(), None);
}

#[tokio::test]
async fn transaction_reader_sees_uncommitted_devices_and_rollback_discards_them() {
    let store = store().await;
    let mut tx = store.begin_write().await.unwrap();
    tx.append(paired(1, "0a01", DeviceRole::Approver))
        .await
        .unwrap();
    assert!(tx.read().device(&device_id(1)).await.unwrap().is_some());
    assert!(tx
        .read()
        .device_by_cert_serial("0a01")
        .await
        .unwrap()
        .is_some());
    tx.rollback().await.unwrap();

    let mut snapshot = store.snapshot().await.unwrap();
    assert_eq!(snapshot.device(&device_id(1)).await.unwrap(), None);
}

#[tokio::test]
async fn devices_are_listed_with_keyset_pagination_including_revoked() {
    let store = store().await;
    commit(
        &store,
        vec![
            paired(1, "01", DeviceRole::Approver),
            paired(2, "02", DeviceRole::Operator),
            paired(3, "03", DeviceRole::Operator),
            revoked(2, &["02"]),
        ],
    )
    .await
    .unwrap();
    let mut snapshot = store.snapshot().await.unwrap();

    let first = snapshot
        .devices(&Page {
            limit: 2,
            ..Page::default()
        })
        .await
        .unwrap();
    let ids: Vec<_> = first.items.iter().map(|d| d.id).collect();
    assert_eq!(ids, vec![device_id(1), device_id(2)]);
    assert_eq!(first.items[1].status, DeviceStatus::Revoked);
    let second = snapshot
        .devices(&Page {
            limit: 2,
            cursor: first.next_cursor.clone(),
            ..Page::default()
        })
        .await
        .unwrap();
    assert_eq!(
        second.items.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![device_id(3)]
    );
    assert_eq!(second.next_cursor, None);

    let desc = snapshot
        .devices(&Page {
            limit: 200,
            order: Order::Desc,
            ..Page::default()
        })
        .await
        .unwrap();
    assert_eq!(
        desc.items.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![device_id(3), device_id(2), device_id(1)]
    );

    // 方向不符、别的资源的游标、篡改的游标、越界的 limit。
    for (cursor, order) in [
        (first.next_cursor.clone().unwrap(), Order::Desc),
        (
            PageCursor::new(format!("inbox:asc:{}", device_id(1))),
            Order::Asc,
        ),
        (PageCursor::new("devices:asc:not-an-id"), Order::Asc),
    ] {
        let error = snapshot
            .devices(&Page {
                limit: 2,
                cursor: Some(cursor),
                order,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::InvalidCursor), "{error}");
    }
    let error = snapshot
        .devices(&Page {
            limit: 0,
            ..Page::default()
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::PreconditionFailed(_)),
        "{error}"
    );
}
