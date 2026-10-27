//! 外壳的状态与命令编排（desktop-shell.md §6.1 `identity_*` / `pairing_*` / `api_request`，§7.2）。
//!
//! 与 Tauri 无关：Tauri 命令只把参数转给 [`Shell`]，原生确认框等 UI 动作在 Tauri 层完成后再调用这里。
//! 这样配对、身份与 API 请求的全部逻辑都能在 CI 里用软件密钥与进程内 HTTPS 服务端测试。

use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::approval::{self, Signer};
use crate::events::{self, ConnState, EventsConfig, FrameSink, Phase, StreamHandle};
use crate::http::{HttpClient, Origin};
use crate::ipc::{ShellError, ShellErrorCode};
use crate::keys::{DeviceKeys, KeyError, KeyStore};
use crate::pairing::{self, PairTransport, PendingIdentity};
use crate::routes;
use crate::sse::StreamState;
use crate::storage::{AppStore, IdentityFile};
use crate::tls::{chain_from_pem, client_config, ClientIdentity, RootFingerprint};

/// 当前时间（Unix 毫秒）；测试可注入。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// 已配对并加载好密钥的连接上下文。
pub struct Connected {
    pub identity: IdentityFile,
    pub keys: Arc<dyn DeviceKeys>,
    pub client: Arc<HttpClient>,
}

pub struct Shell {
    store: AppStore,
    keys: Box<dyn KeyStore>,
    transport: Box<dyn PairTransport>,
    clock: Clock,
    pending: Mutex<Option<PendingIdentity>>,
    connected: RwLock<Option<Arc<Connected>>>,
    signer: Arc<Signer>,
    conn: Mutex<ConnState>,
    stream: Mutex<Option<StreamHandle>>,
    events_config: EventsConfig,
    guarded: crate::guarded::GuardedWrites,
}

impl Shell {
    pub fn new(
        store: AppStore,
        keys: Box<dyn KeyStore>,
        transport: Box<dyn PairTransport>,
        clock: Clock,
    ) -> Self {
        Self {
            store,
            keys,
            transport,
            clock,
            pending: Mutex::new(None),
            connected: RwLock::new(None),
            signer: Signer::new(),
            conn: Mutex::new(ConnState::default()),
            stream: Mutex::new(None),
            events_config: EventsConfig::default(),
            guarded: Default::default(),
        }
    }

    pub(crate) fn guarded(&self) -> &crate::guarded::GuardedWrites {
        &self.guarded
    }

    pub(crate) fn phase(&self) -> Phase {
        self.conn.lock().unwrap().phase
    }

    pub(crate) fn server_now_ms_pub(&self) -> i64 {
        self.server_now_ms()
    }

    pub(crate) async fn me_pub(&self, c: &Connected) -> Result<Value, ShellError> {
        self.me(c).await
    }

    pub(crate) async fn get_json_pub(
        &self,
        c: &Connected,
        path: &str,
    ) -> Result<(u16, Value), ShellError> {
        self.get_json(c, path).await
    }

    /// 测试用：缩短空闲判定与退避。
    pub fn with_events_config(mut self, config: EventsConfig) -> Self {
        self.events_config = config;
        self
    }

    pub fn store(&self) -> &AppStore {
        &self.store
    }

    pub fn signer(&self) -> &Arc<Signer> {
        &self.signer
    }

    fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    /// 已配对时返回连接上下文（必要时从 `identity.json` 与密钥库加载）。
    /// 未配对 → `shell.not_paired`；身份在但密钥丢失 → `shell.key_unavailable`。
    pub fn connected(&self) -> Result<Arc<Connected>, ShellError> {
        if let Some(c) = self.connected.read().unwrap().as_ref() {
            return Ok(c.clone());
        }
        let identity = self
            .store
            .identity()
            .filter(|i| !i.revoked)
            .ok_or_else(ShellError::not_paired)?;
        let keys: Arc<dyn DeviceKeys> = Arc::from(self.keys.load(&identity.key_labels())?);
        let origin = Origin::parse(&identity.api_base_url).ok_or_else(ShellError::not_paired)?;
        let pinned =
            RootFingerprint::parse(&identity.ca_fingerprint).ok_or_else(ShellError::not_paired)?;
        let chain = chain_from_pem(&identity.cert_chain_pem).ok_or_else(ShellError::not_paired)?;
        let client = HttpClient::new(
            origin,
            client_config(
                pinned,
                Some(ClientIdentity {
                    chain,
                    keys: keys.clone(),
                }),
            ),
        );
        let connected = Arc::new(Connected {
            identity,
            keys,
            client: Arc::new(client),
        });
        *self.connected.write().unwrap() = Some(connected.clone());
        Ok(connected)
    }

    // ---- 身份 ----

    /// `identity_status`（§6.1）。
    pub fn identity_status(&self) -> Result<Value, ShellError> {
        if let Some(p) = self.pending.lock().unwrap().as_ref() {
            if !p.is_expired(self.now_ms() / 1000) {
                let d = p.device();
                return Ok(json!({
                    "state": "pending_confirm",
                    "server_host": Value::Null,
                    "ca_fingerprint": Value::Null,
                    "device": {
                        "id": d.id, "name": d.name, "platform": d.platform,
                        "pubkey_fpr": d.pubkey_fpr, "approval_pubkey_fpr": d.approval_pubkey_fpr,
                    },
                    "cert": Value::Null,
                    "key_backend": self.keys.backend().as_str(),
                }));
            }
        }
        let Some(identity) = self.store.identity() else {
            return Ok(unpaired());
        };
        if identity.revoked {
            let mut status = unpaired();
            status["state"] = json!("revoked");
            status["device"] = json!({ "id": identity.device_id, "name": identity.device_name, "platform": identity.platform });
            return Ok(status);
        }
        let connected = self.connected()?;
        let cert = leaf_validity(&identity.cert_chain_pem);
        let state = match &cert {
            Some((_, _, not_after_ms)) if *not_after_ms <= self.now_ms() => "cert_expired",
            _ => "paired",
        };
        let host = Origin::parse(&identity.api_base_url).map(|o| o.host);
        Ok(json!({
            "state": state,
            "server_host": host,
            "ca_fingerprint": identity.ca_fingerprint,
            "device": {
                "id": identity.device_id,
                "name": identity.device_name,
                "platform": identity.platform,
                "pubkey_fpr": connected.keys.tls_public_key().fingerprint(),
                "approval_pubkey_fpr": connected.keys.approval_public_key().fingerprint(),
            },
            "cert": cert.map(|(serial, not_before, not_after)| json!({
                "serial": serial,
                "not_after": rfc3339(not_after),
                // 以 `/v1/me` 的 `certificate.renew_after` 为准（续期切片）；这里按剩余 1/3 有效期本地推算。
                "renew_after": rfc3339(not_before + (not_after - not_before) * 2 / 3),
            })),
            "key_backend": connected.keys.backend().as_str(),
        }))
    }

    /// `pairing_begin`（§7.2 第 1–6 步）。仅在未配对时可用。
    pub async fn pairing_begin(
        &self,
        pairing_string: &str,
        device_name: &str,
    ) -> Result<Value, ShellError> {
        if self.store.identity().is_some() {
            return Err(ShellError::bad_argument("state"));
        }
        // 丢弃之前未确认的待定身份。
        if let Some(old) = self.pending.lock().unwrap().take() {
            let _ = old.discard(self.keys.as_ref());
        }
        let pending = pairing::begin(
            pairing_string,
            device_name,
            self.keys.as_ref(),
            self.transport.as_ref(),
            self.now_ms() / 1000,
        )
        .await?;
        let result = json!({
            "confirmation_code": pending.confirmation_code(),
            "device": pending.device(),
        });
        *self.pending.lock().unwrap() = Some(pending);
        Ok(result)
    }

    /// `pairing_confirm`（§7.2 第 7 步）。`false` 或超时 → 删除密钥与待定证书。
    pub async fn pairing_confirm(&self, matches: bool) -> Result<Value, ShellError> {
        let pending = self
            .pending
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| ShellError::bad_argument("state"))?;
        if !matches || pending.is_expired(self.now_ms() / 1000) {
            let expired = matches;
            pending.discard(self.keys.as_ref())?;
            if expired {
                return Err(ShellError {
                    reason: Some("confirm_timeout".into()),
                    ..ShellError::new(ShellErrorCode::PairingFailed, "确认超时，请重新配对")
                });
            }
            return Ok(unpaired());
        }
        let identity = pending.into_identity(rfc3339(self.now_ms()));
        self.store.save_identity(&identity)?;
        // 用新证书发 `GET /v1/me` 确认可用并记下 `log_epoch`；暂时连不上不影响已配对。
        if let Ok(connected) = self.connected() {
            if let Ok(resp) = connected.client.send("GET", "/v1/me", None, &[]).await {
                let epoch = resp.log_epoch().or_else(|| {
                    serde_json::from_slice::<Value>(&resp.body).ok()?["server"]["log_epoch"]
                        .as_u64()
                });
                if let Some(epoch) = epoch {
                    self.observe_log_epoch(epoch)?;
                }
            }
        }
        self.identity_status()
    }

    /// `identity_reset`：调用前 Tauri 层已弹原生确认框。删除本机密钥、证书、`identity.json`，
    /// 清除 `ui-state/`（其中的项目 ID 属于旧服务端）；`prefs.json` 保留。
    pub fn identity_reset(&self) -> Result<Value, ShellError> {
        if let Some(p) = self.pending.lock().unwrap().take() {
            let _ = p.discard(self.keys.as_ref());
        }
        if let Some(identity) = self.store.identity() {
            match self.keys.delete(&identity.key_labels()) {
                Ok(()) | Err(KeyError::KeyUnavailable) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.store.delete_identity()?;
        self.store.ui_state_clear()?;
        self.signer.clear();
        self.guarded.clear();
        *self.connected.write().unwrap() = None;
        self.stream.lock().unwrap().take();
        *self.conn.lock().unwrap() = ConnState::default();
        Ok(unpaired())
    }

    /// 比对服务端纪元号（P-67）。变化时丢弃全部 `PreparedSigning` 并保存新值；返回是否变化。
    /// 断流、清游标与 `resync_required` 由事件流负责。
    pub fn observe_log_epoch(&self, epoch: u64) -> Result<bool, ShellError> {
        let Some(mut identity) = self.store.identity() else {
            return Ok(false);
        };
        if identity.log_epoch == Some(epoch) {
            return Ok(false);
        }
        let changed = identity.log_epoch.is_some();
        identity.log_epoch = Some(epoch);
        self.store.save_identity(&identity)?;
        {
            let mut guard = self.connected.write().unwrap();
            if let Some(c) = guard.as_ref() {
                let next = Connected {
                    identity: identity.clone(),
                    keys: c.keys.clone(),
                    client: c.client.clone(),
                };
                *guard = Some(Arc::new(next));
            }
        }
        if changed {
            self.signer.clear();
            self.guarded.clear();
        }
        Ok(changed)
    }

    // ---- 事件流（§7.3）----

    /// `events_open`：打开唯一事件流，已打开则替换旧流。`after_seq` 是 UI 快照的最小 `X-Event-Seq`。
    pub fn events_open(
        self: &Arc<Self>,
        after_seq: Option<u64>,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), ShellError> {
        let connected = self.connected()?;
        let state = Arc::new(Mutex::new(StreamState::new(
            after_seq,
            connected.identity.log_epoch,
        )));
        // 先停旧流，再起新流：同时只存在一条。
        self.stream.lock().unwrap().take();
        let handle = events::spawn(self.clone(), state, sink, self.events_config.clone());
        *self.stream.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// `events_ack`：UI 已应用到 `seq`。乱序或回退被忽略。
    pub fn events_ack(&self, seq: u64) {
        if let Some(h) = self.stream.lock().unwrap().as_ref() {
            h.state.lock().unwrap().ack(seq);
        }
    }

    pub fn events_close(&self) {
        self.stream.lock().unwrap().take();
    }

    /// 当前事件流的重连游标（测试与诊断用）。
    pub fn events_cursor(&self) -> Option<u64> {
        self.stream
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|h| h.state.lock().unwrap().cursor())
    }

    /// `connection_state`（§6.2）。
    pub fn connection_state(&self) -> Value {
        let mut conn = self.conn.lock().unwrap().clone();
        if let Some(identity) = self.store.identity() {
            conn.log_epoch = conn.log_epoch.or(identity.log_epoch);
            conn.cert_not_after =
                leaf_validity(&identity.cert_chain_pem).map(|(_, _, na)| rfc3339(na));
        }
        // `live` 当且仅当事件流已建立且最近空闲窗口内收到过字节。
        if conn.phase == Phase::Live
            && conn
                .last_bytes_at
                .is_some_and(|t| t.elapsed() > self.events_config.idle)
        {
            conn.phase = Phase::Reconnecting;
        }
        conn.to_json()
    }

    pub(crate) fn set_phase(&self, phase: Phase, sink: &dyn FrameSink) {
        let changed = {
            let mut conn = self.conn.lock().unwrap();
            let changed = conn.phase != phase;
            conn.phase = phase;
            changed
        };
        if changed {
            sink.send(json!({ "t": "state", "state": self.connection_state() }));
        }
    }

    pub(crate) fn on_stream_open(&self, sink: &dyn FrameSink) {
        {
            let mut conn = self.conn.lock().unwrap();
            conn.epoch += 1;
            conn.phase = Phase::Live;
            conn.last_bytes_at = Some(std::time::Instant::now());
        }
        sink.send(json!({ "t": "state", "state": self.connection_state() }));
    }

    pub(crate) fn touch_bytes(&self) {
        self.conn.lock().unwrap().last_bytes_at = Some(std::time::Instant::now());
    }

    /// 吊销即清除（S-10）：停止请求、丢弃待签内容、删除两把密钥与证书，只留"已吊销"标记。
    pub(crate) fn mark_revoked(&self, sink: &dyn FrameSink) {
        if let Some(mut identity) = self.store.identity() {
            let _ = self.keys.delete(&identity.key_labels());
            identity.cert_chain_pem.clear();
            identity.revoked = true;
            let _ = self.store.save_identity(&identity);
        }
        self.signer.clear();
        self.guarded.clear();
        *self.connected.write().unwrap() = None;
        self.set_phase(Phase::Revoked, sink);
    }

    // ---- 审批签名（§7.4.1）----

    /// `GET /v1/me`：顺带校正时钟偏差（本地 − 服务端）并比对纪元号。
    async fn me(&self, c: &Connected) -> Result<Value, ShellError> {
        let resp = c
            .client
            .send("GET", "/v1/me", None, &[])
            .await
            .map_err(|_| not_connected())?;
        if let Some(epoch) = resp.log_epoch() {
            self.observe_log_epoch(epoch)?;
        }
        if resp.status != 200 {
            return Err(not_connected());
        }
        let me: Value = serde_json::from_slice(&resp.body).map_err(|_| not_connected())?;
        if let Some(server_ms) = me["server"]["time"]
            .as_str()
            .and_then(|t| t.parse::<gantry_core::time::Timestamp>().ok())
            .map(|t| t.as_millis())
        {
            self.conn.lock().unwrap().clock_skew_ms = Some(self.now_ms() - server_ms);
        }
        Ok(me)
    }

    /// 用服务端时间校正后的"现在"（审批倒计时与过期比较都用它）。
    fn server_now_ms(&self) -> i64 {
        self.now_ms() - self.conn.lock().unwrap().clock_skew_ms.unwrap_or(0)
    }

    async fn get_json(&self, c: &Connected, path: &str) -> Result<(u16, Value), ShellError> {
        let resp = c
            .client
            .send("GET", path, None, &[])
            .await
            .map_err(|_| not_connected())?;
        if let Some(epoch) = resp.log_epoch() {
            self.observe_log_epoch(epoch)?;
        }
        Ok((
            resp.status,
            serde_json::from_slice(&resp.body).unwrap_or(Value::Null),
        ))
    }

    /// `approval_prepare`（§7.4.1 第 1–5 步）。
    pub async fn approval_prepare(&self, approval_id: &str) -> Result<Value, ShellError> {
        approval_id
            .parse::<gantry_core::ids::ApprovalId>()
            .map_err(|_| ShellError::bad_argument("approval_id"))?;
        let c = self.connected()?;
        let (status, approval) = self
            .get_json(&c, &format!("/v1/approvals/{approval_id}"))
            .await?;
        if status != 200 {
            return Err(ShellError::bad_argument("approval_id"));
        }
        let me = self.me(&c).await?;
        let device_name = me["device"]["name"]
            .as_str()
            .unwrap_or(&c.identity.device_name)
            .to_string();
        let offset = -self.conn.lock().unwrap().clock_skew_ms.unwrap_or(0);
        let unsigned = self.unsigned_context(&c, &approval).await;

        if approval["status"] != "pending" {
            return Ok(approval::readonly_model(
                &approval,
                &c.identity.device_id,
                &device_name,
                "not_pending",
                offset,
                unsigned,
            ));
        }
        // 非 approver 或非桌面档案：只读展示，不取 challenge（§7.4.1 第 2 步）。
        if me["device"]["role"] != "approver"
            || me["profile"].as_str().is_some_and(|p| p != "desktop")
        {
            return Ok(approval::readonly_model(
                &approval,
                &c.identity.device_id,
                &device_name,
                "not_approver",
                offset,
                unsigned,
            ));
        }
        let (status, challenge) = self
            .get_json(&c, &format!("/v1/approvals/{approval_id}/challenge"))
            .await?;
        if status != 200 {
            return Err(ShellError::new(
                ShellErrorCode::NotConnected,
                "取不到签名 challenge",
            ));
        }
        let internal_digest = approval::cross_device_internal_digest(&approval);
        let checked = approval::self_check(
            approval_id,
            &challenge,
            &approval,
            &c.identity.device_id,
            self.server_now_ms(),
            internal_digest.as_deref(),
        )?;
        let prepared_id = self.signer.insert(checked.clone(), self.server_now_ms())?;
        Ok(approval::sheet_model(
            &prepared_id,
            &checked,
            &device_name,
            json!({ "ok": true }),
            offset,
            unsigned,
        ))
    }

    /// 未签名区：只放展示用的上下文，标注不可信（§6.3、api.md §6.5 规则 2）。
    async fn unsigned_context(&self, c: &Connected, approval: &Value) -> Value {
        let mut title = Value::Null;
        if let Some(dlv) = approval["delivery_id"].as_str() {
            if dlv.parse::<gantry_core::ids::DeliveryId>().is_ok() {
                if let Ok((200, d)) = self.get_json(c, &format!("/v1/deliveries/{dlv}")).await {
                    if let Some(t) = d["title"].as_str() {
                        title =
                            json!({ "text": t, "source": "delivery.title", "truncated": false });
                    }
                }
            }
        }
        json!({
            "delivery_title": title,
            "pr": Value::Null,
            "changes": Value::Null,
            "note": "服务端未提供改动明细（OQ-S9），请在 GitHub 核对该提交。",
        })
    }

    /// `approval_sign`（§7.4.1）：重检 → 用户在场 → 签名 → `grant`。不接受任何待签内容。
    pub async fn approval_sign(&self, prepared_id: &str) -> Result<Value, ShellError> {
        let now = self.server_now_ms();
        let prepared = self
            .signer
            .get(prepared_id, now)
            .ok_or_else(|| ShellError::new(ShellErrorCode::PreparedExpired, "签名句柄已过期"))?;
        // 连接中断时不弹确认框。
        if self.conn.lock().unwrap().phase != Phase::Live {
            return Err(not_connected());
        }
        let c = self.connected()?;
        let approval_id = prepared.approval_id();
        let (status, approval) = self
            .get_json(&c, &format!("/v1/approvals/{approval_id}"))
            .await?;
        let raw = &prepared.checked.raw;
        let unchanged = status == 200
            && approval["status"] == "pending"
            && ["action", "target", "subject_digest"]
                .iter()
                .all(|k| approval[*k] == raw[*k]);
        if !unchanged {
            self.signer.discard(prepared_id);
            return Err(ShellError {
                reason: Some(
                    if approval["status"] != "pending" {
                        "not_pending"
                    } else {
                        "approval_data_mismatch"
                    }
                    .into(),
                ),
                ..ShellError::new(ShellErrorCode::SelfCheckFailed, "审批已变化，请重新载入")
            });
        }
        // 用户在场会阻塞（Touch ID / Windows Hello），放到阻塞线程。
        let (signer, keys, id) = (self.signer.clone(), c.keys.clone(), prepared_id.to_string());
        let grant =
            tokio::task::spawn_blocking(move || signer.sign(&id, keys.as_ref(), now, |_| Ok(())))
                .await
                .map_err(|_| ShellError::new(ShellErrorCode::PresenceFailed, "签名中断"))??;
        let body = serde_json::to_vec(&grant.body).expect("grant 体可序列化");
        let request = routes::PreparedRequest {
            operation_id: "grantApproval",
            method: "POST",
            path_and_query: format!("/v1/approvals/{}/grant", grant.approval_id),
            body: Some(body),
            idempotency_key: Some(grant.idempotency_key),
        };
        match c.client.send_prepared(&request).await {
            Ok(resp) => {
                if let Some(epoch) = resp.log_epoch() {
                    self.observe_log_epoch(epoch)?;
                }
                // 界面只按响应里的 `Approval.status` 更新，不做乐观显示（api.md §6.5 规则 5）。
                Ok(routes::shape_response(
                    resp.status,
                    &resp.headers,
                    &resp.body,
                ))
            }
            Err(e) => Ok(routes::network_failure(e.as_str())),
        }
    }

    pub fn approval_discard(&self, prepared_id: &str) {
        self.signer.discard(prepared_id);
    }

    // ---- API ----

    /// `api_request`（§7.1）。本地校验不通过的请求不会发出。
    pub async fn api_request(
        &self,
        route: &str,
        path: &str,
        query: Option<&Value>,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ShellError> {
        let connected = self.connected()?;
        let prepared = routes::prepare(route, path, query, body, idempotency_key)?;
        match connected.client.send_prepared(&prepared).await {
            Ok(resp) => {
                if let Some(epoch) = resp.log_epoch() {
                    self.observe_log_epoch(epoch)?;
                }
                Ok(routes::shape_response(
                    resp.status,
                    &resp.headers,
                    &resp.body,
                ))
            }
            Err(e) => Ok(routes::network_failure(e.as_str())),
        }
    }
}

fn not_connected() -> ShellError {
    ShellError::new(ShellErrorCode::NotConnected, "连接中断")
}

fn unpaired() -> Value {
    json!({
        "state": "unpaired",
        "server_host": Value::Null,
        "ca_fingerprint": Value::Null,
        "device": Value::Null,
        "cert": Value::Null,
        "key_backend": Value::Null,
    })
}

fn rfc3339(ms: i64) -> String {
    gantry_core::time::Timestamp::from_millis(ms).to_string()
}

/// 叶子证书的（序列号十六进制，notBefore 毫秒，notAfter 毫秒）。
fn leaf_validity(chain: &[String]) -> Option<(String, i64, i64)> {
    let der = chain_from_pem(chain)?.into_iter().next()?;
    let (_, cert) = X509Certificate::from_der(der.as_ref()).ok()?;
    let serial: String = cert
        .raw_serial()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some((
        serial,
        cert.validity().not_before.timestamp() * 1000,
        cert.validity().not_after.timestamp() * 1000,
    ))
}
