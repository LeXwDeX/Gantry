//! 外壳编排：配对 → 确认 → 身份状态 → api_request → 重置（desktop-shell.md §12"配对""密钥与身份"
//! "IPC 白名单与参数"的 A 级部分）。服务端是进程内 HTTPS 替身：配对监听器（单向 TLS）与 API（mTLS）。
#![cfg(feature = "test-software-keys")]

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gantry_desktop_shell::http::HttpPairTransport;
use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::session::{system_clock, Clock, Shell};
use gantry_desktop_shell::storage::AppStore;
use serde_json::{json, Value};
use support::{serve_simple, Handler, Pki, Seen};

/// 配对 token 兼作脱敏哨兵：日志与诊断快照里都不得出现。
const TOKEN: &str = "gp1_SENTINELtoken0123";
const PRJ: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";

struct Env {
    pki: Arc<Pki>,
    pairing_string: String,
    api_hits: Arc<AtomicUsize>,
    log_epoch: Arc<Mutex<u64>>,
    _dir: TempDir,
    shell: Shell,
    keys: TestSoftwareKeyStore,
}

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn env(name: &str, clock: Clock) -> Env {
    let pki = Arc::new(Pki::new());
    let api_hits = Arc::new(AtomicUsize::new(0));
    let log_epoch = Arc::new(Mutex::new(1u64));

    // API（mTLS）。
    let api: Handler = {
        let (hits, epoch) = (api_hits.clone(), log_epoch.clone());
        Arc::new(move |seen: Seen| {
            hits.fetch_add(1, Ordering::SeqCst);
            let epoch = *epoch.lock().unwrap();
            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-log-epoch".to_string(), epoch.to_string()),
                ("etag".to_string(), "\"r1\"".to_string()),
                ("x-request-id".to_string(), "req_01ABC".to_string()),
            ];
            match seen.path.as_str() {
                "/v1/me" => (200, headers, serde_json::to_vec(&json!({"server": {"log_epoch": epoch}})).unwrap()),
                p if p.starts_with("/v1/projects/") => (
                    404,
                    headers,
                    br#"{"code":"not_found","title":"Not Found","request_id":"req_1","detail":"secret internals SENTINEL_DETAIL"}"#.to_vec(),
                ),
                _ => (200, headers, serde_json::to_vec(&json!({"items": [], "next_cursor": null})).unwrap()),
            }
        })
    };
    let api_addr = serve_simple(pki.server_config(true), api).await;

    // 配对监听器（单向 TLS）。
    let pair: Handler = {
        let pki = pki.clone();
        Arc::new(move |seen: Seen| {
            let body: Value = serde_json::from_slice(&seen.body).unwrap();
            if body["token"] != TOKEN {
                return (401, vec![], br#"{"code":"pair_token_invalid"}"#.to_vec());
            }
            let tls =
                gantry_pki::csr::validate_csr(body["csr"].as_str().unwrap(), 16 * 1024).unwrap();
            let approval =
                gantry_pki::csr::parse_approval_pubkey(body["approval_pubkey"].as_str().unwrap())
                    .unwrap();
            let [inter, root] = pki.chain_tail();
            let resp = json!({
                "device": {
                    "id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM", "name": body["device"]["name"], "platform": body["device"]["platform"],
                    "pubkey_fpr": tls.fingerprint().to_string(), "approval_pubkey_fpr": approval.fingerprint().to_string(),
                    "role": "approver",
                },
                "certificate_chain": [pki.issue_device(&tls), inter, root],
                "api_base_url": format!("https://localhost:{}", api_addr.port()),
                "ca_fingerprint": pki.fp(),
                "confirmation_code": "482913",
            });
            (201, vec![], serde_json::to_vec(&resp).unwrap())
        })
    };
    let pair_addr = serve_simple(pki.server_config(false), pair).await;
    let pairing_string = format!(
        "gantry+pair://localhost:{}/?v=1&fp={}&t={TOKEN}",
        pair_addr.port(),
        pki.fp()
    );

    let dir = std::env::temp_dir().join(format!(
        "gantry-shell-session-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let keys = TestSoftwareKeyStore::new(PresenceDouble::new());
    let shell = Shell::new(
        AppStore::open(&dir).unwrap(),
        Box::new(keys.clone()),
        Box::new(HttpPairTransport::default()),
        clock,
    )
    .with_log_dir(&dir.join("logs"));
    Env {
        pki,
        pairing_string,
        api_hits,
        log_epoch,
        _dir: TempDir(dir),
        shell,
        keys,
    }
}

#[tokio::test]
async fn pair_confirm_status_request_reset() {
    let e = env("full", system_clock()).await;
    let shell = &e.shell;
    assert_eq!(shell.identity_status().unwrap()["state"], "unpaired");
    // 未配对时 api_request 不发出。
    assert_eq!(
        shell
            .api_request("me.get", "/v1/me", None, None, None)
            .await
            .unwrap_err()
            .code,
        ShellErrorCode::NotPaired
    );

    let begun = shell
        .pairing_begin(&e.pairing_string, "Susan 的 MacBook")
        .await
        .unwrap();
    assert_eq!(begun["confirmation_code"], "482913");
    assert_eq!(shell.identity_status().unwrap()["state"], "pending_confirm");
    // 待确认的身份尚不可用于 API。
    assert_eq!(
        shell
            .api_request("me.get", "/v1/me", None, None, None)
            .await
            .unwrap_err()
            .code,
        ShellErrorCode::NotPaired
    );

    let status = shell.pairing_confirm(true).await.unwrap();
    assert_eq!(status["state"], "paired");
    assert_eq!(status["ca_fingerprint"], e.pki.fp());
    assert_eq!(status["device"]["name"], "Susan 的 MacBook");
    assert_eq!(status["key_backend"], "test_software");
    assert!(status["cert"]["serial"].as_str().unwrap().len() >= 2);
    // GET /v1/me 已用新证书成功，log_epoch 记下。
    assert_eq!(shell.store().identity().unwrap().log_epoch, Some(1));
    assert!(e.api_hits.load(Ordering::SeqCst) >= 1);

    // api_request 经 mTLS 发出，响应已整形。
    let ok = shell
        .api_request(
            "projects.list",
            "/v1/projects",
            Some(&json!({"limit": 5})),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["headers"], json!({"etag": "\"r1\""}));
    let not_found = shell
        .api_request(
            "projects.get",
            &format!("/v1/projects/{PRJ}"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(not_found["problem"]["code"], "not_found");
    assert!(not_found["problem"].get("detail").is_none());

    // 本地校验不通过的请求不会到达服务端。
    let before = e.api_hits.load(Ordering::SeqCst);
    for (route, path) in [
        ("approvals.grant", "/v1/me"),
        ("projects.get", "/v1/projects/x"),
    ] {
        assert!(shell
            .api_request(route, path, None, None, None)
            .await
            .is_err());
    }
    assert_eq!(e.api_hits.load(Ordering::SeqCst), before);

    // 已配对时不能再 pairing_begin。
    assert!(shell.pairing_begin(&e.pairing_string, "x").await.is_err());

    // 重置：密钥、证书、identity.json、ui-state 删除，prefs 保留。
    shell.store().set_prefs(&json!({"theme": "dark"})).unwrap();
    shell
        .store()
        .ui_state_set(PRJ, &json!({"draft": "x"}))
        .unwrap();
    let status = shell.identity_reset().unwrap();
    assert_eq!(status["state"], "unpaired");
    assert_eq!(e.keys.key_count(), 0);
    assert!(shell.store().identity().is_none());
    assert!(shell.store().ui_state_list().unwrap().is_empty());
    assert_eq!(
        shell.store().prefs().theme,
        gantry_desktop_shell::storage::Theme::Dark
    );
    assert_eq!(
        shell
            .api_request("me.get", "/v1/me", None, None, None)
            .await
            .unwrap_err()
            .code,
        ShellErrorCode::NotPaired
    );
}

#[tokio::test]
async fn confirm_false_and_timeout_delete_keys() {
    let e = env("confirm", system_clock()).await;
    e.shell
        .pairing_begin(&e.pairing_string, "mac")
        .await
        .unwrap();
    assert_eq!(e.keys.key_count(), 2);
    assert_eq!(
        e.shell.pairing_confirm(false).await.unwrap()["state"],
        "unpaired"
    );
    assert_eq!(e.keys.key_count(), 0);

    // 超时：时钟拨过 10 分钟。
    let offset = Arc::new(Mutex::new(0i64));
    let clock: Clock = {
        let offset = offset.clone();
        let base = system_clock();
        Arc::new(move || base() + *offset.lock().unwrap())
    };
    let e = env("timeout", clock).await;
    e.shell
        .pairing_begin(&e.pairing_string, "mac")
        .await
        .unwrap();
    *offset.lock().unwrap() = 11 * 60 * 1000;
    assert_eq!(e.shell.identity_status().unwrap()["state"], "unpaired");
    let err = e.shell.pairing_confirm(true).await.unwrap_err();
    assert_eq!(err.reason.as_deref(), Some("confirm_timeout"));
    assert_eq!(e.keys.key_count(), 0);
    assert!(e.shell.store().identity().is_none());
}

#[tokio::test]
async fn lost_keys_and_corrupt_identity() {
    let e = env("lost", system_clock()).await;
    e.shell
        .pairing_begin(&e.pairing_string, "mac")
        .await
        .unwrap();
    e.shell.pairing_confirm(true).await.unwrap();

    // 模拟 TPM 清除：新进程（新 Shell）加载时密钥不在了。
    e.keys.wipe();
    let fresh = Shell::new(
        AppStore::open(e.shell.store().dir()).unwrap(),
        Box::new(e.keys.clone()),
        Box::new(HttpPairTransport::default()),
        system_clock(),
    );
    assert_eq!(
        fresh.identity_status().unwrap_err().code,
        ShellErrorCode::KeyUnavailable
    );
    // 重置仍然可用（密钥已不在也算删除成功）。
    assert_eq!(fresh.identity_reset().unwrap()["state"], "unpaired");

    // identity.json 损坏 → 未配对。
    std::fs::write(e.shell.store().dir().join("identity.json"), "{broken").unwrap();
    let fresh = Shell::new(
        AppStore::open(e.shell.store().dir()).unwrap(),
        Box::new(e.keys.clone()),
        Box::new(HttpPairTransport::default()),
        system_clock(),
    );
    assert_eq!(fresh.identity_status().unwrap()["state"], "unpaired");
}

#[tokio::test]
async fn log_epoch_change_clears_prepared_signings() {
    let e = env("epoch", system_clock()).await;
    e.shell
        .pairing_begin(&e.pairing_string, "mac")
        .await
        .unwrap();
    e.shell.pairing_confirm(true).await.unwrap();
    assert!(!e.shell.observe_log_epoch(1).unwrap());

    // 放一个待签句柄进去（内容无关紧要，只看是否被清掉）。
    let checked = gantry_desktop_shell::approval::self_check(
        "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
        &json!({
            "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
            "payload": {
                "action": "merge_pr", "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
                "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB", "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
                "expires_at": "2099-01-01T00:00:00.000Z", "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
                "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
                "target": {"kind": "pr", "number": 12, "repo": "acme/web"}, "v": 1
            },
            "signing": {"alg": "ES256", "canonicalization": "JCS", "prefix": "gantry/approval/v1\n"}
        }),
        &json!({"action": "merge_pr", "target": {"kind": "pr", "number": 12, "repo": "acme/web"},
                "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345", "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"}),
        "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
        0,
        None,
    )
    .unwrap();
    e.shell.signer().insert(checked, 0).unwrap();

    // 服务端从备份恢复：X-Log-Epoch 变成 2，任一响应都能触发。
    *e.log_epoch.lock().unwrap() = 2;
    e.shell
        .api_request("me.get", "/v1/me", None, None, None)
        .await
        .unwrap();
    assert_eq!(e.shell.store().identity().unwrap().log_epoch, Some(2));
    assert!(e.shell.signer().is_empty());
}

fn all_logs(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        out.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
    }
    out
}

#[tokio::test]
async fn logs_and_diagnostics_carry_no_secrets() {
    let e = env("redact", system_clock()).await;
    let shell = &e.shell;
    shell.pairing_begin(&e.pairing_string, "mac").await.unwrap();
    shell.pairing_confirm(true).await.unwrap();
    shell
        .api_request(
            "messages.post",
            "/v1/conversations/cnv_01JB7K2M9Q4X8V3N5R6T1W0YZA/messages",
            None,
            Some(&json!({"text": "正文 SENTINEL_BODY"})),
            Some("idem_SENTINELkey012345"),
        )
        .await
        .unwrap();
    shell
        .api_request(
            "projects.get",
            &format!("/v1/projects/{PRJ}"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    // 失败的命令也只记错误码。
    let _ = shell.track(
        "pairing_begin",
        shell.pairing_begin(&e.pairing_string, "again").await,
    );

    let logs = all_logs(&shell.store().dir().join("logs"));
    assert!(logs.contains("\"event\":\"api_request\""), "{logs}");
    assert!(logs.contains("\"route\":\"postMessage\""));
    assert!(logs.contains("\"request_id\""));
    let snapshot = shell.diagnostics_snapshot().to_string();
    for sentinel in [
        TOKEN,
        "SENTINEL_BODY",
        "SENTINEL_DETAIL",
        "idem_SENTINELkey012345",
        "BEGIN CERTIFICATE",
    ] {
        assert!(!logs.contains(sentinel), "日志里出现了 {sentinel}");
        assert!(!snapshot.contains(sentinel), "诊断快照里出现了 {sentinel}");
    }
    let snap = shell.diagnostics_snapshot();
    assert_eq!(snap["key_backend"], "test_software");
    assert!(
        snap["counters"]["api_requests{route=postMessage,status=200}"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(snap["recent_errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "shell.bad_argument"));
    assert!(snap["cert"]["serial"].is_string());
}
