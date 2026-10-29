//! 配对流程（desktop-shell.md §12"配对"、"密钥与身份"的 A 级部分）。
//!
//! 替身服务端：`TestCA`（gantry-fakes）的中间 CA 经 `gantry-pki` 的 `Authority` 按 CSR 里的公钥真实签发
//! 叶子证书，CSR 与 `approval_pubkey` 也用 `gantry-pki` 的校验函数解析——外壳生成的请求必须能被
//! 服务端实现接受。网络层（单向 TLS、固定根）归 mTLS 切片，这里由 `PairTransport` 替身代替。
#![cfg(feature = "test-software-keys")]

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::KeyStore;
use gantry_desktop_shell::pairing::{
    self, PairEndpoint, PairReply, PairRequestBody, PairResponseBody, PairTransport, PairedDevice,
    TransportError, PENDING_TTL_S,
};
use gantry_fakes::test_ca::TestCA;
use gantry_pki::{Authority, CertPolicy, CertSerial};

fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    TokenInvalid,
    Untrusted,
    Network,
    WrongCaFingerprintField,
    ForeignRoot,
    LeafForOtherKey,
    WrongPubkeyFpr,
    WrongApprovalFpr,
    ShortChain,
    WrongApiHost,
    InsecureApiBaseUrl,
    ApiHostCase,
}

struct FakeServer {
    ca: TestCA,
    inter: Authority,
    token: String,
    fault: Mutex<Fault>,
    requests: Mutex<Vec<PairRequestBody>>,
    other: TestCA,
}

impl FakeServer {
    fn new() -> Self {
        let ca = TestCA::new().unwrap();
        let read = |n: &str| std::fs::read_to_string(ca.root_dir().join(n)).unwrap();
        let inter = Authority::load(&read("inter.crt"), &read("inter.key")).unwrap();
        Self {
            ca,
            inter,
            token: "gp1_4Ww9rX0b2sQ7hYk1".into(),
            fault: Mutex::new(Fault::None),
            requests: Mutex::new(Vec::new()),
            other: TestCA::new().unwrap(),
        }
    }

    fn fault(&self) -> Fault {
        *self.fault.lock().unwrap()
    }

    fn set_fault(&self, f: Fault) {
        *self.fault.lock().unwrap() = f;
    }

    fn fingerprint(&self) -> String {
        self.ca.root_fingerprint().to_string()
    }

    fn pairing_string(&self) -> String {
        format!(
            "gantry+pair://gantry.test:8444/?v=1&fp={}&t={}",
            self.fingerprint(),
            self.token
        )
    }

    fn issue(&self, public: &gantry_pki::P256PublicKey) -> String {
        let id: DeviceId = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".parse().unwrap();
        let serial = CertSerial::generate(&gantry_core::testing::SeqEntropy::new(7));
        let now = Timestamp::from_millis(now_s() * 1000);
        self.inter
            .issue_device_leaf(&id, public, &serial, now, &CertPolicy::default())
            .unwrap()
            .pem
    }
}

#[async_trait::async_trait]
impl PairTransport for FakeServer {
    async fn pair(
        &self,
        endpoint: &PairEndpoint,
        body: &PairRequestBody,
    ) -> Result<PairReply, TransportError> {
        self.requests.lock().unwrap().push(body.clone());
        assert_eq!(endpoint.ca_fingerprint, self.fingerprint());
        assert_eq!(
            (endpoint.host.as_str(), endpoint.port),
            ("gantry.test", 8444)
        );
        match self.fault() {
            Fault::Untrusted => return Err(TransportError::Untrusted),
            Fault::Network => return Err(TransportError::Network),
            Fault::TokenInvalid => return Ok(PairReply::TokenInvalid),
            _ => {}
        }
        if body.token.as_str() != self.token {
            return Ok(PairReply::TokenInvalid);
        }
        // 服务端实现的校验：CSR（pki.md §7.4.4）与 approval_pubkey（§7.4.3 第 3 步）。
        let tls =
            gantry_pki::csr::validate_csr(&body.csr, 16 * 1024).expect("服务端接受外壳的 CSR");
        let approval = gantry_pki::csr::parse_approval_pubkey(&body.approval_pubkey)
            .expect("服务端接受外壳的 approval_pubkey");
        assert_ne!(
            tls.spki_der(),
            approval.spki_der(),
            "两把公钥必须不同（P-47）"
        );

        let mut leaf = self.issue(&tls);
        if self.fault() == Fault::LeafForOtherKey {
            leaf = self.issue(&approval);
        }
        let mut chain = vec![
            leaf,
            self.ca.intermediate_cert_pem().to_string(),
            self.ca.root_cert_pem().to_string(),
        ];
        let mut ca_fingerprint = self.fingerprint();
        match self.fault() {
            Fault::WrongCaFingerprintField => {
                ca_fingerprint = self.other.root_fingerprint().to_string()
            }
            Fault::ForeignRoot => chain[2] = self.other.root_cert_pem().to_string(),
            Fault::ShortChain => {
                chain.pop();
            }
            _ => {}
        }
        let mut device = PairedDevice {
            id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".into(),
            name: body.device.name.clone(),
            platform: "macos".into(),
            pubkey_fpr: tls.fingerprint().to_string(),
            approval_pubkey_fpr: Some(approval.fingerprint().to_string()),
            role: "approver".into(),
        };
        match self.fault() {
            Fault::WrongPubkeyFpr => device.pubkey_fpr = approval.fingerprint().to_string(),
            Fault::WrongApprovalFpr => {
                device.approval_pubkey_fpr = Some(tls.fingerprint().to_string())
            }
            _ => {}
        }
        let api_base_url = match self.fault() {
            Fault::WrongApiHost => "https://evil.test:8443".to_owned(),
            Fault::InsecureApiBaseUrl => "http://gantry.test:8443".to_owned(),
            Fault::ApiHostCase => "https://GANTRY.TEST:9000".to_owned(),
            _ => "https://gantry.test:8443".to_owned(),
        };
        Ok(PairReply::Created(Box::new(PairResponseBody {
            device,
            certificate_chain: chain,
            api_base_url,
            ca_fingerprint,
            confirmation_code: "482913".into(),
        })))
    }
}

fn keys() -> TestSoftwareKeyStore {
    TestSoftwareKeyStore::new(PresenceDouble::new())
}

#[tokio::test]
async fn full_flow_produces_identity_after_confirmation() {
    let server = FakeServer::new();
    let store = keys();
    let pending = pairing::begin(
        &server.pairing_string(),
        "Susan 的 MacBook",
        &store,
        &server,
        now_s(),
    )
    .await
    .unwrap();
    assert_eq!(pending.confirmation_code(), "482913");
    assert_eq!(pending.device().role, "approver");
    assert_eq!(store.key_count(), 2);

    // 请求体：含 csr 与 approval_pubkey，两把公钥不同；平台由外壳填。
    let req = server.requests.lock().unwrap()[0].clone();
    assert!(req.csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----"));
    assert_eq!(
        req.approval_pubkey,
        pending.keys().approval_public_key().to_base64url()
    );
    assert!(
        !format!("{req:?}").contains(&server.token),
        "token 不进 Debug 输出"
    );

    let tls_fpr = pending.keys().tls_public_key().fingerprint();
    let identity = pending.into_identity("2026-10-27T00:00:00.000Z".into());
    assert_eq!(identity.ca_fingerprint, server.fingerprint());
    assert_eq!(identity.cert_chain_pem.len(), 3);
    assert_eq!(identity.device_id, "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM");
    // 重启后可按 identity.json 里的引用名重新拿到同一对密钥。
    let reloaded = store.load(&identity.key_labels()).unwrap();
    assert_eq!(reloaded.tls_public_key().fingerprint(), tls_fpr);
}

#[tokio::test]
async fn confirm_false_or_timeout_deletes_keys() {
    let server = FakeServer::new();
    let store = keys();
    let t0 = now_s();
    let pending = pairing::begin(&server.pairing_string(), "mac", &store, &server, t0)
        .await
        .unwrap();
    assert!(!pending.is_expired(t0 + PENDING_TTL_S));
    assert!(pending.is_expired(t0 + PENDING_TTL_S + 1));
    pending.discard(&store).unwrap();
    assert_eq!(store.key_count(), 0);
}

#[tokio::test]
async fn every_rejection_deletes_generated_keys() {
    let cases = [
        (Fault::Untrusted, "ca_fingerprint_mismatch"),
        (Fault::Network, "network"),
        (Fault::WrongCaFingerprintField, "ca_fingerprint_mismatch"),
        (Fault::ForeignRoot, "ca_fingerprint_mismatch"),
        (Fault::LeafForOtherKey, "leaf_key_mismatch"),
        (Fault::WrongPubkeyFpr, "pubkey_fpr_mismatch"),
        (Fault::WrongApprovalFpr, "approval_pubkey_fpr_mismatch"),
        (Fault::ShortChain, "chain_shape"),
    ];
    let server = FakeServer::new();
    for (fault, reason) in cases {
        server.set_fault(fault);
        let store = keys();
        let err = pairing::begin(&server.pairing_string(), "mac", &store, &server, now_s())
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, ShellErrorCode::PairingFailed, "{fault:?}");
        assert_eq!(err.reason.as_deref(), Some(reason), "{fault:?}");
        assert_eq!(store.key_count(), 0, "{fault:?}：失败后密钥必须删除");
    }
}

#[tokio::test]
async fn api_base_url_host_must_match_and_port_is_not_compared() {
    // 端口 8443 vs 8444 不同、主机大小写不同都通过；主机不同或非 https 失败。
    for (fault, expect_ok) in [
        (Fault::None, true),
        (Fault::ApiHostCase, true),
        (Fault::WrongApiHost, false),
        (Fault::InsecureApiBaseUrl, false),
    ] {
        let server = FakeServer::new();
        server.set_fault(fault);
        let store = keys();
        let result = pairing::begin(
            &server.pairing_string(),
            "Susan 的 MacBook",
            &store,
            &server,
            now_s(),
        )
        .await;
        match (result, expect_ok) {
            (Ok(_), true) => {}
            (Err(error), false) => {
                assert_eq!(error.reason.as_deref(), Some("api_base_url_mismatch"))
            }
            (other, _) => panic!("{fault:?}: {:?}", other.map(|_| ())),
        }
    }
}

#[tokio::test]
async fn token_invalid_is_one_uniform_error() {
    let server = FakeServer::new();
    let mut seen = Vec::new();
    // 服务端统一返回 401 pair_token_invalid；外壳也只给一种错误，不区分无效 / 已用 / 过期。
    for pairing_string in [
        server.pairing_string(),
        server.pairing_string().replace(&server.token, "gp1_wrong"),
    ] {
        server.set_fault(if seen.is_empty() {
            Fault::TokenInvalid
        } else {
            Fault::None
        });
        let store = keys();
        let err = pairing::begin(&pairing_string, "mac", &store, &server, now_s())
            .await
            .err()
            .unwrap();
        assert_eq!(store.key_count(), 0);
        seen.push(serde_json::to_string(&err).unwrap());
    }
    assert_eq!(seen[0], seen[1]);
    assert!(seen[0].contains("\"pair_token_invalid\""));
}

#[tokio::test]
async fn hardware_unavailable_fails_closed_before_any_request() {
    let server = FakeServer::new();
    let store = keys();
    store.set_hardware_available(false);
    let err = pairing::begin(&server.pairing_string(), "mac", &store, &server, now_s())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ShellErrorCode::KeyUnavailable);
    assert!(
        server.requests.lock().unwrap().is_empty(),
        "硬件不可用时不得发出配对请求"
    );
}

#[tokio::test]
async fn bad_inputs_never_generate_keys() {
    let server = FakeServer::new();
    let store = keys();
    for (s, name, field) in [
        ("not a pairing string".to_string(), "mac", "pairing_string"),
        (server.pairing_string(), "", "device_name"),
        (server.pairing_string(), "a\u{7}b", "device_name"),
        (server.pairing_string(), &"x".repeat(65)[..], "device_name"),
    ] {
        let err = pairing::begin(&s, name, &store, &server, now_s())
            .await
            .err()
            .unwrap();
        assert_eq!(err.field.as_deref(), Some(field));
    }
    assert_eq!(store.key_count(), 0);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn expired_chain_is_rejected() {
    let server = FakeServer::new();
    let store = keys();
    // 把"现在"放到证书有效期之后。
    let err = pairing::begin(
        &server.pairing_string(),
        "mac",
        &store,
        &server,
        now_s() + 400 * 86_400,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.reason.as_deref(), Some("chain_expired"));
    assert_eq!(store.key_count(), 0);
}
