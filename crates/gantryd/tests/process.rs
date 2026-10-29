//! 进程级测试：起独立的 gantryd 进程，只经控制通道和信号交互。
//!
//! 替身装配的进程由本测试二进制重新执行自身得到（见 `support`）；只测真实装配
//! 启动行为的用例直接跑 `gantryd` 二进制。

#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
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
        .env_remove(support::CHILD_SQLITE_ENV)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const MESSAGE: &str = "msg_01JB7N3R5T8X2V6M9Q4K1W7YZD";

/// 每个测试一个临时目录（放配置和套接字），drop 时清理。
struct Scratch {
    dir: PathBuf,
    owner: Arc<ScratchOwner>,
}

struct ScratchOwner(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = loop {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).unwrap();
            let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("gtd-{name}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create scratch: {error}"),
            }
        };
        let owner = Arc::new(ScratchOwner(dir.clone()));
        let config = format!(
            r#"
[server]
control_socket = "{}"
data_dir = "{}"

[server.api]
public_url = "https://gantry.test"
san = ["gantry.test"]

[forge.webhook]
public_url = "https://hooks.gantry.test/github"

[backup]
recipients = ["age1test"]
journal_target = "/srv/gantry-test/revocations.journal"
"#,
            dir.join("control.sock").display(),
            dir.join("data").display()
        );
        std::fs::write(dir.join("gantry.toml"), config).unwrap();
        Self { dir, owner }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("gantry.toml")
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }

    fn sqlite(&self) -> PathBuf {
        self.dir.join("sqlite")
    }

    /// 当前进程的 uid（测试不以 root 运行，只能把它放行）。
    fn uid(&self) -> u32 {
        std::fs::metadata(self.config()).unwrap().uid()
    }
}

impl Drop for ScratchOwner {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon {
    child: Child,
    owner: Option<Arc<ScratchOwner>>,
}

impl Daemon {
    fn spawn(scratch: &Scratch, control_uid: u32) -> Self {
        Self::start(fake_gantryd(scratch, control_uid), scratch)
    }

    fn spawn_real_policy(scratch: &Scratch) -> Self {
        let mut command = fake_gantryd(scratch, scratch.uid());
        command.env(
            support::CHILD_ENV,
            format!(
                "{}\n{}\nreal-policy",
                scratch.config().display(),
                scratch.uid()
            ),
        );
        Self::start(command, scratch)
    }

    fn spawn_real_executor(scratch: &Scratch) -> Self {
        let mut command = fake_gantryd(scratch, scratch.uid());
        command.env(
            support::CHILD_ENV,
            format!(
                "{}\n{}\nreal-executor",
                scratch.config().display(),
                scratch.uid()
            ),
        );
        Self::start(command, scratch)
    }

    fn spawn_sqlite(scratch: &Scratch) -> Self {
        let mut command = fake_gantryd(scratch, scratch.uid());
        let fd = rustix::fs::open(
            scratch.sqlite(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        command
            .env(support::CHILD_SQLITE_ENV, scratch.sqlite())
            .stdin(Stdio::from(std::fs::File::from(fd)));
        Self::start(command, scratch)
    }

    fn start(mut command: Command, scratch: &Scratch) -> Self {
        let child = command.spawn().unwrap();
        let mut daemon = Self {
            child,
            owner: Some(scratch.owner.clone()),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = daemon.child.try_wait().unwrap() {
                let mut stderr = String::new();
                std::io::Read::read_to_string(daemon.child.stderr.as_mut().unwrap(), &mut stderr)
                    .unwrap();
                panic!("gantryd exited before binding: {status:?}: {stderr}");
            }
            if UnixStream::connect(scratch.socket()).is_ok() {
                if let Some(status) = daemon.child.try_wait().unwrap() {
                    let mut stderr = String::new();
                    std::io::Read::read_to_string(
                        daemon.child.stderr.as_mut().unwrap(),
                        &mut stderr,
                    )
                    .unwrap();
                    panic!("gantryd exited during startup: {status:?}: {stderr}");
                }
                break;
            }
            assert!(Instant::now() < deadline, "control socket never came up");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn crash(mut self) -> std::process::ExitStatus {
        if let Some(status) = self.child.try_wait().unwrap() {
            let mut stderr = String::new();
            std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut stderr)
                .unwrap();
            panic!("gantryd exited before SIGKILL: {status:?}: {stderr}");
        }
        if let Err(error) = self.child.kill() {
            let deadline = Instant::now() + Duration::from_secs(1);
            let status = loop {
                match self.child.try_wait() {
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    result => break result,
                }
            };
            let mut stderr = String::new();
            if matches!(status, Ok(Some(_))) {
                let _ =
                    std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut stderr);
            }
            panic!("failed to send SIGKILL: {error}; status: {status:?}; stderr: {stderr}");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return self.child.wait().unwrap();
            }
            assert!(Instant::now() < deadline, "gantryd ignored SIGKILL");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    // 不确定子进程已停止时保留目录，避免删除其正在使用的数据。
                    if let Some(owner) = self.owner.take() {
                        std::mem::forget(owner);
                    }
                    return;
                }
            }
        }
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
    assert_eq!(status["engine"]["degraded"], json!({}));
    let provider = |module: &str| {
        status["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["module"] == module)
            .unwrap_or_else(|| panic!("{module} missing from {status}"))["provider"]
            .clone()
    };
    for module in ["engine", "notify"] {
        assert_eq!(provider(module), "real", "{module}");
    }
    for module in [
        "store", "policy", "forge", "repo", "sandbox", "models", "egress",
    ] {
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
        "args": {"project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login",
                 "user_text": "修复登录超时"},
    });
    let first = client.ok(create.clone())["delivery_id"].clone();
    let second = client.ok(create)["delivery_id"].clone();
    assert_eq!(first, second);

    let events = client.ok(json!({"cmd": "fake.events", "args": {"after": 0}}));
    // 引擎随即在后台推进这个交付单元，日志里还会有它的后续事件；这里只看建单。
    let events = events["events"].as_array().unwrap();
    let created: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "delivery.created")
        .collect();
    assert_eq!(created.len(), 1, "{events:?}");
    assert_eq!(events[0]["type"], "delivery.created");
    assert_eq!(created[0]["subject"], first);
    assert_eq!(created[0]["data"]["budget"]["template"], "low.single");
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
    let (mut a, mut b) = (
        Daemon {
            child: a,
            owner: Some(scratch.owner.clone()),
        },
        Daemon {
            child: b,
            owner: Some(scratch.owner.clone()),
        },
    );
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
fn server_binary_opens_store_then_refuses_missing_modules() {
    let scratch = Scratch::new();
    let config = scratch.config();
    let output = run_to_exit(&["--config", config.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("persistent SQLite store"),
        "store 不应再是缺失项：{stderr}"
    );
    assert!(stderr.contains("forge"), "下一个缺失模块：{stderr}");
    assert!(!scratch.socket().exists());
    assert!(
        scratch.dir.join("data/gantry.db").exists(),
        "SQLite store 应已打开：{:?}",
        scratch.dir.join("data")
    );
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

/// engine.md §12.2「端到端（替身）」主路径，跑在独立的 gantryd 进程里：
/// `create_delivery(Low)` → 替身沙盒回放 `single` 黄金结果（补丁 + `open_pr` 提议）→
/// 入队 `push_branch` / `create_pr` → 合并审批 → 阶段序列恰为
/// `spec → implement → propose → review → merge → closed`，`spec → implement` 的
/// `reason = admission_skipped`。forge 执行器、GitHub 快照与审批设备由 `fake.drive` 扮演。
#[test]
fn end_to_end_low_delivery_reaches_closed_with_fakes() {
    let scratch = Scratch::new();
    let daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);
    let delivery = client.ok(json!({
        "cmd": "fake.create_delivery",
        "args": {"project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login",
                 "user_text": "修复登录超时"},
    }))["delivery_id"]
        .clone();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let step = client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        if step["done"] == true {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stuck at {}: {:?}",
            step["stage"],
            stages(&mut client, &delivery)
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let stages = stages(&mut client, &delivery);
    let order: Vec<&str> = stages.iter().map(|(to, _)| to.as_str()).collect();
    assert_eq!(order, ["implement", "propose", "review", "merge", "closed"]);
    assert_eq!(stages[0].1, "admission_skipped");
    let status = client.ok(json!({"cmd": "status"}));
    assert_eq!(status["engine"]["degraded"], json!({}), "{status}");

    // 停机：引擎主循环随控制通道一起退出，进程以 0 退出。
    assert!(daemon.terminate().success());
}

/// 默认 `EqualShareReservation` 在预算耗尽（预留为零）时不派发。
///
/// 正向对照证明同一个 `fake.drive` 路径在默认预算下会派发；事件与上报归 #309，
/// 本用例只锁定「不派发」，不把未实现的耗尽事件当作已实现。
#[test]
fn exhausted_reservation_does_not_dispatch() {
    let create = |client: &mut Client, worker_tokens: Option<u64>| {
        let mut args = json!({
            "project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login",
            "user_text": "修复登录超时",
        });
        if let Some(tokens) = worker_tokens {
            args["budget_worker_tokens"] = json!(tokens);
        }
        client.ok(json!({"cmd": "fake.create_delivery", "args": args}))["delivery_id"].clone()
    };
    let has_attempt = |events: &[Value], delivery: &Value| {
        events
            .iter()
            .any(|event| event["type"] == "attempt.started" && event["correlation_id"] == *delivery)
    };

    // 正向对照：默认预算的交付会派发。独立的进程，避免与负向交付共享 outbox / 沙盒。
    {
        let scratch = Scratch::new();
        let daemon = Daemon::spawn(&scratch, scratch.uid());
        let mut client = Client::connect(&scratch);
        let normal = create(&mut client, None);
        let mut dispatched = false;
        for _ in 0..30 {
            let step = client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": normal}}));
            let (_, events) = read_log(&mut client);
            if has_attempt(&events, &normal) {
                dispatched = true;
                break;
            }
            assert_ne!(step["done"], true, "默认预算未派发：{events:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dispatched, "正向对照：默认预算应派发");
        assert!(daemon.terminate().success());
    }

    // 预留为零：同一驱动路径不派发。
    {
        let scratch = Scratch::new();
        let daemon = Daemon::spawn(&scratch, scratch.uid());
        let mut client = Client::connect(&scratch);
        let exhausted = create(&mut client, Some(0));
        for _ in 0..10 {
            let step = client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": exhausted}}));
            assert_eq!(step["done"], false, "{step}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (_, events) = read_log(&mut client);
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "delivery.created" && e["correlation_id"] == exhausted),
            "{events:?}"
        );
        assert!(
            !has_attempt(&events, &exhausted),
            "预留为零时不应派发：{events:?}"
        );
        assert!(daemon.terminate().success());
    }
}

#[test]
fn real_policy_verifies_signed_approval_before_merge_enqueue() {
    real_policy_approval(false);
}

#[test]
fn real_policy_rejects_wrong_signature_without_grant_or_merge() {
    real_policy_approval(true);
}

#[test]
fn real_executor_consumes_real_signed_approval_before_merge() {
    real_executor_merge(false);
}

#[test]
fn real_executor_rejects_changed_live_head_without_merge() {
    real_executor_merge(true);
}

fn real_executor_merge(move_head: bool) {
    let scratch = Scratch::new();
    let daemon = Daemon::spawn_real_executor(&scratch);
    let mut client = Client::connect(&scratch);
    let status = client.ok(json!({"cmd":"status"}));
    assert!(status["modules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["module"] == "forge"
            && m["provider"] == "fake"
            && m["note"]
                .as_str()
                .is_some_and(|d| d.contains("real merge executor"))));
    let delivery = client.ok(json!({"cmd": "fake.create_delivery", "args": {
        "project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login", "user_text": "修复登录超时"
    }}))["delivery_id"].clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    let requested = loop {
        client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        let (_, events) = read_log(&mut client);
        if let Some(event) = events
            .iter()
            .find(|e| e["type"] == "approval.requested" && e["correlation_id"] == delivery)
        {
            break event.clone();
        }
        assert!(Instant::now() < deadline, "no approval: {events:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let approval = requested["subject"].clone();
    if !move_head {
        let invalid =
            client.call(json!({"cmd":"fake.real_grant", "args":{"id":approval, "invalid":true}}));
        assert_eq!(invalid["ok"], false);
        assert_eq!(invalid["error"]["message"], "signature invalid");
        // 非引擎构造的额外动作仅用于验证执行器拒绝尚未授予的真实审批。
        let probe = client.ok(json!({"cmd":"fake.enqueue_merge_probe", "args":{"id":approval}}))
            ["id"]
            .clone();
        let result = client.ok(json!({"cmd":"fake.real_execute"}));
        assert!(
            result["result"].as_str().unwrap().contains("Cancelled"),
            "{result}"
        );
        assert_eq!(result["merge_calls"], 0);
        assert_eq!(
            client.ok(json!({"cmd":"fake.outbox_action", "args":{"id":probe}}))["status"],
            "cancelled"
        );
        assert_eq!(
            client.ok(json!({"cmd":"fake.real_approval", "args":{"id":approval}}))["status"],
            "pending"
        );
    }
    assert_eq!(
        client.ok(json!({"cmd":"fake.real_grant", "args":{"id":approval}}))["status"],
        "granted"
    );
    let action = loop {
        client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        let (_, events) = read_log(&mut client);
        if let Some(event) = events.iter().find(|e| {
            e["type"] == "outbox.enqueued"
                && e["correlation_id"] == delivery
                && e["data"]["action"]["kind"] == "merge_pr"
        }) {
            assert_eq!(event["data"]["action"]["requires_approval"], approval);
            break event["subject"].clone();
        }
        assert!(Instant::now() < deadline, "no merge: {events:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let first = client.ok(json!({"cmd":"fake.real_execute", "args":{"move_head":move_head}}));
    assert_eq!(first["merge_calls"], 0, "{first}");
    let state = client.ok(json!({"cmd":"fake.real_approval", "args":{"id":approval}}));
    let (_, events) = read_log(&mut client);
    let consumed: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "approval.consumed" && e["subject"] == approval)
        .collect();
    if move_head {
        assert!(
            first["result"]
                .as_str()
                .unwrap()
                .contains("precondition_failed"),
            "{first}"
        );
        assert_eq!(state["status"], "granted");
        assert!(consumed.is_empty());
    } else {
        assert!(
            first["result"]
                .as_str()
                .unwrap()
                .contains("Crashed(AfterConsume)"),
            "{first}"
        );
        assert_eq!(state["status"], "consumed");
        assert_eq!(state["consumed_by"], action);
        assert_eq!(consumed.len(), 1);
        assert_eq!(consumed[0]["data"]["consumed_by"], action);
        std::thread::sleep(Duration::from_millis(30));
        let second = client.ok(json!({"cmd":"fake.real_execute"}));
        assert!(
            second["result"].as_str().unwrap().contains("Succeeded"),
            "{second}"
        );
        assert_eq!(second["merge_calls"], 1);
        assert_eq!(second["pull"]["merged"], true);
        let third = client.ok(json!({"cmd":"fake.real_execute"}));
        assert_eq!(third["merge_calls"], 1);
        let (_, events) = read_log(&mut client);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "approval.consumed" && e["subject"] == approval)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "outbox.succeeded" && e["subject"] == action)
                .count(),
            1
        );
        let probe = client
            .ok(json!({"cmd":"fake.enqueue_merge_probe", "args":{"id":approval, "second":true}}))
            ["id"]
            .clone();
        let rejected = client.ok(json!({"cmd":"fake.real_execute"}));
        assert!(
            rejected["result"].as_str().unwrap().contains("Cancelled"),
            "{rejected}"
        );
        assert_eq!(rejected["merge_calls"], 1);
        assert_eq!(
            client.ok(json!({"cmd":"fake.outbox_action", "args":{"id":probe}}))["status"],
            "cancelled"
        );
    }
    assert!(daemon.terminate().success());
}

#[tokio::test]
async fn fake_forge_snapshot_update_waits_for_store_transaction() {
    use gantry_core::canon::GitOid;
    use gantry_core::ports::ForgeReader;

    let (_assembly, handles) = support::assemble_fakes(None, true).await;
    let store = handles.store.clone();
    let forge = handles.forge.clone();
    let transaction = store.begin_write().await.unwrap();
    let head = GitOid::parse(support::engine_cfg::BASE).unwrap();
    let pr = support::engine_cfg::snapshot("open", head);
    let (started, entered) = tokio::sync::oneshot::channel();
    let update_store = store.clone();
    let update_forge = forge.clone();
    let mut update = tokio::spawn(async move {
        started.send(()).unwrap();
        support::fake_cmds::upsert_pr(&update_store, &update_forge, pr).await
    });
    entered.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut update)
            .await
            .is_err(),
        "snapshot update bypassed the write lease"
    );
    assert!(forge
        .pull_request(&support::project_id(), support::engine_cfg::PR_NUMBER)
        .await
        .unwrap()
        .is_none());
    // 同一写门保证已有事务在直接写更新版本前成功提交。
    transaction
        .commit()
        .await
        .expect("transaction must not conflict with snapshot update");
    tokio::time::timeout(Duration::from_secs(5), update)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        forge
            .pull_request(&support::project_id(), support::engine_cfg::PR_NUMBER)
            .await
            .unwrap()
            .unwrap()
            .head_sha,
        head
    );
}

fn real_policy_approval(invalid: bool) {
    let scratch = Scratch::new();
    let daemon = Daemon::spawn_real_policy(&scratch);
    let mut client = Client::connect(&scratch);
    let status = client.ok(json!({"cmd": "status"}));
    assert_eq!(status["profile"], "fakes");
    for (name, expected) in [
        ("policy", "real"),
        ("engine", "real"),
        ("notify", "real"),
        ("store", "fake"),
        ("forge", "fake"),
        ("repo", "fake"),
        ("sandbox", "fake"),
        ("worker", "fake"),
        ("models", "fake"),
        ("egress", "fake"),
        ("conversation", "fake"),
        ("api", "absent"),
        ("pki", "absent"),
    ] {
        assert!(
            status["modules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["module"] == name && m["provider"] == expected),
            "{status}"
        );
    }
    let delivery = client.ok(json!({"cmd": "fake.create_delivery", "args": {
        "project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login", "user_text": "修复登录超时"
    }}))["delivery_id"].clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    let requested = loop {
        let step = client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        let (_, events) = read_log(&mut client);
        if let Some(event) = events
            .iter()
            .find(|e| e["type"] == "approval.requested" && e["correlation_id"] == delivery)
        {
            break event.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no approval at {}: {events:?}",
            step["stage"]
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(requested["data"]["action"], "merge_pr");
    assert_eq!(
        requested["data"]["target"],
        json!({"kind": "pr", "repo": "acme/web", "number": 12})
    );
    let approval = &requested["subject"];
    let response = client
        .call(json!({"cmd": "fake.real_grant", "args": {"id": approval, "invalid": invalid}}));
    if invalid {
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(response["error"]["message"], "signature invalid");
    } else {
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["result"]["status"], "granted");
    }
    let deadline = Instant::now() + Duration::from_secs(if invalid { 1 } else { 30 });
    loop {
        client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        let (_, events) = read_log(&mut client);
        let granted = events
            .iter()
            .find(|e| e["type"] == "approval.granted" && e["subject"] == *approval);
        let merge = events.iter().find(|e| {
            e["type"] == "outbox.enqueued"
                && e["correlation_id"] == delivery
                && e["data"]["action"]["kind"] == "merge_pr"
        });
        if invalid {
            assert!(granted.is_none() && merge.is_none(), "{events:?}");
            if Instant::now() >= deadline {
                break;
            }
        } else if let (Some(granted), Some(merge)) = (granted, merge) {
            assert!(requested["seq"].as_u64() < granted["seq"].as_u64());
            assert!(granted["seq"].as_u64() < merge["seq"].as_u64());
            assert_eq!(merge["data"]["action"]["requires_approval"], *approval);
            assert_eq!(
                merge["data"]["action"]["payload"]["expected_head_sha"],
                requested["data"]["subject_digest"]
            );
            break;
        } else {
            assert!(Instant::now() < deadline, "no merge: {events:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        client.ok(json!({"cmd": "status"}))["engine"]["degraded"],
        json!({})
    );
    assert!(daemon.terminate().success());
}

#[test]
fn sqlite_child_rejects_a_directory_outside_its_scratch() {
    let scratch = Scratch::new();
    let other = Scratch::new();
    std::fs::create_dir(other.sqlite()).unwrap();
    std::os::unix::fs::symlink(other.sqlite(), scratch.sqlite()).unwrap();

    for dir in [other.sqlite(), scratch.sqlite()] {
        let mut command = fake_gantryd(&scratch, scratch.uid());
        command
            .env(support::CHILD_SQLITE_ENV, &dir)
            .stdin(Stdio::from(std::fs::File::open(&dir).unwrap()));
        let mut daemon = Daemon {
            child: command.spawn().unwrap(),
            owner: Some(scratch.owner.clone()),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = daemon.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "invalid SQLite path started gantryd"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!status.success());
        let mut stderr = String::new();
        std::io::Read::read_to_string(daemon.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert!(stderr.contains("assertion"), "{stderr}");
        assert!(!other.sqlite().join("gantry.db").exists());
        assert!(!scratch.socket().exists());
    }
}

#[test]
fn sqlite_child_rejects_a_mismatched_parent_handle() {
    let scratch = Scratch::new();
    let other = Scratch::new();
    std::fs::create_dir(scratch.sqlite()).unwrap();
    std::fs::create_dir(other.sqlite()).unwrap();
    let mut command = fake_gantryd(&scratch, scratch.uid());
    command
        .env(support::CHILD_SQLITE_ENV, scratch.sqlite())
        .stdin(Stdio::from(std::fs::File::open(other.sqlite()).unwrap()));
    let mut daemon = Daemon {
        child: command.spawn().unwrap(),
        owner: Some(scratch.owner.clone()),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "mismatched handle started gantryd"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    assert!(!scratch.sqlite().join("gantry.db").exists());
    assert!(!other.sqlite().join("gantry.db").exists());
}

#[test]
fn sqlite_child_rejects_symlinks_in_the_store() {
    for name in [
        "gantry.db",
        "gantry.db-wal",
        "gantry.db-shm",
        "gantry.lock",
        "cas",
        "cas/nested",
    ] {
        let scratch = Scratch::new();
        let other = Scratch::new();
        std::fs::create_dir(scratch.sqlite()).unwrap();
        if name.starts_with("cas/") {
            std::fs::create_dir(scratch.sqlite().join("cas")).unwrap();
        }
        let target = other.dir.join("sentinel");
        std::fs::write(&target, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&target, scratch.sqlite().join(name)).unwrap();
        let mut command = fake_gantryd(&scratch, scratch.uid());
        command
            .env(support::CHILD_SQLITE_ENV, scratch.sqlite())
            .stdin(Stdio::from(std::fs::File::open(scratch.sqlite()).unwrap()));
        let mut daemon = Daemon {
            child: command.spawn().unwrap(),
            owner: Some(scratch.owner.clone()),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = daemon.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "symlink {name} started gantryd");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!status.success(), "{name}");
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged", "{name}");
        assert!(!scratch.socket().exists());
    }
}

#[test]
fn sqlite_child_rejects_hard_links_in_the_store() {
    for name in [
        "gantry.db",
        "gantry.db-wal",
        "gantry.db-shm",
        "gantry.lock",
        "cas/nested",
    ] {
        let scratch = Scratch::new();
        let other = Scratch::new();
        std::fs::create_dir(scratch.sqlite()).unwrap();
        if name.starts_with("cas/") {
            std::fs::create_dir(scratch.sqlite().join("cas")).unwrap();
        }
        let target = other.dir.join("sentinel");
        std::fs::write(&target, b"unchanged").unwrap();
        std::fs::hard_link(&target, scratch.sqlite().join(name)).unwrap();
        let mut command = fake_gantryd(&scratch, scratch.uid());
        command
            .env(support::CHILD_SQLITE_ENV, scratch.sqlite())
            .stdin(Stdio::from(std::fs::File::open(scratch.sqlite()).unwrap()));
        let mut daemon = Daemon {
            child: command.spawn().unwrap(),
            owner: Some(scratch.owner.clone()),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = daemon.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "hard link {name} started gantryd"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!status.success(), "{name}");
        let mut stderr = String::new();
        std::io::Read::read_to_string(daemon.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        assert!(
            stderr.contains("SQLite directory contains a hard link"),
            "{name}: {stderr}"
        );
        assert!(!scratch.socket().exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged", "{name}");
        drop(daemon);
        drop(scratch);
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged", "{name}");
        assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 1, "{name}");
    }
}

/// 在工作树清理和游标追平后杀死测试进程，以同一 SQLite 目录重启。
#[test]
fn sqlite_store_survives_process_crash_and_restart() {
    let scratch = Scratch::new();
    std::fs::create_dir(scratch.sqlite()).unwrap();
    let daemon = Daemon::spawn_sqlite(&scratch);
    let mut client = Client::connect(&scratch);

    let status = client.ok(json!({"cmd": "status"}));
    assert_eq!(status["profile"], "fakes");
    let modules = status["modules"].as_array().unwrap();
    for (name, provider) in [
        ("store", "real"),
        ("engine", "real"),
        ("notify", "real"),
        ("forge", "fake"),
        ("repo", "fake"),
        ("policy", "fake"),
        ("sandbox", "fake"),
    ] {
        let module = modules
            .iter()
            .find(|module| module["module"] == name)
            .unwrap();
        assert_eq!(module["provider"], provider, "{name}");
    }
    assert_eq!(status["engine"]["degraded"], json!({}));
    let runtime = client.ok(json!({"cmd": "fake.runtime_status"}));
    assert_eq!(runtime["startup_cursor"], 0);

    let create = json!({
        "cmd": "fake.create_delivery",
        "args": {"project_id": PROJECT, "source_message_id": MESSAGE, "title": "fix login",
                 "user_text": "修复登录超时"},
    });
    let delivery = client.ok(create.clone())["delivery_id"].clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let step = client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        if step["done"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "stuck at {}", step["stage"]);
        std::thread::sleep(Duration::from_millis(20));
    }

    let (before_head, before, before_cursor) = caught_up_log(&mut client);
    let created: Vec<_> = before
        .iter()
        .filter(|event| event["type"] == "delivery.created")
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["subject"], delivery);
    assert_eq!(created[0]["data"]["source_message_id"], MESSAGE);
    let transitions: Vec<_> = before
        .iter()
        .filter(|event| event["type"] == "delivery.stage_changed" && event["subject"] == delivery)
        .collect();
    let order: Vec<_> = transitions
        .iter()
        .map(|event| {
            (
                event["data"]["from"].as_str().unwrap(),
                event["data"]["to"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("spec", "implement"),
            ("implement", "propose"),
            ("propose", "review"),
            ("review", "merge"),
            ("merge", "closed"),
        ]
    );
    assert_eq!(transitions[0]["data"]["reason"], "admission_skipped");
    assert_eq!(
        before
            .iter()
            .filter(
                |event| event["type"] == "worktree.removed" && event["correlation_id"] == delivery
            )
            .count(),
        1
    );

    // 由入队事件定位动作：succeeded 的信封没有 delivery correlation_id。
    let actions: Vec<_> = before
        .iter()
        .filter(|event| event["type"] == "outbox.enqueued" && event["correlation_id"] == delivery)
        .map(|event| {
            (
                event["subject"].clone(),
                event["data"]["action"]["kind"].clone(),
            )
        })
        .collect();
    for kind in ["push_branch", "create_pr", "merge_pr"] {
        assert!(
            actions.iter().any(|(_, action_kind)| action_kind == kind),
            "{actions:?}"
        );
    }
    for (id, _) in &actions {
        for ty in ["outbox.executing", "outbox.succeeded"] {
            assert_eq!(
                before
                    .iter()
                    .filter(|event| event["type"] == ty && event["subject"] == *id)
                    .count(),
                1,
                "{id}: {ty}"
            );
        }
    }
    let rows: Vec<_> = actions
        .iter()
        .map(|(id, kind)| {
            let row = client.ok(json!({"cmd": "fake.outbox_action", "args": {"id": id}}));
            assert_eq!(row["id"], *id);
            assert_eq!(row["kind"], *kind);
            assert_eq!(row["delivery_id"], delivery);
            assert_eq!(row["status"], "succeeded");
            assert!(row["idempotency_key"]
                .as_str()
                .is_some_and(|key| !key.is_empty()));
            let enqueued = before
                .iter()
                .find(|event| event["type"] == "outbox.enqueued" && event["subject"] == *id)
                .unwrap();
            assert_eq!(
                row["idempotency_key"],
                enqueued["data"]["action"]["idempotency_key"]
            );
            row
        })
        .collect();
    assert!(before_cursor >= before_head && before_head > 0);

    drop(client);
    let status = daemon.crash();
    assert_eq!(status.signal(), Some(9), "expected SIGKILL, got {status:?}");
    let db = std::fs::read(scratch.sqlite().join("gantry.db")).unwrap();
    assert!(db.starts_with(b"SQLite format 3\0"));

    let daemon = Daemon::spawn_sqlite(&scratch);
    let mut client = Client::connect(&scratch);
    let status = client.ok(json!({"cmd": "status"}));
    assert_eq!(status["profile"], "fakes");
    assert!(status["modules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|module| module["module"] == "store" && module["provider"] == "real"));
    assert_eq!(status["engine"]["degraded"], json!({}));
    let runtime = client.ok(json!({"cmd": "fake.runtime_status"}));
    assert!(runtime["startup_cursor"].as_u64().unwrap() >= before_cursor);
    assert!(runtime["cursor"].as_u64().unwrap() >= before_cursor);
    assert_eq!(runtime["degraded"], json!({}));

    let (_, reopened, _) = caught_up_log(&mut client);
    assert!(reopened.len() >= before.len());
    assert_eq!(&reopened[..before.len()], before.as_slice());
    assert_eq!(client.ok(create)["delivery_id"], delivery);
    for _ in 0..3 {
        let step = client.ok(json!({"cmd": "fake.drive", "args": {"delivery_id": delivery}}));
        assert_eq!(step["stage"], "closed");
        assert_eq!(step["done"], true);
    }
    let (_, after, after_cursor) = caught_up_log(&mut client);
    assert!(after_cursor >= before_cursor);
    assert_eq!(after, reopened, "retry or fake executor appended events");
    assert_eq!(
        after
            .iter()
            .filter(|event| event["type"] == "delivery.created")
            .count(),
        1
    );
    let after_transitions: Vec<_> = after
        .iter()
        .filter(|event| event["type"] == "delivery.stage_changed" && event["subject"] == delivery)
        .collect();
    assert_eq!(after_transitions, transitions);
    assert_eq!(
        after
            .iter()
            .filter(
                |event| event["type"] == "worktree.removed" && event["correlation_id"] == delivery
            )
            .count(),
        1
    );
    let after_actions: Vec<_> = after
        .iter()
        .filter(|event| event["type"] == "outbox.enqueued" && event["correlation_id"] == delivery)
        .map(|event| {
            (
                event["subject"].clone(),
                event["data"]["action"]["kind"].clone(),
            )
        })
        .collect();
    assert_eq!(after_actions, actions);
    for ((id, _), row) in actions.iter().zip(&rows) {
        assert_eq!(
            client.ok(json!({"cmd": "fake.outbox_action", "args": {"id": id}})),
            *row
        );
    }
    for (id, _) in &actions {
        for ty in ["outbox.executing", "outbox.succeeded"] {
            assert_eq!(
                after
                    .iter()
                    .filter(|event| event["type"] == ty && event["subject"] == *id)
                    .count(),
                1,
                "{id}: {ty}"
            );
        }
    }
    assert_eq!(
        client.ok(json!({"cmd": "status"}))["engine"]["degraded"],
        json!({})
    );
    let old_head = after.last().unwrap()["seq"].as_u64().unwrap();
    assert!(old_head >= before_head);
    let new_delivery = client.ok(json!({
        "cmd": "fake.create_delivery",
        "args": {"project_id": PROJECT, "source_message_id": "msg_01JB7N3R5T8X2V6M9Q4K1W7YZE", "title": "another delivery", "user_text": "独立请求"}
    }))["delivery_id"].clone();
    assert_ne!(new_delivery, delivery);
    let (_, extended) = read_log(&mut client);
    assert_eq!(&extended[..after.len()], after.as_slice());
    let created = extended
        .iter()
        .find(|event| event["type"] == "delivery.created" && event["subject"] == new_delivery)
        .unwrap();
    assert_eq!(created["seq"], old_head + 1);
    assert!(daemon.terminate().success());
}

/// 分页读取一次日志前缀，再等持久游标追上该前缀。
fn caught_up_log(client: &mut Client) -> (u64, Vec<Value>, u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (head, events) = read_log(client);
        let runtime = client.ok(json!({"cmd": "fake.runtime_status"}));
        assert_eq!(runtime["degraded"], json!({}), "{runtime}");
        let cursor = runtime["cursor"].as_u64().unwrap();
        if cursor >= head {
            return (head, events, cursor);
        }
        assert!(
            Instant::now() < deadline,
            "engine cursor {cursor} behind {head}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_log(client: &mut Client) -> (u64, Vec<Value>) {
    let mut after = 0;
    let mut head = None;
    let mut events = Vec::new();
    loop {
        let page = client.ok(json!({"cmd": "fake.events", "args": {"after": after}}));
        let target = *head.get_or_insert(page["head"].as_u64().unwrap());
        let page_events = page["events"].as_array().unwrap();
        assert!(
            after >= target || !page_events.is_empty(),
            "incomplete event replay at {after}/{target}"
        );
        for event in page_events {
            let seq = event["seq"].as_u64().unwrap();
            assert_eq!(seq, after + 1, "non-contiguous event log");
            after = seq;
            if seq <= target {
                events.push(event.clone());
            }
        }
        if after >= target {
            return (target, events);
        }
    }
}

/// 交付单元的阶段迁移 `(to, reason)`，按日志顺序。
fn stages(client: &mut Client, delivery: &Value) -> Vec<(String, String)> {
    let page = client.ok(json!({"cmd": "fake.events", "args": {"after": 0}}));
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "delivery.stage_changed" && event["subject"] == *delivery)
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
}

// ---------------------------------------------------------------------------
// `gantry` CLI ↔ 运行中的 gantryd（控制通道）
// ---------------------------------------------------------------------------

const CLI: &str = env!("CARGO_BIN_EXE_gantry");

fn gantry(scratch: &Scratch, args: &[&str]) -> Output {
    Command::new(CLI)
        .arg("--config")
        .arg(scratch.config())
        .env("GANTRY_CONTROL_UID", scratch.uid().to_string())
        .args(args)
        .output()
        .unwrap()
}

#[cfg(feature = "testing")]
#[test]
fn cli_reads_the_socket_path_from_the_boot_config() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let output = gantry(
        &scratch,
        &[
            "settings",
            "bootstrap-set",
            "sandbox",
            "base_image=registry.example/x@sha256:00",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result,
        json!({"resource": "sandbox", "applied": ["base_image"]})
    );
}

#[cfg(feature = "testing")]
#[test]
fn bootstrap_set_validates_whitelist_and_types() {
    let scratch = Scratch::new();
    let _daemon = Daemon::spawn(&scratch, scratch.uid());
    let mut client = Client::connect(&scratch);

    let ok = client.ok(json!({
        "cmd": "settings.bootstrap_set",
        "args": {"resource": "sandbox", "values": {"base_image": "registry.example/x@sha256:00"}},
    }));
    assert_eq!(ok["applied"], json!(["base_image"]));

    for (args, code) in [
        (
            json!({"resource": "sandbox", "values": {"base_image": 1}}),
            "bad_request",
        ),
        (
            json!({"resource": "sandbox", "values": {"bad key": "x"}}),
            "bad_request",
        ),
        (
            json!({"resource": "nope", "values": {"base_image": "x"}}),
            "bad_request",
        ),
        (json!({"resource": "sandbox", "values": {}}), "bad_request"),
        (
            json!({"resource": "github", "values": {"app_owner": "x"}}),
            "not_writable",
        ),
        (
            json!({"resource": "github", "values": {"app.app_id": "123"}}),
            "not_writable",
        ),
        (
            json!({"resource": "github", "values": {"commit_identity": "x"}}),
            "not_writable",
        ),
    ] {
        let response = client.call(json!({"cmd": "settings.bootstrap_set", "args": args}));
        assert_eq!(response["ok"], false, "{args} -> {response}");
        assert_eq!(response["error"]["code"], code, "{args} -> {response}");
    }
}

#[cfg(feature = "testing")]
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

#[cfg(feature = "testing")]
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
