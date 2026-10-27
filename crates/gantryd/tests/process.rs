//! 进程级测试：起独立的 gantryd 进程，只经控制通道和信号交互。
//!
//! 替身装配的进程由本测试二进制重新执行自身得到（见 `support`）；只测真实装配
//! 启动行为的用例直接跑 `gantryd` 二进制。

#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_gantryd");

/// 替身装配的 gantryd 子进程入口；不带 `CHILD_ENV` 时什么也不做。
#[test]
fn fake_gantryd_child() {
    if let Ok(spec) = std::env::var(support::CHILD_ENV) {
        support::child_main(&spec);
    }
}

/// 起一个替身装配的 gantryd（本测试二进制只跑 `fake_gantryd_child`）。
fn fake_gantryd(scratch: &Scratch, control_uid: u32) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            support::CHILD_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            support::CHILD_ENV,
            format!("{}\n{control_uid}", scratch.config().display()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const MESSAGE: &str = "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD";

/// 每个测试一个临时目录（放配置和套接字），drop 时清理。
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gtd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = format!(
            r#"
[server]
control_socket = "{}"

[server.api]
public_url = "https://gantry.test"
san = ["gantry.test"]

[forge.webhook]
public_url = "https://hooks.gantry.test/github"

[backup]
recipients = ["age1test"]
journal_target = "/srv/gantry-test/revocations.journal"
"#,
            dir.join("control.sock").display()
        );
        std::fs::write(dir.join("gantry.toml"), config).unwrap();
        Self { dir }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("gantry.toml")
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }

    /// 当前进程的 uid（测试不以 root 运行，只能把它放行）。
    fn uid(&self) -> u32 {
        std::fs::metadata(self.config()).unwrap().uid()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    fn spawn(scratch: &Scratch, control_uid: u32) -> Self {
        let child = fake_gantryd(scratch, control_uid).spawn().unwrap();
        let daemon = Self { child };
        wait_for_socket(&scratch.socket());
        daemon
    }

    fn terminate(mut self) -> std::process::ExitStatus {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "gantryd ignored SIGTERM");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while UnixStream::connect(path).is_err() {
        assert!(Instant::now() < deadline, "control socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Client {
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect(scratch: &Scratch) -> Self {
        let stream = UnixStream::connect(scratch.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            reader: BufReader::new(stream),
        }
    }

    fn call(&mut self, request: Value) -> Value {
        let mut line = serde_json::to_vec(&request).unwrap();
        line.push(b'\n');
        self.reader.get_mut().write_all(&line).unwrap();
        self.read()
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("bad response line {line:?}"))
    }

    fn ok(&mut self, request: Value) -> Value {
        let response = self.call(request.clone());
        assert_eq!(response["ok"], true, "{request} -> {response}");
        response["result"].clone()
    }
}

fn run_to_exit(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

#[test]
fn status_reports_profile_and_module_providers() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let status = Client::connect(&scratch).ok(json!({"cmd": "status"}));
    assert_eq!(status["profile"], "fakes");
    assert_eq!(status["head_seq"], 0);
    let provider = |module: &str| {
        status["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["module"] == module)
            .unwrap_or_else(|| panic!("{module} missing from {status}"))["provider"]
            .clone()
    };
    for module in ["engine", "notify", "policy"] {
        assert_eq!(provider(module), "real", "{module}");
    }
    for module in ["store", "forge", "repo", "sandbox", "models", "egress"] {
        assert_eq!(provider(module), "fake", "{module}");
    }
}

#[test]
fn socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mode = std::fs::metadata(scratch.socket())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn other_uid_is_refused_before_any_command() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid() + 1);
    let mut client = Client::connect(&scratch);
    let response = client.read();
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "forbidden");
}

#[test]
fn malformed_and_unknown_requests_get_errors_and_connection_stays_up() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);
    client.reader.get_mut().write_all(b"not json\n").unwrap();
    assert_eq!(client.read()["error"]["code"], "bad_request");
    let response = client.call(json!({"cmd": "pair"}));
    assert_eq!(response["error"]["code"], "unknown_command");
    client.ok(json!({"cmd": "status"}));
}

#[test]
fn oversized_request_closes_the_connection() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);
    let blob = vec![b'x'; 70 * 1024];
    let _ = client.reader.get_mut().write_all(&blob);
    assert_eq!(client.read()["error"]["code"], "too_large");
}

#[test]
fn create_delivery_appends_one_event_and_dedupes_by_message() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);
    let create = json!({
        "cmd": "fake.create_delivery",
        "args": {"project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login", "user_text": "修复登录超时"},
    });
    let first = client.ok(create.clone())["delivery_id"].clone();
    let second = client.ok(create)["delivery_id"].clone();
    assert_eq!(first, second);

    let events = client.ok(json!({"cmd": "fake.events", "args": {"after": 0}}));
    let events = events["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["type"], "delivery.created");
    assert_eq!(events[0]["subject"], first);
    assert_eq!(events[0]["data"]["budget"]["template"], "low.single");
}

#[test]
fn sigterm_shuts_down_and_removes_the_socket() {
    let scratch = Scratch::new();
    let daemon = Daemon::spawn(&scratch, scratch.uid());
    assert!(daemon.terminate().success());
    assert!(!scratch.socket().exists());
}

#[test]
fn stale_socket_is_replaced_but_a_live_one_is_not() {
    let scratch = Scratch::new();
    // 残留：被 SIGKILL 的进程来不及删除套接字文件。不在测试进程里绑定监听：
    // 并行测试 fork 出的子进程会在 exec 前短暂持有该 fd，让"残留"看起来还活着。
    let mut crashed = Daemon::spawn(&scratch, scratch.uid());
    crashed.child.kill().unwrap();
    crashed.child.wait().unwrap();
    assert!(scratch.socket().exists());
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    Client::connect(&scratch).ok(json!({"cmd": "status"}));

    let second = fake_gantryd(&scratch, scratch.uid()).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("another gantryd"));
    Client::connect(&scratch).ok(json!({"cmd": "status"}));
}

/// 两个实例同时启动、路径上有残留套接字：恰好一个赢得锁并服务，另一个拒绝启动；
/// 输的一方不删赢家的套接字。
#[test]
fn concurrent_starts_leave_exactly_one_owner() {
    let scratch = Scratch::new();
    let mut crashed = Daemon::spawn(&scratch, scratch.uid());
    crashed.child.kill().unwrap();
    crashed.child.wait().unwrap();

    let a = fake_gantryd(&scratch, scratch.uid()).spawn().unwrap();
    let b = fake_gantryd(&scratch, scratch.uid()).spawn().unwrap();
    let (mut a, mut b) = (Daemon { child: a }, Daemon { child: b });
    let deadline = Instant::now() + Duration::from_secs(30);
    let loser = loop {
        if let Some(status) = a.child.try_wait().unwrap() {
            break (status, &mut a);
        }
        if let Some(status) = b.child.try_wait().unwrap() {
            break (status, &mut b);
        }
        assert!(Instant::now() < deadline, "neither instance gave up");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!loser.0.success());
    let mut stderr = String::new();
    std::io::Read::read_to_string(loser.1.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("another gantryd"), "{stderr}");
    wait_for_socket(&scratch.socket());
    Client::connect(&scratch).ok(json!({"cmd": "status"}));
    let alive = [a.child.try_wait().unwrap(), b.child.try_wait().unwrap()];
    assert_eq!(alive.iter().filter(|status| status.is_none()).count(), 1);
}

#[test]
fn server_binary_has_no_fakes_flags() {
    for flag in [&["--profile", "fakes"][..], &["--control-uid", "501"]] {
        let output = run_to_exit(flag);
        assert_eq!(output.status.code(), Some(2), "{flag:?}");
    }
}

#[test]
fn server_binary_refuses_to_start_without_a_persistent_store() {
    let scratch = Scratch::new();
    let config = scratch.config();
    let output = run_to_exit(&["--config", config.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("store"));
    assert!(!scratch.socket().exists());
}

#[test]
fn bad_config_fails_before_binding() {
    let scratch = Scratch::new();
    let text = std::fs::read_to_string(scratch.config()).unwrap();
    std::fs::write(
        scratch.config(),
        format!("{text}\n[levels]\ndefault_level = \"low\"\n"),
    )
    .unwrap();
    let config = scratch.config();
    let output = run_to_exit(&["--config", config.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("levels"));
    assert!(!output.status.success());
    assert!(!scratch.socket().exists());
}

/// engine.md §12.2「端到端（替身）」。引擎的 `decide`、执行体与终结管线未实现，
/// 交付单元创建后停在 `spec`；`fake.sandbox_golden` / `fake.forge_check_suite`
/// 两条注入命令随执行体接入一起补。
#[test]
#[ignore = "等待 #70（Low 正常路径 decide）、#71（执行体与事件路由）、#72（AttemptRunner 与终结管线）"]
fn end_to_end_low_delivery_reaches_closed_with_fakes() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);
    client.ok(json!({"cmd": "fake.sandbox_golden", "args": {"node": "single", "open_pr": true}}));
    let delivery = client.ok(json!({
        "cmd": "fake.create_delivery",
        "args": {"project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login", "user_text": "修复登录超时"},
    }))["delivery_id"]
        .clone();

    let stages = |client: &mut Client| -> Vec<(String, String)> {
        let page = client.ok(json!({"cmd": "fake.events", "args": {"after": 0}}));
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| {
                event["type"] == "delivery.stage_changed" && event["subject"] == delivery
            })
            .map(|event| {
                (
                    event["data"]["to"].as_str().unwrap_or_default().to_owned(),
                    event["data"]["reason"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect()
    };
    let wait_for = |client: &mut Client, stage: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !stages(client).iter().any(|(to, _)| to == stage) {
            assert!(
                Instant::now() < deadline,
                "never reached {stage}: {:?}",
                stages(client)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    wait_for(&mut client, "review");
    client.ok(json!({"cmd": "fake.forge_check_suite", "args": {"conclusion": "success"}}));
    wait_for(&mut client, "closed");

    let stages = stages(&mut client);
    let order: Vec<&str> = stages.iter().map(|(to, _)| to.as_str()).collect();
    assert_eq!(order, ["implement", "propose", "review", "merge", "closed"]);
    assert_eq!(stages[0].1, "admission_skipped");
}

// ---------------------------------------------------------------------------
// `gantry` CLI ↔ 运行中的 gantryd（控制通道）
// ---------------------------------------------------------------------------

const CLI: &str = env!("CARGO_BIN_EXE_gantry");

fn gantry(scratch: &Scratch, args: &[&str]) -> Output {
    Command::new(CLI)
        .arg("--config")
        .arg(scratch.config())
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_reads_the_socket_path_from_the_boot_config() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let output = gantry(
        &scratch,
        &["settings", "bootstrap-set", "github", "app_owner=org:acme"],
    );
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result,
        json!({"resource": "github", "applied": ["app_owner"]})
    );
}

#[test]
fn cli_bootstrap_set_is_refused_after_the_bootstrap_period() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    Client::connect(&scratch).ok(json!({"cmd": "fake.end_bootstrap"}));
    let output = gantry(
        &scratch,
        &["settings", "bootstrap-set", "github", "app_owner=x"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not_bootstrap_period"));
}

#[test]
fn cli_reports_a_refused_peer_and_a_missing_daemon() {
    let scratch = Scratch::new();
    let args = ["settings", "bootstrap-set", "github", "app_owner=x"];
    let missing = gantry(&scratch, &args);
    assert_eq!(missing.status.code(), Some(1));

    let _daemon = Daemon::spawn(&scratch, scratch.uid() + 1);
    let refused = gantry(&scratch, &args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("forbidden"),
        "{refused:?}"
    );
}

#[test]
fn cli_usage_errors_exit_2_without_touching_the_daemon() {
    let scratch = Scratch::new();
    for args in [
        &["github", "setup"][..],
        &["pair"],
        &["settings", "bootstrap-set", "x"],
    ] {
        assert_eq!(gantry(&scratch, args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn cli_doctor_host_emits_a_json_report() {
    let scratch = Scratch::new();
    let output = gantry(&scratch, &["doctor", "host", "--json"]);
    assert!(matches!(output.status.code(), Some(0..=2)), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report.is_object(), "{report}");
}

/// deploy/README.md「与 gantry CLI 的接线」：begin → status（pending → completed），
/// 并发 begin 被拒，应答不含任何密钥字段。
#[test]
fn github_setup_commands_follow_the_deploy_contract() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);
    let begin = json!({"cmd": "github.setup.begin", "args": {
        "webhook_url": "https://hooks.gantry.test/webhook",
        "redirect_url": "https://hooks.gantry.test/setup/github/callback",
        "ttl_secs": 900,
    }});
    let begun = client.ok(begin.clone());
    let state = begun["state"].as_str().unwrap().to_owned();
    assert!(state.len() >= 32 && state.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert_eq!(client.call(begin)["error"]["code"], "setup_in_progress");

    let status = json!({"cmd": "github.setup.status", "args": {"state": state}});
    assert_eq!(client.ok(status.clone())["status"], "pending");
    let done = client.ok(status);
    assert_eq!(done["status"], "completed");
    assert_eq!(done["app"]["permissions"]["contents"], "write");
    let text = done.to_string();
    for secret in ["pem", "private_key", "webhook_secret", "client_secret"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let wrong = json!({"cmd": "github.setup.status", "args": {"state": "nope"}});
    assert_eq!(client.ok(wrong)["status"], "expired");
}
