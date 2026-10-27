//! 配对服务（pki.md §6.3、§7.4）：主机侧开 / 关会话，网络侧 `redeem`。
//!
//! `redeem` 在**一个**写事务里完成：令牌核验、首台角色判定、签发、追加 `device.paired`、
//! 写 `pki_device_certs` / `pki_device_state`、会话置 `redeemed`。要么全部可见，要么都不可见。
//! 令牌只在签发成功后失效；校验失败不消耗令牌，但未匹配的令牌会让当前会话的失败次数 +1
//! （这次计数单独提交，否则攻击者可无限猜测）。

use std::sync::Arc;
use std::time::Duration;

use gantry_core::entities::{Actor, ActorKind, Device, DevicePlatform, DeviceStatus};
use gantry_core::events::{DevicePairedData, EventType, NewEvent, Subject};
use gantry_core::ids::{Clock, Dev, Entropy, IdGen};
use gantry_core::secret::SecretString;
use gantry_core::time::Timestamp;
use gantry_store::{Page, Store, WriteTx};
use tokio::sync::watch;

use crate::ca::{Authority, IssuedCert};
use crate::config::CertPolicy;
use crate::csr::{parse_approval_pubkey, validate_csr};
use crate::directory::{desktop_ever_paired, encode_spki, is_desktop, store_error};
use crate::keys::{CertSerial, P256PublicKey};
use crate::pairing::{
    confirmation_code, initial_role, PairingSession, PairingSessionId, PairingWindow, SessionError,
    SessionTable, TokenInvalidReason,
};
use crate::store_schema::{
    CertRowStatus, DeviceCertRow, DeviceStateRow, DEVICE_CERTS, DEVICE_STATE, MODULE,
    PAIRING_SESSIONS, SESSIONS_KEY,
};
use crate::text::validate_device_name;
use crate::PkiError;

/// 配对参数（pki.md §10）；默认值全部是建议值。
#[derive(Clone, Debug)]
pub struct PairingConfig {
    /// `pki.pair_token_ttl`。
    pub token_ttl: Duration,
    /// `pki.pair_max_failures`。
    pub max_failures: u32,
    /// `pki.max_csr_bytes`。
    pub max_csr_bytes: usize,
    /// `pki.device_name_max_chars`。
    pub device_name_max_chars: usize,
    /// `pki.api_base_url`，写进配对响应。
    pub api_base_url: String,
    pub cert_policy: CertPolicy,
}

impl PairingConfig {
    pub fn new(api_base_url: impl Into<String>) -> Self {
        Self {
            token_ttl: Duration::from_secs(10 * 60),
            max_failures: 5,
            max_csr_bytes: 4096,
            device_name_max_chars: 64,
            api_base_url: api_base_url.into(),
            cert_policy: CertPolicy::default(),
        }
    }
}

/// 签发方：在线中间 CA（私钥经 `secrets` 读取后载入）与离线根 CA 的证书。
pub struct Issuer {
    intermediate: Authority,
    root: IssuedCert,
}

impl Issuer {
    pub fn new(intermediate: Authority, root: IssuedCert) -> Self {
        Self { intermediate, root }
    }

    /// 配对串与响应里的 `ca_fingerprint`：离线根 CA 证书 DER 的 SHA-256（P-64）。
    pub fn ca_fingerprint(&self) -> String {
        format!("sha256:{}", self.root.fingerprint().to_hex())
    }

    /// `[叶子, 中间 CA, 根]`（P-64）。
    pub fn chain(&self, leaf: &IssuedCert) -> [String; 3] {
        [
            leaf.pem.clone(),
            self.intermediate.certificate().pem.clone(),
            self.root.pem.clone(),
        ]
    }

    pub(crate) fn intermediate(&self) -> &Authority {
        &self.intermediate
    }
}

/// `open_session` 的结果；明文令牌只在这里出现一次。
#[derive(Debug)]
pub struct PairingTicket {
    pub session_id: PairingSessionId,
    pub token: SecretString,
    pub ca_fingerprint: String,
    pub expires_at: Timestamp,
}

/// `POST /v1/pair` 的请求体（`api` 已做严格解析与大小限制，§7.4.3 第 1 步）。
#[derive(Clone, Debug)]
pub struct PairRequest {
    pub token: String,
    pub csr_pem: String,
    /// P-256 未压缩点的 base64url（无填充）；仅 `platform = ios` 时可省略（P-76）。
    pub approval_pubkey: Option<String>,
    pub device: NewDevice,
}

#[derive(Clone, Debug)]
pub struct NewDevice {
    pub name: String,
    pub platform: DevicePlatform,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairResponse {
    pub device: Device,
    /// `[叶子 PEM, 中间 CA PEM, 根 PEM]`。
    pub certificate_chain: [String; 3],
    pub api_base_url: String,
    pub ca_fingerprint: String,
    /// 6 位数字。
    pub confirmation_code: String,
}

/// 字段级校验错误（`422 validation_failed` 的 `errors[]`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub pointer: &'static str,
    pub code: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// 无效 / 已用 / 过期 / 作废：对外统一 `401 pair_token_invalid`。原因只供日志与指标。
    #[error("pairing token invalid")]
    TokenInvalid(TokenInvalidReason),
    #[error("pairing request invalid: {0:?}")]
    Validation(Vec<FieldError>),
    /// `500`，细节只进日志。
    #[error("pairing failed: {0}")]
    Internal(#[from] PkiError),
}

pub struct PairingService {
    store: Store,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    ids: Arc<IdGen>,
    issuer: Arc<Issuer>,
    config: PairingConfig,
    window: watch::Sender<Option<PairingWindow>>,
}

impl PairingService {
    pub fn new(
        store: Store,
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
        issuer: Arc<Issuer>,
        config: PairingConfig,
    ) -> Self {
        let ids = Arc::new(IdGen::new(clock.clone(), entropy.clone()));
        Self {
            store,
            clock,
            entropy,
            ids,
            issuer,
            config,
            window: watch::channel(None).0,
        }
    }

    /// `api` 据此开关配对监听器：`Some` = 开，`None` = 关。
    pub fn window(&self) -> watch::Receiver<Option<PairingWindow>> {
        self.window.subscribe()
    }

    /// 创建会话并作废此前所有 `open` 会话（`gantry pair`）。
    pub async fn open_session(&self, created_by_uid: u32) -> Result<PairingTicket, PkiError> {
        let now = self.clock.now();
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let mut table = load_sessions(&mut tx)?;
        let opened = table.open(
            now,
            self.config.token_ttl,
            created_by_uid,
            self.entropy.as_ref(),
        );
        save_sessions(&mut tx, &table)?;
        tx.commit().await.map_err(store_error)?;
        self.publish_window(&table, now);
        Ok(PairingTicket {
            session_id: opened.id,
            token: opened.token,
            ca_fingerprint: self.issuer.ca_fingerprint(),
            expires_at: opened.expires_at,
        })
    }

    /// 主机取消会话；已结束的会话取消视为成功。
    pub async fn cancel_session(&self, id: PairingSessionId) -> Result<(), PkiError> {
        self.update_sessions(|table| match table.cancel(id) {
            Ok(()) => Ok(()),
            Err(SessionError::NotFound) => Err(PkiError::NotFound),
            Err(SessionError::NotOpen) => Ok(()),
        })
        .await
    }

    /// 把已过期的 `open` 会话置为 `expired` 并刷新窗口（后台任务定期调用）。
    pub async fn expire_sessions(&self) -> Result<(), PkiError> {
        let now = self.clock.now();
        self.update_sessions(|table| {
            table.expire(now);
            Ok(())
        })
        .await
    }

    /// 主机侧读取会话结果（`gantry pair` 等待 `redeemed` 后打印设备名、指纹与确认码）。
    pub async fn session(&self, id: PairingSessionId) -> Result<Option<PairingSession>, PkiError> {
        let snapshot = self.store.snapshot().await.map_err(store_error)?;
        let table: Option<SessionTable> = snapshot
            .module_conn(MODULE)
            .get(PAIRING_SESSIONS, SESSIONS_KEY)
            .map_err(store_error)?;
        Ok(table.and_then(|table| table.get(id).cloned()))
    }

    /// `POST /v1/pair`（§7.4.3）。
    pub async fn redeem(&self, req: PairRequest) -> Result<PairResponse, PairError> {
        let now = self.clock.now();
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let mut table = load_sessions(&mut tx)?;

        // 第 2 步：令牌。不匹配的计数要提交，令牌本身不在这里消耗。
        let session_id = match table.check(&req.token, now, self.config.max_failures) {
            Ok(id) => id,
            Err(invalid) => {
                save_sessions(&mut tx, &table)?;
                tx.commit().await.map_err(store_error)?;
                self.publish_window(&table, now);
                tracing::info!(reason = ?invalid.reason, "pairing token rejected");
                return Err(PairError::TokenInvalid(invalid.reason));
            }
        };

        // 第 3–5 步：不可信字段。失败不消耗令牌（事务回滚即可，check 未改动匹配的会话）。
        let validated = match self.validate(&mut tx, &req).await? {
            Ok(validated) => validated,
            Err(errors) => {
                tx.rollback().await.map_err(store_error)?;
                return Err(PairError::Validation(errors));
            }
        };

        // 第 6 步：单一写事务。
        let platform = req.device.platform.clone();
        let role = initial_role(&platform, desktop_ever_paired(&mut tx.read()).await?);
        let device_id = self
            .ids
            .next::<Dev>()
            .map_err(|error| PkiError::Crypto(format!("device id: {error}")))?;
        let serial = self.unused_serial(&mut tx)?;
        let leaf = self.issuer.intermediate().issue_device_leaf(
            &device_id,
            &validated.client_key,
            &serial,
            now,
            &self.config.cert_policy,
        )?;
        let approval_pubkey_fpr = validated
            .approval_key
            .as_ref()
            .map(P256PublicKey::fingerprint);
        let data = DevicePairedData {
            device_id,
            name: req.device.name.clone(),
            platform,
            role,
            cert_serial: serial.to_string(),
            pubkey_fpr: validated.client_key.fingerprint(),
            approval_pubkey: req.approval_pubkey.clone(),
            approval_pubkey_fpr,
        };
        tx.append(NewEvent {
            ty: EventType::DevicePaired,
            subject: Subject::parse(&device_id.to_string())
                .map_err(|error| PkiError::Crypto(format!("device subject: {error}")))?,
            project_id: None,
            actor: Actor::new(ActorKind::System, "pki"),
            causation_id: None,
            correlation_id: None,
            data: serde_json::to_value(&data)
                .map_err(|error| PkiError::Crypto(format!("encode device.paired: {error}")))?,
            dedup_key: None,
        })
        .await
        .map_err(store_error)?;

        let code = confirmation_code(self.entropy.as_ref());
        {
            let mut conn = tx.module_conn(MODULE);
            conn.insert(
                DEVICE_CERTS,
                serial.as_str(),
                &DeviceCertRow {
                    serial: serial.clone(),
                    device_id,
                    not_before: leaf.not_before,
                    not_after: leaf.not_after,
                    status: CertRowStatus::Active,
                },
            )
            .map_err(store_error)?;
            conn.put(
                DEVICE_STATE,
                &device_id.to_string(),
                &DeviceStateRow {
                    device_id,
                    pubkey_spki: encode_spki(validated.client_key.spki_der()),
                    last_seen: None,
                },
            )
            .map_err(store_error)?;
        }
        table
            .redeem(session_id, device_id, code.clone())
            .map_err(|error| PkiError::Crypto(format!("redeem session: {error}")))?;
        save_sessions(&mut tx, &table)?;
        let device = tx
            .read()
            .device(&device_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| PkiError::Crypto("paired device not projected".into()))?;
        tx.commit().await.map_err(store_error)?;
        self.publish_window(&table, now);
        tracing::info!(device_id = %device_id, serial = %serial, ?role, "device paired");

        Ok(PairResponse {
            device,
            certificate_chain: self.issuer.chain(&leaf),
            api_base_url: self.config.api_base_url.clone(),
            ca_fingerprint: self.issuer.ca_fingerprint(),
            confirmation_code: code,
        })
    }

    /// §7.4.3 第 3–5 步与 §7.4.4 的重复公钥检查。外层 `Err` 是内部错误，内层是字段错误。
    async fn validate(
        &self,
        tx: &mut WriteTx,
        req: &PairRequest,
    ) -> Result<Result<Validated, Vec<FieldError>>, PkiError> {
        let mut errors = Vec::new();
        let approval_key = match (&req.approval_pubkey, is_desktop(&req.device.platform)) {
            (Some(encoded), _) => match parse_approval_pubkey(encoded) {
                Ok(key) => Some(key),
                Err(_) => {
                    errors.push(field("/approval_pubkey", "invalid"));
                    None
                }
            },
            (None, true) => {
                errors.push(field("/approval_pubkey", "required"));
                None
            }
            (None, false) => None,
        };
        if validate_device_name(&req.device.name, self.config.device_name_max_chars).is_err() {
            errors.push(field("/device/name", "invalid"));
        }
        let client_key = match validate_csr(&req.csr_pem, self.config.max_csr_bytes) {
            Ok(key) => Some(key),
            Err(error) => {
                tracing::info!(%error, "pairing CSR rejected");
                errors.push(field("/csr", "invalid"));
                None
            }
        };
        if let (Some(client), Some(approval)) = (&client_key, &approval_key) {
            if client == approval {
                errors.push(field("/approval_pubkey", "same_as_csr_key"));
            }
        }
        // 与 active 设备重复：mTLS 公钥（避免一个密钥两个 Device）、审批公钥（OQ-K9）。
        let active = active_devices(tx).await?;
        if let Some(client) = &client_key {
            let fpr = client.fingerprint();
            if active.iter().any(|device| device.pubkey_fpr == fpr) {
                errors.push(field("/csr", "duplicate_key"));
            }
        }
        if let Some(approval) = &approval_key {
            let fpr = Some(approval.fingerprint());
            if active
                .iter()
                .any(|device| device.approval_pubkey_fpr == fpr)
            {
                errors.push(field("/approval_pubkey", "duplicate_key"));
            }
        }
        Ok(match client_key {
            Some(client_key) if errors.is_empty() => Ok(Validated {
                client_key,
                approval_key,
            }),
            _ => Err(errors),
        })
    }

    /// 128 位随机序列号；与在册证书冲突则重新生成（§7.1）。
    fn unused_serial(&self, tx: &mut WriteTx) -> Result<CertSerial, PkiError> {
        loop {
            let serial = CertSerial::generate(self.entropy.as_ref());
            let taken: Option<DeviceCertRow> = tx
                .module_conn(MODULE)
                .get(DEVICE_CERTS, serial.as_str())
                .map_err(store_error)?;
            if taken.is_none() {
                return Ok(serial);
            }
        }
    }

    async fn update_sessions(
        &self,
        f: impl FnOnce(&mut SessionTable) -> Result<(), PkiError>,
    ) -> Result<(), PkiError> {
        let now = self.clock.now();
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let mut table = load_sessions(&mut tx)?;
        f(&mut table)?;
        save_sessions(&mut tx, &table)?;
        tx.commit().await.map_err(store_error)?;
        self.publish_window(&table, now);
        Ok(())
    }

    fn publish_window(&self, table: &SessionTable, now: Timestamp) {
        let window = table.window(now);
        self.window.send_if_modified(|current| {
            let changed = *current != window;
            *current = window;
            changed
        });
    }
}

struct Validated {
    client_key: P256PublicKey,
    approval_key: Option<P256PublicKey>,
}

fn field(pointer: &'static str, code: &'static str) -> FieldError {
    FieldError { pointer, code }
}

fn load_sessions(tx: &mut WriteTx) -> Result<SessionTable, PkiError> {
    Ok(tx
        .module_conn(MODULE)
        .get(PAIRING_SESSIONS, SESSIONS_KEY)
        .map_err(store_error)?
        .unwrap_or_default())
}

fn save_sessions(tx: &mut WriteTx, table: &SessionTable) -> Result<(), PkiError> {
    tx.module_conn(MODULE)
        .put(PAIRING_SESSIONS, SESSIONS_KEY, table)
        .map_err(store_error)?;
    Ok(())
}

async fn active_devices(tx: &mut WriteTx) -> Result<Vec<Device>, PkiError> {
    let mut reader = tx.read();
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let page = reader
            .devices(&Page {
                limit: 200,
                cursor,
                ..Page::default()
            })
            .await
            .map_err(store_error)?;
        out.extend(
            page.items
                .into_iter()
                .filter(|device| device.status == DeviceStatus::Active),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}
