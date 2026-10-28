//! 事件流运行器（desktop-shell.md §12"事件流"、"端到端（外壳层）"的 A 级部分）。
//!
//! 进程内 mTLS 服务端按脚本逐连接推送 SSE；外壳的身份直接由 TestCA 签发后写入 `identity.json`。
#![cfg(feature = "test-software-keys")]

mod support;

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gantry_desktop_shell::approval::self_check;
use gantry_desktop_shell::events::{EventsConfig, FrameSink};
use gantry_desktop_shell::http::HttpPairTransport;
use gantry_desktop_shell::keys::software::{PresenceDouble, TestSoftwareKeyStore};
use gantry_desktop_shell::keys::KeyStore;
use gantry_desktop_shell::session::{system_clock, Shell};
use gantry_desktop_shell::storage::{AppStore, IdentityFile};
use http_body_util::StreamBody;
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use support::Pki;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

/// 服务端对一条连接的安排。
enum Reply {
    /// 非 200 的普通响应。
    Status(u16, &'static str),
    /// 200 流：依次写出这些块；`hold` 为真时写完后保持连接不发任何字节。
    Stream {
        epoch: u64,
        chunks: Vec<String>,
        hold: bool,
    },
    /// 同上，但写完后只保持 `linger_ms` 毫秒就关闭。
    StreamThenClose {
        epoch: u64,
        chunks: Vec<String>,
        linger_ms: u64,
    },
}

type Script = Arc<dyn Fn(usize, Option<String>) -> Reply + Send + Sync>;

struct Server {
    /// 每条连接带来的 `Last-Event-ID`。
    seen: Arc<Mutex<Vec<Option<String>>>>,
    port: u16,
}

async fn sse_server(pki: &Pki, script: Script) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = TlsAcceptor::from(pki.server_config(true));
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen_task = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, script, seen) = (acceptor.clone(), script.clone(), seen_task.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let (script, seen) = (script.clone(), seen.clone());
                    async move {
                        let last = req
                            .headers()
                            .get("last-event-id")
                            .map(|v| v.to_str().unwrap().to_string());
                        let index = {
                            let mut s = seen.lock().unwrap();
                            s.push(last.clone());
                            s.len() - 1
                        };
                        let (tx, rx) = mpsc::channel::<Bytes>(16);
                        let body =
                            StreamBody::new(futures_util::stream::unfold(rx, |mut rx| async {
                                rx.recv()
                                    .await
                                    .map(|b| (Ok::<_, Infallible>(Frame::data(b)), rx))
                            }));
                        let resp = match script(index, last) {
                            Reply::Status(status, code) => {
                                let _ = tx.try_send(Bytes::from(format!(
                                    r#"{{"code":"{code}","title":"","request_id":"r"}}"#
                                )));
                                drop(tx);
                                Response::builder().status(status).body(body).unwrap()
                            }
                            Reply::StreamThenClose {
                                epoch,
                                chunks,
                                linger_ms,
                            } => {
                                tokio::spawn(async move {
                                    for c in chunks {
                                        if tx.send(Bytes::from(c)).await.is_err() {
                                            return;
                                        }
                                    }
                                    tokio::time::sleep(Duration::from_millis(linger_ms)).await;
                                });
                                Response::builder()
                                    .status(200)
                                    .header("content-type", "text/event-stream")
                                    .header("x-log-epoch", epoch.to_string())
                                    .body(body)
                                    .unwrap()
                            }
                            Reply::Stream {
                                epoch,
                                chunks,
                                hold,
                            } => {
                                tokio::spawn(async move {
                                    for c in chunks {
                                        if tx.send(Bytes::from(c)).await.is_err() {
                                            return;
                                        }
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                    }
                                    if hold {
                                        tokio::time::sleep(Duration::from_secs(30)).await;
                                    }
                                });
                                Response::builder()
                                    .status(200)
                                    .header("content-type", "text/event-stream")
                                    .header("x-log-epoch", epoch.to_string())
                                    .body(body)
                                    .unwrap()
                            }
                        };
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    Server { seen, port }
}

fn ev(seq: u64) -> String {
    let envelope =
        json!({"id": format!("evt_{seq}"), "seq": seq, "type": "message.posted", "data": {}});
    format!("id: {seq}\nevent: message.posted\ndata: {envelope}\n\n")
}

fn hb(seq: u64, epoch: u64) -> String {
    format!("event: heartbeat\nid: {seq}\ndata: {{\"seq\":{seq},\"log_epoch\":{epoch}}}\n\n")
}

fn close(reason: &str) -> String {
    format!("event: close\ndata: {{\"reason\":\"{reason}\"}}\n\n")
}

#[derive(Default)]
struct Collect {
    frames: Mutex<Vec<Value>>,
}

impl FrameSink for Collect {
    fn send(&self, frame: Value) -> bool {
        self.frames.lock().unwrap().push(frame);
        true
    }
}

impl Collect {
    fn events(&self) -> Vec<u64> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f["t"] == "event")
            .map(|f| f["envelope"]["seq"].as_u64().unwrap())
            .collect()
    }

    fn has(&self, pred: impl Fn(&Value) -> bool) -> bool {
        self.frames.lock().unwrap().iter().any(pred)
    }

    fn phases(&self) -> Vec<String> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f["t"] == "state")
            .map(|f| f["state"]["phase"].as_str().unwrap().to_string())
            .collect()
    }
}

struct Env {
    shell: Arc<Shell>,
    keys: TestSoftwareKeyStore,
    _dir: std::path::PathBuf,
}

fn fast() -> EventsConfig {
    EventsConfig {
        idle: Duration::from_millis(400),
        backoff_scale: 0.001,
        handshake_fail_limit: 3,
    }
}

/// 直接写好一份已配对的身份（证书由 `issuer` 的中间 CA 签发，根固定为 `pinned`）。
fn paired(name: &str, issuer: &Pki, pinned: &Pki, port: u16) -> Env {
    let dir =
        std::env::temp_dir().join(format!("gantry-shell-events-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let keys = TestSoftwareKeyStore::new(PresenceDouble::new());
    let device = keys.generate().unwrap();
    let public =
        gantry_pki::P256PublicKey::from_spki_der(&device.tls_public_key().spki_der()).unwrap();
    let [inter, root] = issuer.chain_tail();
    let store = AppStore::open(&dir).unwrap();
    store
        .save_identity(&IdentityFile {
            api_base_url: format!("https://localhost:{port}"),
            ca_fingerprint: pinned.fp(),
            device_id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".into(),
            device_name: "mac".into(),
            platform: "macos".into(),
            cert_chain_pem: vec![issuer.issue_device(&public), inter, root],
            tls_key_label: device.labels().tls.clone(),
            approval_key_label: device.labels().approval.clone(),
            paired_at: "2026-10-27T00:00:00.000Z".into(),
            renewed_at: None,
            log_epoch: Some(1),
            revoked: false,
        })
        .unwrap();
    let shell = Arc::new(
        Shell::new(
            store,
            Box::new(keys.clone()),
            Box::new(HttpPairTransport::default()),
            system_clock(),
        )
        .with_events_config(fast()),
    );
    Env {
        shell,
        keys,
        _dir: dir,
    }
}

async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时");
}

#[tokio::test]
async fn reconnect_resumes_from_acked_cursor_without_loss_or_duplicates() {
    let pki = Pki::new();
    // 第一条连接发到 3 后停留 300 ms 再断开（给 UI ack 的时间）；之后的连接从游标补发到 5。
    let script: Script = Arc::new(|i, last| {
        let after: u64 = last.and_then(|l| l.parse().ok()).unwrap_or(0);
        if i == 0 {
            Reply::StreamThenClose {
                epoch: 1,
                chunks: (1..=3).map(ev).collect(),
                linger_ms: 300,
            }
        } else {
            Reply::Stream {
                epoch: 1,
                chunks: (after + 1..=5).map(ev).collect(),
                hold: true,
            }
        }
    });
    let server = sse_server(&pki, script).await;
    let env = paired("resume", &pki, &pki, server.port);
    let sink = Arc::new(Collect::default());
    env.shell.events_open(None, sink.clone()).unwrap();

    wait_until(|| sink.events().len() >= 3).await;
    // 只 ack 到 2：第 3 个未 ack，断线后会被重放。
    env.shell.events_ack(2);
    wait_until(|| server.seen.lock().unwrap().len() >= 2).await;
    assert_eq!(
        server.seen.lock().unwrap()[1].as_deref(),
        Some("2"),
        "重连带 Last-Event-ID = 已 ack 的最大 seq"
    );
    wait_until(|| sink.events().contains(&5)).await;
    // UI 看到的序列不重复（3 被服务端重放，但外壳按 seq 去重）。
    assert_eq!(sink.events(), vec![1, 2, 3, 4, 5]);
    assert!(sink.phases().contains(&"live".to_string()));
    assert!(sink.phases().contains(&"reconnecting".to_string()));
    let state = env.shell.connection_state();
    assert!(state["epoch"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn cursor_ahead_sends_resync_and_does_not_retry() {
    let pki = Pki::new();
    let server = sse_server(&pki, Arc::new(|_, _| Reply::Status(409, "cursor_ahead"))).await;
    let env = paired("ahead", &pki, &pki, server.port);
    let sink = Arc::new(Collect::default());
    env.shell.events_open(Some(999), sink.clone()).unwrap();
    wait_until(|| sink.has(|f| f["t"] == "resync_required")).await;
    assert!(sink.has(|f| f["reason"] == "cursor_ahead"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.seen.lock().unwrap().len(), 1, "不带旧游标重试");
}

#[tokio::test]
async fn log_epoch_change_resyncs_drops_signings_and_reopens_without_cursor() {
    let pki = Pki::new();
    let script: Script = Arc::new(|i, _| match i {
        0 => Reply::Stream {
            epoch: 1,
            chunks: vec![ev(1), hb(4, 2)],
            hold: true,
        },
        _ => Reply::Stream {
            epoch: 2,
            chunks: vec![hb(1, 2)],
            hold: true,
        },
    });
    let server = sse_server(&pki, script).await;
    let env = paired("epoch", &pki, &pki, server.port);
    let challenge = json!({
        "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
        "payload": {"action": "merge_pr", "approval_id": "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
            "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB", "device_id": "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
            "expires_at": "2099-01-01T00:00:00.000Z", "nonce": "lIXSmI8Oxz4mQbtZkKkIguvXjQtExu3PzyWZHQSw7js",
            "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345",
            "target": {"kind": "pr", "number": 12, "repo": "acme/web"}, "v": 1},
        "signing": {"alg": "ES256", "canonicalization": "JCS", "prefix": "gantry/approval/v1\n"}
    });
    let checked = self_check(
        "apr_01JB7P4M8T2X6N9R3Q5K1V7WZK",
        &challenge,
        &json!({"action": "merge_pr", "target": challenge["payload"]["target"], "subject_digest": "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345", "delivery_id": "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB"}),
        "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM",
        0,
        None,
    )
    .unwrap();
    env.shell.signer().insert(checked, 0).unwrap();

    let sink = Arc::new(Collect::default());
    env.shell.events_open(Some(0), sink.clone()).unwrap();
    wait_until(|| sink.has(|f| f["reason"] == "log_epoch_changed")).await;
    assert!(env.shell.signer().is_empty(), "丢弃 PreparedSigning");
    assert_eq!(env.shell.store().identity().unwrap().log_epoch, Some(2));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.seen.lock().unwrap().len(), 1, "断流后不自行重连");

    // UI 全量重同步后不带游标重开。
    let sink2 = Arc::new(Collect::default());
    env.shell.events_open(None, sink2.clone()).unwrap();
    wait_until(|| server.seen.lock().unwrap().len() >= 2).await;
    assert_eq!(
        server.seen.lock().unwrap()[1],
        None,
        "重连不带 Last-Event-ID"
    );
}

#[tokio::test]
async fn idle_stream_reconnects_and_lagging_reconnects_immediately() {
    let pki = Pki::new();
    let script: Script = Arc::new(|i, _| match i {
        0 => Reply::Stream {
            epoch: 1,
            chunks: vec![ev(1)],
            hold: true,
        }, // 之后不发任何字节
        1 => Reply::Stream {
            epoch: 1,
            chunks: vec![ev(2), close("lagging")],
            hold: true,
        },
        _ => Reply::Stream {
            epoch: 1,
            chunks: vec![ev(3)],
            hold: true,
        },
    });
    let server = sse_server(&pki, script).await;
    let env = paired("idle", &pki, &pki, server.port);
    let sink = Arc::new(Collect::default());
    env.shell.events_open(None, sink.clone()).unwrap();
    let mut observed = false;
    for _ in 0..400 {
        if sink.events() == vec![1, 2, 3] {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        observed,
        "等待超时: events={:?}, phases={:?}, connections={}",
        sink.events(),
        sink.phases(),
        server.seen.lock().unwrap().len()
    );
    assert!(server.seen.lock().unwrap().len() >= 3);
    assert!(sink.phases().contains(&"reconnecting".to_string()));
}

#[tokio::test]
async fn device_revoked_close_deletes_keys() {
    let pki = Pki::new();
    let server = sse_server(
        &pki,
        Arc::new(|_, _| Reply::Stream {
            epoch: 1,
            chunks: vec![ev(1), close("device_revoked")],
            hold: true,
        }),
    )
    .await;
    let env = paired("revoked", &pki, &pki, server.port);
    let sink = Arc::new(Collect::default());
    env.shell.events_open(None, sink.clone()).unwrap();
    wait_until(|| sink.phases().contains(&"revoked".to_string())).await;
    assert_eq!(env.keys.key_count(), 0, "两把密钥被删除");
    assert_eq!(env.shell.identity_status().unwrap()["state"], "revoked");
    assert!(env
        .shell
        .store()
        .identity()
        .unwrap()
        .cert_chain_pem
        .is_empty());
    assert_eq!(env.shell.connection_state()["phase"], "revoked");
    // 之后的请求不再发出。
    assert!(env
        .shell
        .api_request("me.get", "/v1/me", None, None, None)
        .await
        .is_err());
    // 重置后回到未配对。
    assert_eq!(env.shell.identity_reset().unwrap()["state"], "unpaired");
}

#[tokio::test]
async fn cert_expired_close_stops() {
    let pki = Pki::new();
    let server = sse_server(
        &pki,
        Arc::new(|_, _| Reply::Stream {
            epoch: 1,
            chunks: vec![close("cert_expired")],
            hold: true,
        }),
    )
    .await;
    let env = paired("expired", &pki, &pki, server.port);
    let sink = Arc::new(Collect::default());
    env.shell.events_open(None, sink.clone()).unwrap();
    wait_until(|| sink.phases().contains(&"cert_expired".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.seen.lock().unwrap().len(), 1, "不再重试");
}

#[tokio::test]
async fn three_tls_failures_give_handshake_failed_without_deleting_keys() {
    let pki = Pki::new();
    let other = Pki::new();
    // 服务端证书来自另一套 CA：每次握手都在 TLS 层失败。
    let server = sse_server(&other, Arc::new(|_, _| Reply::Status(200, ""))).await;
    let env = paired("handshake", &pki, &pki, server.port);
    let sink = Arc::new(Collect::default());
    env.shell.events_open(None, sink.clone()).unwrap();
    wait_until(|| sink.phases().contains(&"handshake_failed".to_string())).await;
    assert_eq!(env.keys.key_count(), 2, "不自动清除密钥");
    assert_eq!(env.shell.identity_status().unwrap()["state"], "paired");
}

#[tokio::test]
async fn only_one_stream_at_a_time() {
    let pki = Pki::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let script: Script = {
        let counter = counter.clone();
        Arc::new(move |_, _| {
            let n = counter.fetch_add(1, Ordering::SeqCst) as u64;
            Reply::Stream {
                epoch: 1,
                chunks: vec![ev(n * 10 + 1)],
                hold: true,
            }
        })
    };
    let server = sse_server(&pki, script).await;
    let env = paired("single", &pki, &pki, server.port);
    let first = Arc::new(Collect::default());
    env.shell.events_open(None, first.clone()).unwrap();
    wait_until(|| !first.events().is_empty()).await;
    let second = Arc::new(Collect::default());
    env.shell.events_open(None, second.clone()).unwrap();
    wait_until(|| !second.events().is_empty()).await;
    let before = first.frames.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        first.frames.lock().unwrap().len(),
        before,
        "旧流已关闭，不再收到帧"
    );
    env.shell.events_close();
    assert_eq!(server.seen.lock().unwrap().len(), 2);
}
