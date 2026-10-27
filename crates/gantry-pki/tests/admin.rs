//! pki.md §12.2「设备管理与吊销」与「续期」中设备叶子的部分：改角色、吊销（含
//! `revocations.journal`）、改名、设备证书续期、恢复后重放。

mod common;

use std::fs;

use common::{Env, Keys, DAY_MS};
use gantry_core::entities::{Actor, ActorKind, DevicePlatform, DeviceRole, DeviceStatus};
use gantry_core::events::{DeviceRevokedData, DeviceUpdatedData, EventType, RoleChange, Subject};
use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use gantry_pki::admin::{Audit, DeviceAdmin, DeviceError, RenewError, RenewRequest, RevokeOutcome};
use gantry_pki::directory::DeviceDirectory;
use gantry_pki::journal::{JournalEntry, JOURNAL_VERSION};
use gantry_pki::pairing_service::PairResponse;
use gantry_pki::store_schema::CertRowStatus;
use gantry_pki::verifier::{CertStatusLookup, DeviceCertStatus};
use gantry_pki::{CertSerial, IssuedCert};
use serde_json::Value;

fn device_actor(id: &DeviceId) -> Actor {
    Actor::new(ActorKind::Device, id.to_string())
}

async fn events(env: &Env, id: &DeviceId) -> Vec<(EventType, Value)> {
    env.store
        .events()
        .by_subject(&Subject::parse(&id.to_string()).unwrap())
        .await
        .unwrap()
        .into_iter()
        .map(|event| (event.ty, event.data))
        .collect()
}

fn serial_of(device: &PairResponse) -> CertSerial {
    device.device.cert_serial.parse().unwrap()
}

async fn set_role(
    env: &Env,
    id: &DeviceId,
    role: DeviceRole,
    actor: &Actor,
) -> Result<gantry_core::entities::Device, DeviceError> {
    let mut tx = env.store.begin_write().await.unwrap();
    let result = env.admin.set_role(&mut tx, id, role, actor).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn revoke(env: &Env, id: &DeviceId, actor: &Actor) -> Result<RevokeOutcome, DeviceError> {
    let mut tx = env.store.begin_write().await.unwrap();
    let result = env.admin.revoke(&mut tx, id, actor).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

// ---------------------------------------------------------------------------
// 改角色
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_role_takes_effect_on_the_next_lookup_and_guards_invariants() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let other = env.pair(DevicePlatform::Windows).await;
    let phone = env.pair(DevicePlatform::Ios).await;
    let by = device_actor(&approver.device.id);

    let updated = set_role(&env, &other.device.id, DeviceRole::Viewer, &by)
        .await
        .unwrap();
    assert_eq!(updated.role, DeviceRole::Viewer);
    let seen = env
        .directory
        .by_cert_serial(&serial_of(&other))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.role, DeviceRole::Viewer);
    let (ty, data) = events(&env, &other.device.id).await.pop().unwrap();
    assert_eq!(ty, EventType::DeviceUpdated);
    let data: DeviceUpdatedData = serde_json::from_value(data).unwrap();
    assert_eq!(
        data.changed.role,
        Some(RoleChange {
            from: DeviceRole::Operator,
            to: DeviceRole::Viewer
        })
    );

    // 同一角色：不发事件。
    let before = events(&env, &other.device.id).await.len();
    set_role(&env, &other.device.id, DeviceRole::Viewer, &by)
        .await
        .unwrap();
    assert_eq!(events(&env, &other.device.id).await.len(), before);

    assert!(matches!(
        set_role(&env, &approver.device.id, DeviceRole::Operator, &by).await,
        Err(DeviceError::SelfRoleChange)
    ));
    assert!(matches!(
        set_role(&env, &phone.device.id, DeviceRole::Approver, &by).await,
        Err(DeviceError::MobileCannotApprove)
    ));
    let unknown: DeviceId = "dev_01J00000000000000000000000".parse().unwrap();
    assert!(matches!(
        set_role(&env, &unknown, DeviceRole::Viewer, &by).await,
        Err(DeviceError::NotFound)
    ));
}

#[tokio::test]
async fn rolled_back_caller_transaction_leaves_no_trace() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let other = env.pair(DevicePlatform::Windows).await;
    let by = device_actor(&approver.device.id);
    let head = env.store.snapshot().await.unwrap().head_seq();

    let mut tx = env.store.begin_write().await.unwrap();
    env.admin
        .set_role(&mut tx, &other.device.id, DeviceRole::Approver, &by)
        .await
        .unwrap();
    env.admin
        .revoke(&mut tx, &approver.device.id, &by)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(env.store.snapshot().await.unwrap().head_seq(), head);
    let other_now = env.directory.get(&other.device.id).await.unwrap().unwrap();
    assert_eq!(other_now.role, DeviceRole::Operator);
    let approver_now = env
        .directory
        .get(&approver.device.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approver_now.status, DeviceStatus::Active);
    assert_eq!(
        env.directory
            .lookup(&serial_of(&approver))
            .unwrap()
            .unwrap()
            .status,
        DeviceCertStatus::Active
    );
    // 日志里多出的一行保留（偏向安全，§7.6）；Pki::open 的核对会据此要求重放。
    assert_eq!(env.journal.read_all().unwrap().len(), 1);
    assert_eq!(
        env.admin.pending_revocations().await.unwrap(),
        vec![approver.device.id]
    );
}

// ---------------------------------------------------------------------------
// 吊销
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_marks_device_and_every_certificate_and_journals_once() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let target = env.pair(DevicePlatform::Windows).await;
    // 先续期一次，让目标设备有新旧两个序列号。
    let renewed = env
        .admin
        .renew(
            &target.device.id,
            &serial_of(&target),
            RenewRequest {
                csr_pem: Keys::new().csr(),
            },
        )
        .await
        .unwrap();
    let new_serial = IssuedCert::from_der(
        pem::parse(&renewed.certificate_chain[0])
            .unwrap()
            .into_contents(),
    )
    .unwrap()
    .serial;
    let by = device_actor(&approver.device.id);

    let outcome = revoke(&env, &target.device.id, &by).await.unwrap();
    let RevokeOutcome::Revoked(device) = outcome else {
        panic!("expected Revoked");
    };
    assert_eq!(device.status, DeviceStatus::Revoked);
    for serial in [serial_of(&target), new_serial.clone()] {
        let record = env.directory.lookup(&serial).unwrap().unwrap();
        assert_eq!(record.status, DeviceCertStatus::Revoked);
        assert!(!record.device_active);
    }
    let (ty, data) = events(&env, &target.device.id).await.pop().unwrap();
    assert_eq!(ty, EventType::DeviceRevoked);
    let data: DeviceRevokedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.remaining_approvers, 1);
    let mut serials = data.cert_serials.clone();
    serials.sort();
    let mut expected = vec![serial_of(&target).to_string(), new_serial.to_string()];
    expected.sort();
    assert_eq!(serials, expected);

    // 日志：本地与备份目标各一行，内容与事件一致。
    let entries = env.journal.read_all().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.device_id, target.device.id);
    assert_eq!(entry.v, JOURNAL_VERSION);
    assert_eq!(entry.actor, by);
    assert_eq!(entry.approval_pubkey_fpr, target.device.approval_pubkey_fpr);
    assert_eq!(
        fs::read_to_string(env.journal.local_path())
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        fs::read_to_string(env.journal.backup_path())
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        env.directory
            .verifying_key(&target.device.id)
            .await
            .unwrap(),
        None
    );

    // 重复吊销：AlreadyRevoked，不发事件、不再写日志。
    let before = events(&env, &target.device.id).await.len();
    assert!(matches!(
        revoke(&env, &target.device.id, &by).await.unwrap(),
        RevokeOutcome::AlreadyRevoked(_)
    ));
    assert_eq!(events(&env, &target.device.id).await.len(), before);
    assert_eq!(env.journal.read_all().unwrap().len(), 1);

    // 已吊销的设备不能再改名 / 改角色 / 续期。
    assert!(matches!(
        env.admin.rename(&target.device.id, "x", &by).await,
        Err(DeviceError::Revoked)
    ));
    assert!(matches!(
        env.admin
            .renew(
                &target.device.id,
                &new_serial,
                RenewRequest {
                    csr_pem: Keys::new().csr()
                }
            )
            .await,
        Err(RenewError::Revoked)
    ));
}

#[tokio::test]
async fn a_device_may_revoke_itself_even_as_the_last_approver() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let me = device_actor(&approver.device.id);
    revoke(&env, &approver.device.id, &me).await.unwrap();
    let (_, data) = events(&env, &approver.device.id).await.pop().unwrap();
    let data: DeviceRevokedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.remaining_approvers, 0);
}

#[tokio::test]
async fn journal_failure_aborts_the_revocation() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let target = env.pair(DevicePlatform::Windows).await;
    // 备份目标不可写：把它换成一个目录。
    fs::create_dir_all(env.journal.backup_path()).unwrap();

    let mut tx = env.store.begin_write().await.unwrap();
    let result = env
        .admin
        .revoke(
            &mut tx,
            &target.device.id,
            &device_actor(&approver.device.id),
        )
        .await;
    assert!(matches!(result, Err(DeviceError::Journal(_))), "{result:?}");
    tx.rollback().await.unwrap();
    assert_eq!(
        env.directory
            .get(&target.device.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DeviceStatus::Active
    );
}

// ---------------------------------------------------------------------------
// 改名
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rename_validates_and_emits_device_updated() {
    let env = Env::new().await;
    let device = env.pair(DevicePlatform::Macos).await;
    let by = device_actor(&device.device.id);
    let renamed = env
        .admin
        .rename(&device.device.id, "工作机", &by)
        .await
        .unwrap();
    assert_eq!(renamed.name, "工作机");
    let (ty, data) = events(&env, &device.device.id).await.pop().unwrap();
    assert_eq!(ty, EventType::DeviceUpdated);
    let data: DeviceUpdatedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.changed.name.as_deref(), Some("工作机"));

    for bad in ["", "a\u{7}b", &"x".repeat(65)] {
        assert!(matches!(
            env.admin.rename(&device.device.id, bad, &by).await,
            Err(DeviceError::Validation(_))
        ));
    }
}

// ---------------------------------------------------------------------------
// 设备证书续期（P-64）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn renew_keeps_or_rotates_the_client_key_and_supersedes_the_old_certificate() {
    let env = Env::new().await;
    let keys = Keys::new();
    let paired = env
        .pairing
        .redeem(keys.request(&env.token().await, DevicePlatform::Macos))
        .await
        .unwrap();
    let id = paired.device.id;
    let old_serial = serial_of(&paired);

    // 沿用旧密钥：新序列号，pubkey_fpr 不变。
    let same = env
        .admin
        .renew(
            &id,
            &old_serial,
            RenewRequest {
                csr_pem: keys.csr(),
            },
        )
        .await
        .unwrap();
    assert_eq!(same.certificate_chain[2], paired.certificate_chain[2]);
    let leaf = IssuedCert::from_der(
        pem::parse(&same.certificate_chain[0])
            .unwrap()
            .into_contents(),
    )
    .unwrap();
    assert_ne!(leaf.serial, old_serial);
    assert_eq!(same.not_after, leaf.not_after);
    let device = env.directory.get(&id).await.unwrap().unwrap();
    assert_eq!(device.cert_serial, leaf.serial.to_string());
    assert_eq!(device.pubkey_fpr, paired.device.pubkey_fpr);
    assert_eq!(device.approval_pubkey, paired.device.approval_pubkey);
    let (_, data) = events(&env, &id).await.pop().unwrap();
    let data: DeviceUpdatedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.changed.cert_serial, Some(leaf.serial.to_string()));
    assert_eq!(data.changed.pubkey_fpr, None);

    // 旧证书被取代：原 notAfter 之前仍能握手 / 映射，之后不行。
    let old_row = env.cert_row(old_serial.as_str()).await.unwrap();
    assert_eq!(old_row.status, CertRowStatus::Superseded);
    assert!(env
        .directory
        .by_cert_serial(&old_serial)
        .await
        .unwrap()
        .is_some());

    // 换新 mTLS 密钥：pubkey_fpr 变化，审批公钥不变；可用被取代但未过期的旧证书发起。
    let new_keys = Keys::new();
    env.admin
        .renew(
            &id,
            &old_serial,
            RenewRequest {
                csr_pem: new_keys.csr(),
            },
        )
        .await
        .unwrap();
    let device = env.directory.get(&id).await.unwrap().unwrap();
    assert_eq!(device.pubkey_fpr, new_keys.client_public().fingerprint());
    assert_eq!(
        device.approval_pubkey_fpr,
        paired.device.approval_pubkey_fpr
    );
    let (_, data) = events(&env, &id).await.pop().unwrap();
    let data: DeviceUpdatedData = serde_json::from_value(data).unwrap();
    assert_eq!(
        data.changed.pubkey_fpr,
        Some(new_keys.client_public().fingerprint())
    );

    env.clock.set(old_row.not_after);
    assert_eq!(
        env.directory.by_cert_serial(&old_serial).await.unwrap(),
        None
    );
    assert!(matches!(
        env.admin
            .renew(
                &id,
                &old_serial,
                RenewRequest {
                    csr_pem: keys.csr()
                }
            )
            .await,
        Err(RenewError::Revoked)
    ));
}

#[tokio::test]
async fn renew_rejects_bad_csrs_and_foreign_serials() {
    let env = Env::new().await;
    let keys = Keys::new();
    let a = env
        .pairing
        .redeem(keys.request(&env.token().await, DevicePlatform::Macos))
        .await
        .unwrap();
    let b = env.pair(DevicePlatform::Windows).await;

    let invalid = env
        .admin
        .renew(
            &a.device.id,
            &serial_of(&a),
            RenewRequest {
                csr_pem: "garbage".into(),
            },
        )
        .await;
    assert!(matches!(invalid, Err(RenewError::Validation(_))));
    // 审批密钥不能拿来当 mTLS 密钥（两把密钥必须分开，P-47）——这里用 b 的 mTLS 公钥重复来覆盖"已属其他 active 设备"。
    let dup = env
        .admin
        .renew(
            &b.device.id,
            &serial_of(&b),
            RenewRequest {
                csr_pem: keys.csr(),
            },
        )
        .await;
    assert!(matches!(dup, Err(RenewError::Validation(_))));
    // 拿别的设备的序列号续期 → 不在册。
    assert!(matches!(
        env.admin
            .renew(
                &a.device.id,
                &serial_of(&b),
                RenewRequest {
                    csr_pem: Keys::new().csr()
                }
            )
            .await,
        Err(RenewError::Revoked)
    ));
    // 失败不留痕：序列号仍是原来的。
    assert_eq!(
        env.directory
            .get(&a.device.id)
            .await
            .unwrap()
            .unwrap()
            .cert_serial,
        a.device.cert_serial
    );
}

// ---------------------------------------------------------------------------
// 恢复后重放（P-67、P-80）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_revokes_devices_journaled_after_the_backup() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let a = env.pair(DevicePlatform::Windows).await;
    let b = env.pair(DevicePlatform::Windows).await;
    let backup_at = Timestamp::from_millis(env.clock.now_timestamp().as_millis() - DAY_MS);

    // 模拟"备份之后吊销、恢复回到吊销之前"：日志里有 a 的吊销与一台库中不存在的设备，
    // 库里 a 仍是 active；b 在库里已吊销。
    revoke(&env, &b.device.id, &device_actor(&approver.device.id))
        .await
        .unwrap();
    let ghost: DeviceId = "dev_01J00000000000000000000099".parse().unwrap();
    let identity = env.store.log_identity();
    for (device_id, serial) in [(a.device.id, serial_of(&a)), (ghost, "7f".parse().unwrap())] {
        env.journal
            .append(&JournalEntry {
                v: JOURNAL_VERSION,
                device_id,
                cert_serials: vec![serial],
                approval_pubkey_fpr: None,
                revoked_at: env.clock.now_timestamp(),
                actor: Actor::new(ActorKind::Device, approver.device.id.to_string()),
                via: None,
                log_id: identity.log_id.clone(),
                log_epoch: identity.log_epoch,
            })
            .unwrap();
    }
    let mut pending = env.admin.pending_revocations().await.unwrap();
    pending.sort();
    assert_eq!(pending, vec![a.device.id]);

    let report = env.admin.replay_revocations(backup_at).await.unwrap();
    assert_eq!(report.replayed, vec![a.device.id]);
    assert_eq!(report.already_revoked, 1);
    assert_eq!(report.unknown_devices, 1);
    assert!(env.admin.pending_revocations().await.unwrap().is_empty());
    assert!(
        !env.directory
            .lookup(&serial_of(&a))
            .unwrap()
            .unwrap()
            .device_active
    );
    let (ty, data) = events(&env, &a.device.id).await.pop().unwrap();
    assert_eq!(ty, EventType::DeviceRevoked);
    let data: DeviceRevokedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.via.as_deref(), Some("journal_replay"));
    // 重放不再写日志；再次重放幂等。
    assert_eq!(env.journal.read_all().unwrap().len(), 3);
    let again = env.admin.replay_revocations(backup_at).await.unwrap();
    assert!(again.replayed.is_empty());
    assert_eq!(again.already_revoked, 2);

    // 早于备份时刻的条目不重放。
    let later = Timestamp::from_millis(env.clock.now_timestamp().as_millis() + 1);
    let none = env.admin.replay_revocations(later).await.unwrap();
    assert_eq!(none, Default::default());
}

#[tokio::test]
async fn host_recover_audit_fields_are_recorded() {
    let env = Env::new().await;
    let approver = env.pair(DevicePlatform::Macos).await;
    let other = env.pair(DevicePlatform::Windows).await;
    let host = Actor::new(ActorKind::System, "host_recover");
    let audit = Audit {
        via: Some("host_recover".into()),
        host_uid: Some(0),
        sudo_user: Some("lex".into()),
    };
    let mut tx = env.store.begin_write().await.unwrap();
    env.admin
        .set_role_with(
            &mut tx,
            &other.device.id,
            DeviceRole::Approver,
            &host,
            audit,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (_, data) = events(&env, &other.device.id).await.pop().unwrap();
    let data: DeviceUpdatedData = serde_json::from_value(data).unwrap();
    assert_eq!(data.via.as_deref(), Some("host_recover"));
    assert_eq!(data.host_uid, Some(0));
    assert_eq!(data.sudo_user.as_deref(), Some("lex"));
    assert_eq!(approver.device.role, DeviceRole::Approver);
}
