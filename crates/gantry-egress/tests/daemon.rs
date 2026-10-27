//! 进程级测试（起真实的 `gantry-egress` 二进制）：`docs/modules/egress.md` §12.A（重启后令牌与
//! 用量恢复）、§12.F（重启后用持久化的视图继续服务，`applied_rev` 不回退）、§12.B 密钥侧哨兵
//! （数据目录、审计、命令行与环境里没有 key）、§12.I（控制面 socket 权限 0660）。

#![cfg(unix)]

use std::net::TcpListener as StdListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::UnixStream;

const SENTINEL: &str = "sk-gantry-sentinel-daemon-0042";

struct Daemon {
    dir: PathBuf,
    config: PathBuf,
    socket: PathBuf,
    child: Option<Child>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn current_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().uid()
}

impl Daemon {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(format!("/tmp/gd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = dir.join("master.key");
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).unwrap();
        std::fs::write(&key, bytes).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o400)).unwrap();
        let socket = dir.join("egress.sock");
        let config = dir.join("egress.json");
        let body = json!({
            "data_dir": dir.join("data"),
            "control_socket": socket,
            "control_allowed_uid": current_uid(&dir),
            "listen": {
                "sandbox_model": format!("127.0.0.1:{}", free_port()),
                "sandbox_connect": format!("127.0.0.1:{}", free_port()),
                "svc_model": format!("127.0.0.1:{}", free_port())
            },
            "secrets": { "dir": dir.join("secrets"), "master_key": { "file": key } }
        });
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(&config, body.to_string()).unwrap();
        Self {
            dir,
            config,
            socket,
            child: None,
        }
    }

    async fn start(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_gantry-egress"))
            .arg("--config")
            .arg(&self.config)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if UnixStream::connect(&self.socket).await.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "daemon did not come up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// kill -9：不给任何收尾机会。
    fn crash(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "egress")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(
                body.map(|b| b.to_string()).unwrap_or_default(),
            )))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
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

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn the_process_restarts_with_its_state_and_never_stores_the_key_in_clear() {
    let mut daemon = Daemon::new("restart");
    daemon.start().await;

    let mode = std::fs::metadata(&daemon.socket)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o660, "control socket is 0660");

    let (status, body) = daemon.send("GET", "/control/v1/status", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["health"], "unhealthy", "no config yet: fail closed");

    assert_eq!(
        daemon
            .send("PUT", "/control/v1/config", Some(view(7)))
            .await
            .0,
        200
    );
    let (status, staged) = daemon
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
    assert_eq!(status, 201, "{staged}");
    let id = staged["staged_id"].as_str().unwrap();
    let path = format!("/control/v1/secrets/staged/{id}/activate");
    assert_eq!(daemon.send("POST", &path, Some(json!({}))).await.0, 200);
    let (status, grant) = daemon
        .send(
            "POST",
            "/control/v1/jobs",
            Some(json!({
                "job_id": "att_01JBX4Q9ZK3M8T5V2N7RHC6WYD",
                "sandbox_id": "sbx_01JBX4R2M6P8Q1S3T5V7W9Y0ZA",
                "project_id": "prj_01JBX2C7Q5N8D4F6G9H3K1M0PA",
                "ledger": { "subject": "delivery", "id": "dlv_01JBX3A8E2R5T7V9W1Y4Z6B0CD" },
                "tier": "worker",
                "provider": "anthropic",
                "profile": "default",
                "groups": ["model_api", "npm"],
                "tokens_total": 1000,
                "token_weights": { "input": 1.0, "output": 1.0, "cache_read": 1.0, "cache_write": 1.0 },
                "wall_time_s": 1800,
                "config_rev": 7
            })),
        )
        .await;
    assert_eq!(status, 201, "{grant}");
    let token = grant["job_token"].as_str().unwrap().to_owned();

    // /proc 下的环境变量与命令行（Linux）里没有 key。
    #[cfg(target_os = "linux")]
    for file in ["environ", "cmdline"] {
        let bytes = std::fs::read(format!("/proc/{}/{file}", daemon.pid())).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(SENTINEL));
    }
    let _ = daemon.pid();

    daemon.crash();
    daemon.start().await;

    let (_, status) = daemon.send("GET", "/control/v1/status", None).await;
    assert_eq!(status["applied_rev"], 7, "applied_rev does not go back");
    assert_eq!(status["health"], "ok");
    assert_eq!(status["active_jobs"], 1, "tokens survive a restart");
    let (status, usage) = daemon
        .send(
            "GET",
            "/control/v1/jobs/att_01JBX4Q9ZK3M8T5V2N7RHC6WYD/usage",
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(usage["state"], "active");
    let (_, present) = daemon
        .send(
            "POST",
            "/control/v1/secrets/present",
            Some(json!({"refs": ["model.anthropic.main"]})),
        )
        .await;
    assert_eq!(present["present"]["model.anthropic.main"], true);

    // 数据目录（egress.db、审计）里既没有 key 也没有令牌原文；密钥库只有密文。
    let data = daemon.dir.join("data");
    let audit_mode = std::fs::metadata(data.join("audit"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(audit_mode & 0o777, 0o700);
    for file in files_under(&data)
        .into_iter()
        .chain(files_under(&daemon.dir.join("secrets")))
    {
        let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).into_owned();
        assert!(
            !text.contains(SENTINEL),
            "{} contains the key",
            file.display()
        );
        assert!(
            !text.contains(&token[5..]),
            "{} contains the token",
            file.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_bootstrap_configs_refuse_to_start() {
    let daemon = Daemon::new("badcfg");
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&daemon.config).unwrap()).unwrap();
    config["listen"]["svc_model"] = json!("0.0.0.0:18082");
    std::fs::write(&daemon.config, config.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gantry-egress"))
        .arg("--config")
        .arg(&daemon.config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
}
