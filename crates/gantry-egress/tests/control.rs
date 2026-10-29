//! 控制面服务测试：`docs/interfaces/egress-control.md` §2（Unix socket、对端 uid）、§4（各操作的
//! HTTP 形状与状态码）、§6（problem+json 与错误码）；`egress-call-terminal.md` §2（`scope: call`）。

#![cfg(unix)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gantry_core::secret::Consumer;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_egress::audit::MemoryAudit;
use gantry_egress::control::ControlServer;
use gantry_egress::db::Db;
use gantry_egress::registry::{Endpoints, Registry, RegistryOptions};
use gantry_fakes::secrets::MemorySecretStore;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::{UnixListener, UnixStream};

const T0: i64 = 1_793_000_000_000;
const SENTINEL: &str = "sk-gantry-sentinel-control-0001";

struct Server {
    socket: PathBuf,
    audit: Arc<MemoryAudit>,
    _dir: TempDir,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn current_uid(dir: &Path) -> u32 {
    std::fs::metadata(dir).unwrap().uid()
}

async fn start(name: &str, uid_offset: u32) -> Server {
    // Unix socket 路径有长度上限，放在短目录里。
    let dir = PathBuf::from(format!("/tmp/ge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("egress.sock");
    let clock = Arc::new(FakeClock::new(T0));
    let store = MemorySecretStore::new(clock.now_timestamp());
    store.allow(Consumer::Egress);
    let registry = Registry::open(
        Db::open_in_memory().unwrap(),
        clock,
        Arc::new(SeqEntropy::new(3)),
        Arc::new(store.scoped(Consumer::Egress).unwrap()),
        RegistryOptions::new(Endpoints {
            sandbox_model_base: "http://10.89.0.1:8081".into(),
            connect_proxy_authority: "10.89.0.1:3128".into(),
            loopback_model_base: "http://127.0.0.1:8082".into(),
        }),
    )
    .unwrap();
    let audit = Arc::new(MemoryAudit::new());
    let listener = UnixListener::bind(&socket).unwrap();
    let uid = current_uid(&dir).wrapping_add(uid_offset);
    tokio::spawn(ControlServer::new(registry, audit.clone(), uid).serve(listener));
    Server {
        socket,
        audit,
        _dir: TempDir(dir),
    }
}

struct Reply {
    status: u16,
    content_type: String,
    body: Value,
    raw: String,
}

impl Server {
    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.send_raw(
            method,
            path,
            body.map(|b| b.to_string()).unwrap_or_default(),
        )
        .await
        .expect("response")
    }

    async fn send_raw(&self, method: &str, path: &str, body: String) -> Option<Reply> {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        tokio::spawn(conn);
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "egress")
            .header("content-type", "application/json")
            .header("x-gantry-request-id", "01JBX5C7E9G2J4K6N8Q0S3T5WY")
            .body(Full::new(Bytes::from(body)))
            .unwrap();
        let response = sender.send_request(request).await.ok()?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = response.into_body().collect().await.ok()?.to_bytes();
        let raw = String::from_utf8(bytes.to_vec()).unwrap();
        let body = serde_json::from_str(&raw).unwrap_or(Value::Null);
        Some(Reply {
            status,
            content_type,
            body,
            raw,
        })
    }

    async fn configure(&self) {
        let reply = self.send("PUT", "/control/v1/config", Some(view(7))).await;
        assert_eq!(reply.status, 200, "{}", reply.raw);
        assert_eq!(reply.body, json!({"applied_rev": 7}));
        let staged = self
            .send(
                "POST",
                "/control/v1/secrets/stage",
                Some(json!({
                    "request_id": "01JBX5C7E9G2J4K6N8Q0S3T5WY",
                    "secret_ref": "model.anthropic.main",
                    "value": SENTINEL
                })),
            )
            .await;
        assert_eq!(staged.status, 201, "{}", staged.raw);
        let id = staged.body["staged_id"].as_str().unwrap().to_owned();
        let activated = self
            .send(
                "POST",
                &format!("/control/v1/secrets/staged/{id}/activate"),
                Some(json!({})),
            )
            .await;
        assert_eq!(activated.status, 200, "{}", activated.raw);
    }
}

fn view(rev: u64) -> Value {
    json!({
        "rev": rev,
        "providers": {
            "anthropic": {
                "kind": "anthropic",
                "base_url": "https://api.anthropic.com",
                "secret_ref": "model.anthropic.main"
            }
        },
        "groups": {
            "model_api": { "hosts": ["api.anthropic.com"] },
            "npm": { "hosts": ["registry.npmjs.org"] }
        },
        "profiles": { "default": { "groups": ["model_api", "npm"] } },
        "default_profile": "default",
        "injection": {},
        "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 }
    })
}

fn job_request() -> Value {
    json!({
        "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
        "sandbox_id": "sbx_01JBX4R2M6P8Q1S3T5V7W9Y0ZA",
        "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
        "ledger": { "subject": "delivery", "id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD" },
        "tier": "worker",
        "provider": "anthropic",
        "profile": "default",
        "groups": ["model_api", "npm"],
        "tokens_total": 800000,
        "token_weights": { "input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write": 1.25 },
        "wall_time_s": 1800,
        "config_rev": 7
    })
}

fn assert_problem(reply: &Reply, status: u16, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.raw);
    assert_eq!(reply.content_type, "application/problem+json");
    assert_eq!(reply.body["code"], code, "{}", reply.raw);
    assert_eq!(reply.body["status"], status);
    assert_eq!(reply.body["type"], format!("urn:gantry:problem:{code}"));
    assert_eq!(reply.body["request_id"], "01JBX5C7E9G2J4K6N8Q0S3T5WY");
}

#[tokio::test]
async fn peers_with_the_wrong_uid_are_dropped_without_a_response() {
    let server = start("peer", 1).await;
    let reply = server
        .send_raw("GET", "/control/v1/status", String::new())
        .await;
    assert!(reply.is_none(), "connection must close without HTTP");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events = server.audit.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "control");
    assert_eq!(events[0]["decision"], "deny");
    assert_eq!(events[0]["rule"], "control.peer_not_allowed");
    assert!(events[0]["caller"]["src"]
        .as_str()
        .unwrap()
        .starts_with("uid:"));
}

#[tokio::test]
async fn status_and_endpoints_have_the_contract_shape() {
    let server = start("status", 0).await;
    let status = server.send("GET", "/control/v1/status", None).await;
    assert_eq!(status.status, 200);
    assert_eq!(status.body["protocol"], "gantry-egress-control/1");
    assert_eq!(status.body["health"], "unhealthy");
    assert_eq!(status.body["applied_rev"], Value::Null);
    server.configure().await;
    let status = server.send("GET", "/control/v1/status", None).await;
    assert_eq!(status.body["health"], "ok");
    assert_eq!(status.body["applied_rev"], 7);
    assert_eq!(status.body["detail"], "source_check: unverified");
    assert_eq!(status.body["unresolved_secret_refs"], json!([]));

    let endpoints = server.send("GET", "/control/v1/endpoints", None).await;
    assert_eq!(
        endpoints.body,
        json!({
            "sandbox_model_base": "http://10.89.0.1:8081",
            "connect_proxy_url": "http://10.89.0.1:3128",
            "loopback_model_base": "http://127.0.0.1:8082",
            "registry_base": null
        })
    );
    assert_problem(
        &server.send("GET", "/control/v1/nope", None).await,
        404,
        "not_found",
    );
    assert_problem(
        &server.send("GET", "/v1/status", None).await,
        404,
        "not_found",
    );
}

#[tokio::test]
async fn job_lifecycle_over_the_socket() {
    let server = start("jobs", 0).await;
    assert_problem(
        &server
            .send("POST", "/control/v1/jobs", Some(job_request()))
            .await,
        503,
        "not_configured",
    );
    server.configure().await;

    let issued = server
        .send("POST", "/control/v1/jobs", Some(job_request()))
        .await;
    assert_eq!(issued.status, 201, "{}", issued.raw);
    assert_eq!(issued.body["reissued"], false);
    let token = issued.body["job_token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("gjt1_"));
    let reissued = server
        .send("POST", "/control/v1/jobs", Some(job_request()))
        .await;
    assert_eq!(reissued.status, 200);
    assert_eq!(reissued.body["reissued"], true);

    let mut other = job_request();
    other["tokens_total"] = json!(1);
    assert_problem(
        &server.send("POST", "/control/v1/jobs", Some(other)).await,
        409,
        "binding_mismatch",
    );

    let id = "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD";
    let usage = server
        .send("GET", &format!("/control/v1/jobs/{id}/usage"), None)
        .await;
    assert_eq!(usage.body["state"], "active");
    assert_problem(
        &server
            .send("DELETE", &format!("/control/v1/jobs/{id}"), None)
            .await,
        409,
        "conflict",
    );
    assert_eq!(
        server
            .send("GET", &format!("/control/v1/jobs/{id}/usage"), None)
            .await
            .body,
        usage.body
    );
    let revoked = server
        .send(
            "POST",
            &format!("/control/v1/jobs/{id}/revoke"),
            Some(json!({})),
        )
        .await;
    assert_eq!(revoked.status, 200);
    assert_eq!(revoked.body["state"], "revoked");
    let again = server
        .send(
            "POST",
            &format!("/control/v1/jobs/{id}/revoke"),
            Some(json!({})),
        )
        .await;
    assert_eq!(again.body, revoked.body);
    assert_problem(
        &server
            .send("POST", "/control/v1/jobs", Some(job_request()))
            .await,
        409,
        "job_revoked",
    );

    let forgotten = server
        .send("DELETE", &format!("/control/v1/jobs/{id}"), None)
        .await;
    assert_eq!(forgotten.status, 200);
    assert_problem(
        &server
            .send(
                "POST",
                &format!("/control/v1/jobs/{id}/revoke"),
                Some(json!({})),
            )
            .await,
        404,
        "not_found",
    );
}

#[tokio::test]
async fn request_bodies_are_validated_strictly() {
    let server = start("strict", 0).await;
    server.configure().await;
    let mut unknown = job_request();
    unknown["extra"] = json!(true);
    assert_problem(
        &server.send("POST", "/control/v1/jobs", Some(unknown)).await,
        422,
        "validation_failed",
    );
    let syntax = server
        .send_raw("POST", "/control/v1/jobs", "{not json".into())
        .await
        .unwrap();
    assert_problem(&syntax, 400, "invalid_request");
    let too_big = server
        .send_raw("POST", "/control/v1/jobs", "x".repeat(70 * 1024))
        .await
        .unwrap();
    assert_problem(&too_big, 400, "invalid_request");
    let mut groups = job_request();
    groups["groups"] = json!(["pypi"]);
    let reply = server.send("POST", "/control/v1/jobs", Some(groups)).await;
    assert_problem(&reply, 422, "validation_failed");
    assert_eq!(reply.body["errors"][0]["pointer"], "/groups/0");
}

#[tokio::test]
async fn config_views_are_reviewed_and_ordered_by_rev() {
    let server = start("config", 0).await;
    server.configure().await;
    let mut bad = view(8);
    bad["groups"]["npm"]["hosts"] = json!(["api.github.com"]);
    let reply = server.send("PUT", "/control/v1/config", Some(bad)).await;
    assert_problem(&reply, 422, "config_invalid");
    assert_eq!(reply.body["errors"][0]["pointer"], "/groups/npm/hosts/0");
    let status = server.send("GET", "/control/v1/status", None).await;
    assert_eq!(status.body["health"], "degraded");
    assert_eq!(status.body["applied_rev"], 7);
    assert_problem(
        &server
            .send("PUT", "/control/v1/config", Some(view(6)))
            .await,
        409,
        "stale_rev",
    );
    assert_eq!(
        server
            .send("PUT", "/control/v1/config", Some(view(7)))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn svc_tokens_and_call_scope_revoke() {
    let server = start("svc", 0).await;
    server.configure().await;
    let issue = json!({
        "issued_to": { "process": "gantryd", "pid": 4182, "started_at": "2026-10-24T08:00:01.004Z" }
    });
    let first = server
        .send("POST", "/control/v1/svc-tokens", Some(issue.clone()))
        .await;
    assert_eq!(first.status, 201, "{}", first.raw);
    let first_id = first.body["svc_token_id"].as_str().unwrap().to_owned();
    let second = server
        .send("POST", "/control/v1/svc-tokens", Some(issue))
        .await;
    assert_eq!(second.body["revoked_previous"], json!([first_id]));

    let call = "01JBX5D2F4H6K8M0P2R4T6V8XZ";
    let closed = server
        .send(
            "POST",
            &format!("/control/v1/svc-tokens/{first_id}/revoke"),
            Some(json!({"scope": "call", "call_id": call})),
        )
        .await;
    assert_eq!(closed.status, 200, "{}", closed.raw);
    assert_eq!(closed.body["call_id"], call);
    assert_eq!(closed.body["terminal"]["state"], "closed");
    assert_eq!(closed.body["terminal"]["record_count"], 0);
    let statement = server
        .send("GET", &format!("/control/v1/usage?call_id={call}"), None)
        .await;
    assert_eq!(statement.body["items"], json!([]));
    assert_eq!(statement.body["terminal"], closed.body["terminal"]);

    for body in [
        json!({"scope": "call"}),
        json!({"call_id": call}),
        json!({"scope": "token", "call_id": call}),
    ] {
        assert_problem(
            &server
                .send(
                    "POST",
                    &format!("/control/v1/svc-tokens/{first_id}/revoke"),
                    Some(body),
                )
                .await,
            422,
            "validation_failed",
        );
    }
    let whole = server
        .send(
            "POST",
            &format!("/control/v1/svc-tokens/{first_id}/revoke"),
            Some(json!({})),
        )
        .await;
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body["state"], "revoked");
    assert_problem(
        &server
            .send(
                "POST",
                "/control/v1/svc-tokens/svt_01JBX5A2C4E6G8J1L3N5Q7S9UW/revoke",
                Some(json!({"scope": "call", "call_id": call})),
            )
            .await,
        404,
        "not_found",
    );
}

#[tokio::test]
async fn usage_queries() {
    let server = start("usage", 0).await;
    server.configure().await;
    let page = server.send("GET", "/control/v1/usage", None).await;
    assert_eq!(page.body, json!({"items": [], "next_cursor": "0"}));
    for query in [
        "?bogus=1",
        "?limit=x",
        "?after=1&after=2",
        "?call_id=01JBX5D2F4H6K8M0P2R4T6V8XZ&after=1",
    ] {
        assert_problem(
            &server
                .send("GET", &format!("/control/v1/usage{query}"), None)
                .await,
            400,
            "invalid_request",
        );
    }
    assert_problem(
        &server
            .send(
                "GET",
                "/control/v1/usage?svc_token_id=svt_01JBX5A2C4E6G8J1L3N5Q7S9UW",
                None,
            )
            .await,
        404,
        "not_found",
    );
}

#[tokio::test]
async fn secrets_are_write_only() {
    let server = start("secrets", 0).await;
    server.configure().await;
    let present = server
        .send(
            "POST",
            "/control/v1/secrets/present",
            Some(json!({"refs": ["model.anthropic.main", "registry.npm.corp"]})),
        )
        .await;
    assert_eq!(
        present.body,
        json!({"present": {"model.anthropic.main": true, "registry.npm.corp": false}})
    );

    let staged = server
        .send(
            "POST",
            "/control/v1/secrets/stage",
            Some(json!({
                "request_id": "01JBX5C7E9G2J4K6N8Q0S3T5W0",
                "secret_ref": "registry.npm.corp",
                "value": SENTINEL
            })),
        )
        .await;
    assert_eq!(staged.status, 201);
    assert_eq!(staged.body["version"], 1);
    let list = server.send("GET", "/control/v1/secrets/staged", None).await;
    assert_eq!(list.body["items"].as_array().unwrap().len(), 1);
    let id = staged.body["staged_id"].as_str().unwrap();
    let discarded = server
        .send(
            "POST",
            &format!("/control/v1/secrets/staged/{id}/discard"),
            Some(json!({})),
        )
        .await;
    assert_eq!(discarded.body, json!({}));

    let replay_conflict = server
        .send(
            "POST",
            "/control/v1/secrets/stage",
            Some(json!({
                "request_id": "01JBX5C7E9G2J4K6N8Q0S3T5W0",
                "secret_ref": "registry.npm.corp",
                "value": "different"
            })),
        )
        .await;
    assert_problem(&replay_conflict, 409, "conflict");
    let invalid = server
        .send(
            "POST",
            "/control/v1/secrets/stage",
            Some(json!({
                "request_id": "01JBX5C7E9G2J4K6N8Q0S3T5W1",
                "secret_ref": "Bad Ref",
                "value": SENTINEL
            })),
        )
        .await;
    assert_problem(&invalid, 422, "validation_failed");
    assert!(
        !invalid.raw.contains(SENTINEL),
        "errors never echo the value"
    );

    assert_problem(
        &server
            .send("DELETE", "/control/v1/secrets/model.anthropic.main", None)
            .await,
        409,
        "conflict",
    );
    assert_eq!(
        server
            .send("DELETE", "/control/v1/secrets/registry.npm.corp", None)
            .await
            .status,
        200
    );
    // 没有任何读值的路由。
    assert_problem(
        &server
            .send("GET", "/control/v1/secrets/model.anthropic.main", None)
            .await,
        404,
        "not_found",
    );
    for path in [
        "/control/v1/status",
        "/control/v1/secrets/staged",
        "/control/v1/usage",
    ] {
        assert!(!server.send("GET", path, None).await.raw.contains(SENTINEL));
    }
    assert!(!server.audit.lines().concat().contains(SENTINEL));
}
