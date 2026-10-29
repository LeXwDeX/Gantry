//! 配对客户端流程（desktop-shell.md §1 第 2 条、§7.2、api.md §1.3、pki.md §7.4.2）。
//!
//! 本模块只管"要发什么、收到什么才算数"，网络交给 [`PairTransport`]（单向 TLS + 固定离线根 CA
//! 指纹，由 mTLS 切片实现；测试用替身）。步骤：
//!
//! 1. 解析配对串（[`PairingString`]），token 视为秘密；
//! 2. 在硬件里生成两把密钥（P-47）；
//! 3. 用 mTLS 客户端密钥签出 CSR，审批签名公钥编码为 `approval_pubkey`；
//! 4. `POST /v1/pair`；
//! 5. 校验响应（[`verify_response`]）：链形状、根指纹、链内签名与有效期、叶子公钥、两个公钥指纹；
//! 6. 暂存为 [`PendingIdentity`]，等用户核对 6 位确认码；
//! 7. `confirm(true)` 写 `identity.json`；`false` 或超时删除两把密钥。
//!
//! 第 2 步之后任何一步失败都删除已生成的密钥。

use std::sync::Arc;

use gantry_core::canon::sha256;
use serde::{Deserialize, Serialize};
use x509_parser::prelude::{FromDer, X509Certificate};
use zeroize::Zeroizing;

use crate::ipc::{ShellError, ShellErrorCode};
use crate::keys::{DeviceKeys, KeyStore, P256PublicKey};
use crate::storage::IdentityFile;

const SCHEME: &str = "gantry+pair://";
/// 待确认身份的有效期 [建议值]：与服务端配对窗口一样长（pki.md OQ-K3：10 分钟）。
pub const PENDING_TTL_S: i64 = 600;

// ---- 配对串 ----

/// `gantry+pair://<host>:<port>/?v=1&fp=sha256:<64 位十六进制>&t=<token>`（pki.md §7.4.2，OQ-S3）。
pub struct PairingString {
    pub host: String,
    pub port: u16,
    /// 离线根 CA 证书 DER 的 SHA-256（P-64）。
    pub ca_fingerprint: String,
    token: Zeroizing<String>,
}

impl std::fmt::Debug for PairingString {
    // token 是秘密，不进日志（§4）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingString")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("ca_fingerprint", &self.ca_fingerprint)
            .finish_non_exhaustive()
    }
}

impl PairingString {
    pub fn parse(raw: &str) -> Result<Self, ShellError> {
        let bad = || ShellError::bad_argument("pairing_string");
        let raw = raw.trim();
        if raw.len() > 1024 || raw.chars().any(char::is_control) {
            return Err(bad());
        }
        let rest = raw.strip_prefix(SCHEME).ok_or_else(bad)?;
        let (authority, query) = rest.split_once("/?").ok_or_else(bad)?;
        let (host, port) = authority.rsplit_once(':').ok_or_else(bad)?;
        let port: u16 = port.parse().map_err(|_| bad())?;
        if port == 0 || !is_host(host) {
            return Err(bad());
        }
        let (mut v, mut fp, mut token) = (None, None, None);
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or_else(bad)?;
            let slot = match key {
                "v" => &mut v,
                "fp" => &mut fp,
                "t" => &mut token,
                _ => return Err(bad()),
            };
            if slot.replace(value.to_string()).is_some() {
                return Err(bad());
            }
        }
        if v.as_deref() != Some("1") {
            return Err(bad());
        }
        let fp = fp.filter(|f| is_fingerprint(f)).ok_or_else(bad)?;
        let token = token
            .filter(|t| !t.is_empty() && t.len() <= 256 && t.bytes().all(is_token_byte))
            .ok_or_else(bad)?;
        Ok(Self {
            host: host.to_string(),
            port,
            ca_fingerprint: fp,
            token: Zeroizing::new(token),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        && !host.starts_with(['.', '-'])
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

fn is_fingerprint(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// ---- 线上形状（api.md §1.3）----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Windows,
}

impl Platform {
    /// 由外壳填，不接受 UI 传入（§6.1 `pairing_begin`）。
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Macos
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PairDevice {
    pub name: String,
    pub platform: Platform,
}

/// `POST /v1/pair` 的请求体。`Debug` 不打印 token。
#[derive(Clone)]
pub struct PairRequestBody {
    pub token: Zeroizing<String>,
    pub csr: String,
    pub approval_pubkey: String,
    pub device: PairDevice,
}

// `Zeroizing` 不实现 `Serialize`；线上形状仍是普通字符串 `token` 字段。
impl Serialize for PairRequestBody {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("PairRequestBody", 4)?;
        s.serialize_field("token", self.token.as_str())?;
        s.serialize_field("csr", &self.csr)?;
        s.serialize_field("approval_pubkey", &self.approval_pubkey)?;
        s.serialize_field("device", &self.device)?;
        s.end()
    }
}

impl std::fmt::Debug for PairRequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairRequestBody")
            .field("approval_pubkey", &self.approval_pubkey)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// 响应里的 `device`。客户端忽略未知字段（api.md §1.4）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub pubkey_fpr: String,
    pub approval_pubkey_fpr: Option<String>,
    pub role: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PairResponseBody {
    pub device: PairedDevice,
    pub certificate_chain: Vec<String>,
    pub api_base_url: String,
    pub ca_fingerprint: String,
    pub confirmation_code: String,
}

/// 配对端点地址与要固定的根指纹。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairEndpoint {
    pub host: String,
    pub port: u16,
    pub ca_fingerprint: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairReply {
    Created(Box<PairResponseBody>),
    /// `401 pair_token_invalid`：无效 / 已用 / 过期 / 作废不区分（api.md §1.3）。
    TokenInvalid,
    /// 其他 HTTP 错误；只保留 `problem.code`。
    Problem {
        status: u16,
        code: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransportError {
    /// 服务端证书没有链到固定的根（疑似中间人）。
    Untrusted,
    /// 连接失败、超时、响应丢失（token 可能已被消耗）。
    Network,
}

/// 单向 TLS 的配对传输：只信链到 `endpoint.ca_fingerprint` 的服务端证书，不用系统信任库。
/// 实现见 [`crate::http::HttpPairTransport`]。
#[async_trait::async_trait]
pub trait PairTransport: Send + Sync {
    async fn pair(
        &self,
        endpoint: &PairEndpoint,
        body: &PairRequestBody,
    ) -> Result<PairReply, TransportError>;
}

// ---- CSR ----

/// rcgen 的远程密钥：CSR 由硬件里的 mTLS 客户端密钥签名，私钥不出硬件。
struct RemoteTlsKey {
    keys: Arc<dyn DeviceKeys>,
    public: [u8; 65],
}

impl rcgen::RemoteKeyPair for RemoteTlsKey {
    fn public_key(&self) -> &[u8] {
        &self.public
    }

    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.keys
            .sign_tls(msg)
            .map_err(|_| rcgen::Error::RemoteKeyError)
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

/// subject 任意，服务端会重写为 `CN = device.id`（api.md §1.3）。
pub fn build_csr(keys: Arc<dyn DeviceKeys>) -> Result<String, ShellError> {
    let failed = || pairing_failed("csr");
    let public = *keys.tls_public_key().as_bytes();
    let key_pair = rcgen::KeyPair::from_remote(Box::new(RemoteTlsKey { keys, public }))
        .map_err(|_| failed())?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|_| failed())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "gantry-desktop");
    let csr = params.serialize_request(&key_pair).map_err(|_| failed())?;
    csr.pem().map_err(|_| failed())
}

// ---- 响应校验 ----

fn pairing_failed(reason: &str) -> ShellError {
    ShellError {
        reason: Some(reason.into()),
        ..ShellError::new(ShellErrorCode::PairingFailed, "配对失败")
    }
}

/// §7.2 第 5 步。`now_s` 为 Unix 秒。任何一项不符都拒绝（reason 见各分支）。
pub fn verify_response(
    resp: &PairResponseBody,
    pinned_fingerprint: &str,
    keys: &dyn DeviceKeys,
    now_s: i64,
) -> Result<(), ShellError> {
    if resp.ca_fingerprint != pinned_fingerprint {
        return Err(pairing_failed("ca_fingerprint_mismatch"));
    }
    let tls = keys.tls_public_key();
    verify_chain(&resp.certificate_chain, pinned_fingerprint, &tls, now_s)?;
    if resp.device.pubkey_fpr != tls.fingerprint() {
        return Err(pairing_failed("pubkey_fpr_mismatch"));
    }
    if resp.device.approval_pubkey_fpr.as_deref() != Some(&keys.approval_public_key().fingerprint())
    {
        return Err(pairing_failed("approval_pubkey_fpr_mismatch"));
    }
    if !resp.api_base_url.starts_with("https://") || !resp.device.id.starts_with("dev_") {
        return Err(pairing_failed("response_shape"));
    }
    if resp.confirmation_code.len() != 6
        || !resp.confirmation_code.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(pairing_failed("response_shape"));
    }
    Ok(())
}

/// 设备证书链 `[叶子, 中间 CA, 根]` 的校验（配对与续期共用，§7.2 第 5 步、"自动续期"）：
/// 根证书 DER 的 SHA-256 等于固定指纹；逐级签名有效、中间与根是 CA；全部在有效期内；
/// 叶子的公钥等于本机 mTLS 客户端公钥。
pub fn verify_chain(
    chain: &[String],
    pinned_fingerprint: &str,
    tls: &P256PublicKey,
    now_s: i64,
) -> Result<(), ShellError> {
    if chain.len() != 3 {
        return Err(pairing_failed("chain_shape"));
    }
    let ders = chain
        .iter()
        .map(|p| pem_to_der(p))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| pairing_failed("chain_shape"))?;
    if format!("sha256:{}", sha256(&ders[2]).to_hex()) != pinned_fingerprint {
        return Err(pairing_failed("ca_fingerprint_mismatch"));
    }
    let certs = ders
        .iter()
        .map(|der| X509Certificate::from_der(der).ok().map(|(_, c)| c))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| pairing_failed("chain_shape"))?;
    let (leaf, intermediate, root) = (&certs[0], &certs[1], &certs[2]);
    let signed_by = |child: &X509Certificate<'_>, issuer: &X509Certificate<'_>| {
        child.issuer() == issuer.subject()
            && child.verify_signature(Some(issuer.public_key())).is_ok()
    };
    if !signed_by(leaf, intermediate)
        || !signed_by(intermediate, root)
        || !signed_by(root, root)
        || !intermediate.is_ca()
        || !root.is_ca()
    {
        return Err(pairing_failed("chain_invalid"));
    }
    let valid_now = |c: &X509Certificate<'_>| {
        c.validity().not_before.timestamp() <= now_s && now_s <= c.validity().not_after.timestamp()
    };
    if !certs.iter().all(valid_now) {
        return Err(pairing_failed("chain_expired"));
    }
    if leaf.public_key().raw != tls.spki_der().as_slice() {
        return Err(pairing_failed("leaf_key_mismatch"));
    }
    Ok(())
}

fn pem_to_der(pem_text: &str) -> Option<Vec<u8>> {
    let parsed = pem::parse(pem_text).ok()?;
    (parsed.tag() == "CERTIFICATE").then(|| parsed.into_contents())
}

// ---- 流程 ----

/// 已通过校验、等待用户核对确认码的身份。尚不可用于 API（§7.2 第 6 步）。
pub struct PendingIdentity {
    keys: Arc<dyn DeviceKeys>,
    response: PairResponseBody,
    pinned_fingerprint: String,
    created_at_s: i64,
}

impl PendingIdentity {
    pub fn confirmation_code(&self) -> &str {
        &self.response.confirmation_code
    }

    pub fn device(&self) -> &PairedDevice {
        &self.response.device
    }

    pub fn keys(&self) -> &Arc<dyn DeviceKeys> {
        &self.keys
    }

    pub fn is_expired(&self, now_s: i64) -> bool {
        now_s - self.created_at_s > PENDING_TTL_S
    }

    /// 用户核对一致：得到要写入 `identity.json` 的内容。`paired_at` 为 RFC 3339。
    pub fn into_identity(self, paired_at: String) -> IdentityFile {
        let labels = self.keys.labels().clone();
        IdentityFile {
            api_base_url: self.response.api_base_url,
            ca_fingerprint: self.pinned_fingerprint,
            device_id: self.response.device.id,
            device_name: self.response.device.name,
            platform: self.response.device.platform,
            cert_chain_pem: self.response.certificate_chain,
            tls_key_label: labels.tls,
            approval_key_label: labels.approval,
            paired_at,
            renewed_at: None,
            log_epoch: None,
            revoked: false,
        }
    }

    /// 用户核对不一致、超时或放弃：删除两把密钥与待定证书（§6.1 `pairing_confirm`，OQ-S4）。
    pub fn discard(self, store: &dyn KeyStore) -> Result<(), ShellError> {
        store.delete(self.keys.labels()).map_err(ShellError::from)
    }
}

/// `pairing_begin` 的第 1–6 步。失败时已生成的密钥一律删除。
pub async fn begin(
    pairing_string: &str,
    device_name: &str,
    store: &dyn KeyStore,
    transport: &dyn PairTransport,
    now_s: i64,
) -> Result<PendingIdentity, ShellError> {
    let parsed = PairingString::parse(pairing_string)?;
    let name = device_name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ShellError::bad_argument("device_name"));
    }
    // 硬件不可用时 fail-closed（OQ-S2），不会降级为软件密钥。
    let keys: Arc<dyn DeviceKeys> = Arc::from(store.generate().map_err(ShellError::from)?);
    let result = exchange(&parsed, name, keys.clone(), transport, now_s).await;
    match result {
        Ok(response) => Ok(PendingIdentity {
            keys,
            response,
            pinned_fingerprint: parsed.ca_fingerprint,
            created_at_s: now_s,
        }),
        Err(e) => {
            let _ = store.delete(keys.labels());
            Err(e)
        }
    }
}

async fn exchange(
    parsed: &PairingString,
    name: &str,
    keys: Arc<dyn DeviceKeys>,
    transport: &dyn PairTransport,
    now_s: i64,
) -> Result<PairResponseBody, ShellError> {
    let body = PairRequestBody {
        token: Zeroizing::new(parsed.token().to_string()),
        csr: build_csr(keys.clone())?,
        approval_pubkey: keys.approval_public_key().to_base64url(),
        device: PairDevice {
            name: name.to_string(),
            platform: Platform::current(),
        },
    };
    let endpoint = PairEndpoint {
        host: parsed.host.clone(),
        port: parsed.port,
        ca_fingerprint: parsed.ca_fingerprint.clone(),
    };
    let response = match transport.pair(&endpoint, &body).await {
        Ok(PairReply::Created(response)) => *response,
        // 三种原因 UI 看到的错误完全相同（§12 配对第 3 条）。响应丢失时 token 已被消耗，同样提示重新生成。
        Ok(PairReply::TokenInvalid) => return Err(token_invalid()),
        Ok(PairReply::Problem { .. }) => return Err(pairing_failed("server_rejected")),
        Err(TransportError::Untrusted) => return Err(pairing_failed("ca_fingerprint_mismatch")),
        Err(TransportError::Network) => return Err(pairing_failed("network")),
    };
    verify_response(&response, &parsed.ca_fingerprint, keys.as_ref(), now_s)?;
    Ok(response)
}

fn token_invalid() -> ShellError {
    // 前端按服务端错误码 `pair_token_invalid` 本地化（与 Mock 适配器一致）。
    ShellError::new(
        ShellErrorCode::PairTokenInvalid,
        "配对串无效、已使用或已过期",
    )
}

/// 审批签名公钥在 `approval_pubkey` 里的形态，供替身服务端解析。
pub fn decode_approval_pubkey(value: &str) -> Option<P256PublicKey> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    P256PublicKey::from_uncompressed(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroize;

    const FP: &str = "sha256:3b7e0c52a91d6f48e2b05c7a9d1f3e64b8a20c5d7e91f3a6b4c8d0e2f1a39b57";

    #[test]
    fn parses_the_suggested_form() {
        let s =
            format!("gantry+pair://gantry.example.com:8444/?v=1&fp={FP}&t=gp1_4Ww9rX0b2sQ7hYk1");
        let p = PairingString::parse(&s).unwrap();
        assert_eq!((p.host.as_str(), p.port), ("gantry.example.com", 8444));
        assert_eq!(p.ca_fingerprint, FP);
        assert_eq!(p.token(), "gp1_4Ww9rX0b2sQ7hYk1");
        assert!(!format!("{p:?}").contains("gp1_"));
        // 粘贴时带的首尾空白可容忍。
        assert!(PairingString::parse(&format!("  {s}\n")).is_ok());
    }

    #[test]
    fn pairing_token_is_stored_in_a_zeroizing_string() {
        // §7.6「内存」：配对 token 以 `SecretString` / `zeroize` 保存。
        let p = PairingString::parse(&format!(
            "gantry+pair://gantry.example.com:8444/?v=1&fp={FP}&t=gp1_secret_token"
        ))
        .unwrap();
        // 编译期断言：字段类型是 `Zeroizing<String>`，不是普通 `String`。
        let token: &Zeroizing<String> = &p.token;
        assert_eq!(token.as_str(), "gp1_secret_token");

        // 显式清零覆盖 token 的缓冲区；`Zeroizing` 在 Drop 时做同一件事。
        let mut token = p.token;
        token.zeroize();
        assert!(token.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn pair_request_serializes_the_token_field_unchanged() {
        // 不改变 IPC / 线上形状：手写 `Serialize` 仍输出普通 `token` 字符串字段。
        let body = PairRequestBody {
            token: Zeroizing::new("gp1_secret_token".into()),
            csr: "csr".into(),
            approval_pubkey: "pk".into(),
            device: PairDevice {
                name: "laptop".into(),
                platform: Platform::Macos,
            },
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["token"], "gp1_secret_token");
        assert_eq!(value.as_object().unwrap().len(), 4);
        assert!(!format!("{body:?}").contains("gp1_secret_token"));
    }

    #[test]
    fn rejects_malformed_pairing_strings() {
        let ok_fp = FP;
        for bad in [
            String::new(),
            format!("https://h:1/?v=1&fp={ok_fp}&t=x"),
            format!("gantry+pair://h/?v=1&fp={ok_fp}&t=x"),
            format!("gantry+pair://h:0/?v=1&fp={ok_fp}&t=x"),
            format!("gantry+pair://h:99999/?v=1&fp={ok_fp}&t=x"),
            format!("gantry+pair://user@h:1/?v=1&fp={ok_fp}&t=x"),
            format!("gantry+pair://h:1/?v=2&fp={ok_fp}&t=x"),
            format!("gantry+pair://h:1/?fp={ok_fp}&t=x"),
            "gantry+pair://h:1/?v=1&fp=sha256:ABC&t=x".to_string(),
            format!("gantry+pair://h:1/?v=1&fp={}&t=x", FP.to_uppercase()),
            format!("gantry+pair://h:1/?v=1&fp={ok_fp}&t="),
            format!("gantry+pair://h:1/?v=1&fp={ok_fp}"),
            format!("gantry+pair://h:1/?v=1&fp={ok_fp}&t=x&t=y"),
            format!("gantry+pair://h:1/?v=1&fp={ok_fp}&t=x&extra=1"),
            format!("gantry+pair://h:1/?v=1&fp={ok_fp}&t=x y"),
            format!("gantry+pair://-h:1/?v=1&fp={ok_fp}&t=x"),
        ] {
            let err = PairingString::parse(&bad).unwrap_err();
            assert_eq!(err.field.as_deref(), Some("pairing_string"), "{bad}");
        }
    }
}
