//! 进程级测试的替身装配。
//!
//! 只在测试里编译（dev-dependency），不进服务端二进制。测试二进制带着
//! `GANTRYD_FAKES_CHILD` 环境变量重新执行自身（只跑 [`CHILD_TEST`] 这一个测试），
//! 这个子进程就是"用替身装配的 gantryd"；父进程只经控制通道和信号跟它交互。

pub mod engine_cfg;
pub mod fake_cmds;

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::GitOid;
use gantry_core::entities::{Actor, ActorKind, Device, DevicePlatform, MergeMethod, Project};
use gantry_core::ids::{Clock, DeviceId, IdGen, ProjectId, Ulid};
use gantry_core::ports::inbox::InboxPort;
use gantry_core::ports::{ActorCtx, Expect, MirrorWant, SecretSink, SecretSource, SecretValue};
use gantry_core::secret::{Consumer, SecretName, SecretString};
use gantry_engine::runtime::{EnginePorts, EngineRuntime};
use gantry_fakes::{
    conversation::FakeConversation, egress::EgressStub, golden::GoldenSandbox,
    models::FakeModelProvider, worker::StubWorkerAdapter,
};
use gantry_forge::{
    Dispatcher, Executor, ExecutorOptions, Forge, ForgeConfig, Permission, ProjectBinding,
    StaticTokenProvider,
};
use gantry_pki::bootstrap::{self, BootstrapParams, MemoryFsProbe};
use gantry_pki::csr::encode_approval_pubkey;
use gantry_pki::directory::DeviceDirectory;
use gantry_pki::journal::RevocationJournal;
use gantry_pki::open::{INTERMEDIATE_KEY_SECRET, SERVER_KEY_SECRET};
use gantry_pki::pairing_service::{NewDevice, PairingConfig};
use gantry_pki::{CertPolicy, P256PublicKey, Pki, PkiConfig};
use gantry_policy::PolicyApprovals;
use gantry_repo::{RepoConfig, RepoDeps};
use gantry_secrets::{MasterKey, MasterKeySource, Secrets, SecretsConfig, StaticMasterKey};
use gantry_store::{Page, Store, StoreConfig, StoreDeps};
use gantryd::assembly::{
    build_forge, build_forge_executor, build_forge_reader, build_notify, build_pki, build_policy,
    build_repo, build_secrets, module_specs, secrets_consumers, Assembly, ForgeDeps,
    ForgeExecutorDeps, ModuleInfo, OsEntropy, Profile, Provider, SystemClock,
};
use gantryd::config::BootConfig;
use gantryd::control::{CommandSet, ControlAuth};
use p256::ecdsa::SigningKey;

/// 子进程要执行的测试名（`--exact` 过滤）。
pub const CHILD_TEST: &str = "fake_gantryd_child";
/// 子进程的配置路径、控制通道放行的 uid 与可选模式，格式 `<config>\n<uid>[\nmanual]`。
pub const CHILD_ENV: &str = "GANTRYD_FAKES_CHILD";
/// 仅测试子进程可用；目录必须是测试临时目录下的 `sqlite`。
pub const CHILD_SQLITE_ENV: &str = "GANTRYD_FAKES_SQLITE_DIR";
/// 父进程持有的外部事实：`<control_base>\n<api_base>\n<git_base>\n<base_oid>\n<bare>`。
pub const CHILD_FORGE_ENV: &str = "GANTRYD_FAKES_FORGE";

/// `fake.*` 命令要直接操纵的替身。
pub struct FakeHandles {
    pub store: Store,
    pub approvals: PolicyApprovals,
    pub auto_approve: bool,
    pub forge: Arc<Forge>,
    /// 真实 forge 执行器：`fake.drive` 用它处理 Outbox。
    pub executor: Executor,
    /// 真实 Dispatcher：`fake.drive` 用它回读 FakeGitHub 并写快照。
    pub dispatcher: Arc<Dispatcher>,
    /// 父进程 FakeGitHub 的控制客户端（子进程经 HTTP 操作）。
    pub github: GitHubControl,
    /// 测试裸远端（`{git_base}/acme/web.git`）；Git 与 REST 夹具只从它的实际 ref 同步。
    pub bare: PathBuf,
    /// 真实构造的测试 PKI；设备经公开 `pairing()` 注册，审批经 `directory()` 实时读取。
    pub pki: Arc<Pki>,
    pub test_device: DeviceId,
}

/// 测试设备的审批签名密钥种子；只存在于测试进程内存。
pub const TEST_APPROVAL_KEY: [u8; 32] = [7; 32];
/// 测试实例的主密钥种子；只在测试临时目录里使用。
pub const TEST_MASTER_KEY: [u8; 32] = [9; 32];
/// 测试设备名，用于重启后识别已登记设备而不是重复配对。
pub const TEST_DEVICE_NAME: &str = "gantryd test device";
/// 测试 CA 材料在 scratch 下的目录名。
pub const TEST_CA_DIR: &str = "pki-ca";

pub fn test_signing_key() -> SigningKey {
    SigningKey::from_slice(&TEST_APPROVAL_KEY).expect("test approval key")
}

/// 测试设备登记的 `approval_pubkey` 指纹（SPKI DER 的 SHA-256）。
pub fn test_signing_fingerprint() -> gantry_core::canon::Sha256Digest {
    let point = test_signing_key().verifying_key().to_encoded_point(false);
    let key = P256PublicKey::from_uncompressed_point(point.as_bytes()).expect("test approval key");
    key.fingerprint()
}

fn test_master_key() -> Box<dyn MasterKeySource> {
    Box::new(StaticMasterKey::new(MasterKey::from_bytes(TEST_MASTER_KEY)))
}

/// 父进程持有的外部事实：FakeGitHub 地址、裸远端与 base 提交。
pub struct ExternalFacts {
    pub control_base: String,
    pub api_base: String,
    pub git_base: PathBuf,
    pub base: GitOid,
    pub bare: PathBuf,
}

/// 测试侧控制服务器：父进程持有 `FakeGitHub`，子进程经回环 HTTP 调用它的公开方法。
///
/// 放在 `tests/support`，不修改 `gantry-fakes` 的业务路由；只接受回环连接，随 drop 停止。
pub struct TestForgeControl {
    addr: std::net::SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestForgeControl {
    pub fn start(github: Arc<gantry_fakes::github::FakeGitHub>) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind forge control");
        listener
            .set_nonblocking(true)
            .expect("nonblocking forge control");
        let addr = listener.local_addr().expect("forge control addr");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_forge_control(stream, &github),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for TestForgeControl {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_forge_control(mut stream: std::net::TcpStream, github: &gantry_fakes::github::FakeGitHub) {
    use std::io::{Read, Write};
    // 监听 socket 是非阻塞的；accept 出的连接在 macOS 上可能继承该标志。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let mut lines = head.lines();
    let Some(request_line) = lines.next() else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while request.len() < body_start + length {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let body: serde_json::Value = request
        .get(body_start..body_start + length)
        .and_then(|body| serde_json::from_slice(body).ok())
        .unwrap_or_default();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let response = match (method, segments.as_slice()) {
        ("POST", ["branch", owner, repo]) => {
            if let (Some(branch), Some(sha)) = (
                body.get("branch").and_then(serde_json::Value::as_str),
                body.get("sha").and_then(serde_json::Value::as_str),
            ) {
                github.set_branch(&format!("{owner}/{repo}"), branch, sha);
                serde_json::json!({"ok": true})
            } else {
                serde_json::json!({"error": "bad request"})
            }
        }
        ("POST", ["check-run", owner, repo, sha]) => {
            github.add_check_run(&format!("{owner}/{repo}"), sha, body);
            serde_json::json!({"ok": true})
        }
        ("POST", ["pull-head", owner, repo, number]) => {
            if let (Some(sha), Ok(number)) = (
                body.get("sha").and_then(serde_json::Value::as_str),
                number.parse::<u64>(),
            ) {
                github.set_pull_head(&format!("{owner}/{repo}"), number, sha);
                serde_json::json!({"ok": true})
            } else {
                serde_json::json!({"error": "bad request"})
            }
        }
        ("GET", ["pull", owner, repo, number]) => match number.parse::<u64>() {
            Ok(number) => github
                .pull(&format!("{owner}/{repo}"), number)
                .unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::json!({"error": "bad request"}),
        },
        ("GET", ["calls"]) => serde_json::Value::Array(
            github
                .recorded_calls()
                .into_iter()
                .map(|call| {
                    serde_json::json!({
                        "method": call.method,
                        "path": call.path,
                        "status": call.status,
                    })
                })
                .collect(),
        ),
        _ => serde_json::json!({"error": "not found"}),
    };
    let body = response.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

/// 父进程持有的 FakeGitHub 的 HTTP 控制客户端（子进程经它操作外部事实）。
pub struct GitHubControl {
    base: String,
}

impl GitHubControl {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> serde_json::Value {
        use std::io::{Read, Write};
        let base = self
            .base
            .strip_prefix("http://")
            .expect("FakeGitHub uses plain HTTP");
        let host_port = base.split('/').next().unwrap_or(base);
        let mut stream = std::net::TcpStream::connect(host_port).expect("connect FakeGitHub");
        let timeout = Some(std::time::Duration::from_secs(5));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .expect("write control request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read control response");
        let (_, body) = response
            .split_once("\r\n\r\n")
            .expect("control response body");
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
    }

    pub fn set_branch(&self, repo: &str, branch: &str, sha: &str) {
        self.request(
            "POST",
            &format!("/branch/{repo}"),
            Some(&serde_json::json!({"branch": branch, "sha": sha})),
        );
    }

    pub fn add_check_run(&self, repo: &str, head_sha: &str, run: serde_json::Value) {
        self.request("POST", &format!("/check-run/{repo}/{head_sha}"), Some(&run));
    }

    pub fn set_pull_head(&self, repo: &str, number: u64, sha: &str) {
        self.request(
            "POST",
            &format!("/pull-head/{repo}/{number}"),
            Some(&serde_json::json!({"sha": sha})),
        );
    }

    pub fn pull(&self, repo: &str, number: u64) -> Option<serde_json::Value> {
        let value = self.request("GET", &format!("/pull/{repo}/{number}"), None);
        (!value.is_null()).then_some(value)
    }

    pub fn recorded_calls(&self) -> Vec<serde_json::Value> {
        match self.request("GET", "/calls", None) {
            serde_json::Value::Array(calls) => calls,
            _ => Vec::new(),
        }
    }
}

/// 在 `dir` 里跑 git；失败时把 stderr 带进 panic，避免测试静默跳过。
fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// 测试补丁：改动 `src/login.rs` 的一行；`git apply` 要求补丁以换行结束。
const TEST_PATCH: &str = "diff --git a/src/login.rs b/src/login.rs\n--- a/src/login.rs\n+++ b/src/login.rs\n@@ -1,3 +1,3 @@\n fn login() {\n-    println!(\"login\");\n+    println!(\"login timeout fixed\");\n }\n";

/// 创建测试裸远端与 base 提交，返回 `(git_base, base_sha, patch)`。
///
/// 子进程可能并发启动并共用同一 scratch：用原子目录创建选出唯一的创建者，
/// 其余进程等待完成标记后读取同一个 base。
pub fn init_bare_remote(test_dir: &Path) -> (PathBuf, GitOid, Vec<u8>) {
    let root = test_dir.join("forge-git");
    let bare = root.join("acme/web.git");
    let seed = root.join("seed");
    let ready = root.join("ready");
    let owner = match std::fs::create_dir(&root) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => panic!("create forge git root: {error}"),
    };
    if owner {
        std::fs::create_dir_all(&bare).expect("bare remote");
        std::fs::create_dir_all(seed.join("src")).expect("seed repo");
        git(&bare, &["init", "--bare", "--quiet"]);
        git(&seed, &["init", "--quiet"]);
        git(&seed, &["config", "user.name", "gantry test"]);
        git(&seed, &["config", "user.email", "gantry@example.invalid"]);
        std::fs::write(
            seed.join("src/login.rs"),
            "fn login() {\n    println!(\"login\");\n}\n",
        )
        .expect("seed file");
        git(&seed, &["add", "src/login.rs"]);
        git(&seed, &["commit", "--quiet", "-m", "seed"]);
        git(
            &seed,
            &[
                "push",
                "--quiet",
                bare.to_str().expect("bare path"),
                "HEAD:refs/heads/main",
            ],
        );
        std::fs::write(&ready, b"ready").expect("ready marker");
    } else {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready.is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the shared bare remote"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let base = GitOid::parse(&git(
        &bare,
        &[
            "--git-dir",
            bare.to_str().expect("bare path"),
            "rev-parse",
            "refs/heads/main",
        ],
    ))
    .expect("base oid");
    (root, base, TEST_PATCH.as_bytes().to_vec())
}

fn test_actor_ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "gantryd-test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(1, 1).expect("fixture"),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

/// 测试专用内存盘探测：`bootstrap` 要求 tmpfs / ramfs，测试目录不是。
struct TestMemoryFs;

impl MemoryFsProbe for TestMemoryFs {
    fn is_memory_fs(&self, _dir: &Path) -> std::io::Result<bool> {
        Ok(true)
    }
}

/// 在测试临时目录里一次性准备好 secrets 实例与 CA 材料（父进程调用，子进程只打开）。
///
/// 这是**测试**的显式初始化；生产的 [`build_secrets`] 只 `open`，不会走到这里。
pub fn provision_test_pki(config: &BootConfig, test_dir: &Path) {
    let secrets_dir = &config.server.secrets_dir;
    if !secrets_dir.exists() {
        Secrets::init(SecretsConfig::new(
            secrets_dir,
            secrets_consumers(),
            test_master_key(),
        ))
        .expect("initialize test secrets");
    }
    let secrets = build_secrets(config, test_master_key()).expect("open test secrets");
    let ca_dir = test_dir.join(TEST_CA_DIR);
    if !ca_dir.join(bootstrap::ROOT_CRT).exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&ca_dir)
            .expect("create test ca dir");
        std::fs::set_permissions(&ca_dir, std::fs::Permissions::from_mode(0o700))
            .expect("test ca dir mode");
        bootstrap::run_with(
            BootstrapParams {
                server_names: config.server.api.san.clone(),
                root_passphrase: SecretString::new("test passphrase".into()),
                install_id: "gantryd-test".into(),
                out_dir: ca_dir.clone(),
                policy: CertPolicy::default(),
            },
            &SystemClock,
            &OsEntropy,
            &TestMemoryFs,
        )
        .expect("bootstrap test ca");
    }
    // deploy 的引导步骤：两把私钥进 Pki 命名空间，再删掉临时明文。
    let scoped = secrets.scoped(Consumer::Pki).expect("pki secrets");
    let ctx = test_actor_ctx();
    for (name, file) in [
        (INTERMEDIATE_KEY_SECRET, bootstrap::INTERMEDIATE_KEY),
        (SERVER_KEY_SECRET, bootstrap::SERVER_KEY),
    ] {
        let name = SecretName::parse(name).expect("secret name");
        if !scoped.present(&name).expect("secret presence") {
            let value = std::fs::read(ca_dir.join(file)).expect("bootstrap key");
            scoped
                .put(&name, SecretValue::from_bytes(value), Expect::Absent, &ctx)
                .expect("store pki key");
        }
        match std::fs::remove_file(ca_dir.join(file)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove plaintext pki key: {error}"),
        }
    }
    for dir in [
        test_dir.join("journal"),
        config
            .backup
            .journal_target
            .parent()
            .expect("journal target parent")
            .to_path_buf(),
    ] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create journal dir");
    }
}

async fn open_test_pki(
    config: &BootConfig,
    test_dir: &Path,
    store: &Store,
    secrets: &Secrets,
    clock: Arc<dyn Clock>,
) -> Pki {
    let scoped: Arc<dyn SecretSink> = Arc::new(secrets.scoped(Consumer::Pki).expect("pki secrets"));
    let pki_config = PkiConfig {
        ca_dir: test_dir.join(TEST_CA_DIR),
        root_key_scan_dirs: vec![
            test_dir.join(TEST_CA_DIR),
            config.server.data_dir.clone(),
            config.server.secrets_dir.clone(),
        ],
        journal: RevocationJournal::new(
            test_dir.join("journal/revocations.journal"),
            config.backup.journal_target.clone(),
        ),
        server_names: config.server.api.san.clone(),
        cert_policy: CertPolicy::default(),
        pairing: PairingConfig::new(config.server.api.public_url.clone()),
    };
    build_pki(pki_config, store, scoped, clock, Arc::new(OsEntropy))
        .await
        .expect("test pki")
}

async fn find_test_device(pki: &Pki) -> Option<Device> {
    let mut cursor = None;
    loop {
        let page = pki
            .directory()
            .list(&Page {
                limit: 200,
                cursor,
                ..Page::default()
            })
            .await
            .expect("list devices");
        if let Some(device) = page
            .items
            .into_iter()
            .find(|device| device.name == TEST_DEVICE_NAME)
        {
            return Some(device);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return None,
        }
    }
}

/// 首次启动经公开配对服务登记测试设备；重启时按名字复用，避免重复配对与第二台设备。
async fn ensure_test_device(pki: &Pki) -> DeviceId {
    if let Some(device) = find_test_device(pki).await {
        assert_eq!(
            device.approval_pubkey_fpr,
            Some(test_signing_fingerprint()),
            "a device with the test name but a different approval key must not be reused"
        );
        return device.id;
    }
    let approval = test_signing_key().verifying_key().to_encoded_point(false);
    let approval =
        P256PublicKey::from_uncompressed_point(approval.as_bytes()).expect("approval key");
    let ticket = pki
        .pairing()
        .open_session(0)
        .await
        .expect("pairing session");
    let client = rcgen::KeyPair::generate().expect("client key");
    let csr = rcgen::CertificateParams::default()
        .serialize_request(&client)
        .expect("csr")
        .pem()
        .expect("csr pem");
    let response = pki
        .pairing()
        .redeem(gantry_pki::pairing_service::PairRequest {
            token: ticket.token.expose_secret().to_owned(),
            csr_pem: csr,
            approval_pubkey: Some(encode_approval_pubkey(&approval)),
            device: NewDevice {
                name: TEST_DEVICE_NAME.into(),
                platform: DevicePlatform::Macos,
            },
        })
        .await
        .expect("pair test device");
    assert_eq!(
        response.device.role,
        gantry_core::entities::DeviceRole::Approver,
        "the first desktop device must be the approver"
    );
    response.device.id
}

pub fn project_id() -> ProjectId {
    engine_cfg::PROJECT.parse().expect("fixture")
}

async fn assemble_variant(
    config: &BootConfig,
    test_dir: &Path,
    sqlite_dir: Option<&Path>,
    auto_approve: bool,
    external: ExternalFacts,
) -> (Assembly, FakeHandles) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let deps = StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    };
    let modules = module_specs();
    let store = match sqlite_dir {
        Some(dir) => Store::open(StoreConfig::new(dir), deps, &modules)
            .await
            .expect("SQLite store"),
        None => Store::open_in_memory_sync_with(deps, &modules).expect("in-memory store"),
    };
    let now = clock.now();
    let project = Project {
        id: project_id(),
        repo: engine_cfg::repo_slug(),
        installation_id: 1,
        bot_user_id: 2,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: now,
        updated_at: now,
    };
    // 真实 GitRepo 需要 `github.commit_identity` 才能生成提交（P-69）；
    // `forge.synced` 的投影按 `project.added` 绑定仓库，两者都在装配前写入。
    {
        let mut tx = store.begin_write().await.expect("settings write");
        let rev = tx.read().config_rev(None);
        tx.append(gantry_core::events::NewEvent {
            ty: gantry_core::events::EventType::SettingsUpdated,
            subject: gantry_core::events::Subject::parse("settings").expect("subject"),
            project_id: None,
            actor: Actor::new(ActorKind::User, "test"),
            causation_id: None,
            correlation_id: None,
            data: serde_json::json!({
                "scope": "global", "resource": "github", "schema_version": 1,
                "base_rev": rev.as_u64(),
                "value": {"commit_identity": {"name": "Gantry Test", "email": "gantry@example.invalid"}},
                "relaxes": [], "secret_changes": []
            }),
            dedup_key: None,
        })
        .await
        .expect("github settings event");
        tx.commit().await.expect("github settings commit");
    }
    {
        let mut tx = store.begin_write().await.expect("project write");
        tx.append(gantry_core::events::NewEvent {
            ty: gantry_core::events::EventType::ProjectAdded,
            subject: gantry_core::events::Subject::parse(&project.id.to_string())
                .expect("project subject"),
            project_id: Some(project.id),
            actor: Actor::new(ActorKind::System, "test"),
            causation_id: None,
            correlation_id: None,
            data: serde_json::to_value(&project).expect("project data"),
            dedup_key: None,
        })
        .await
        .expect("project.added event");
        tx.commit().await.expect("project.added commit");
    }
    let secrets = Arc::new(build_secrets(config, test_master_key()).expect("test secrets"));
    let pki = Arc::new(open_test_pki(config, test_dir, &store, &secrets, clock.clone()).await);
    let test_device = ensure_test_device(&pki).await;
    let ExternalFacts {
        control_base,
        api_base,
        git_base,
        base,
        bare,
    } = external;
    engine_cfg::set_base(base);
    let github = GitHubControl::new(control_base);
    let tokens: Arc<dyn gantry_forge::TokenProvider> = Arc::new(StaticTokenProvider::new(
        SecretString::new("test-only".into()),
    ));
    let forge_stack = build_forge(
        &store,
        clock.clone(),
        ForgeDeps {
            config: ForgeConfig::default().with_project(ProjectBinding {
                project: project.clone(),
                granted: [Permission::ContentsWrite, Permission::PullRequestsWrite]
                    .into_iter()
                    .collect(),
            }),
            tokens: tokens.clone(),
            api_base,
            git_base: git_base.to_string_lossy().into_owned(),
            http_timeout: Duration::from_secs(5),
            git_timeout: Duration::from_secs(5),
            dispatcher_options: gantry_forge::DispatcherOptions {
                http_timeout: Duration::from_secs(5),
                ..gantry_forge::DispatcherOptions::default()
            },
        },
    );
    let gantryd::assembly::PolicyStack {
        port: policy,
        approvals,
    } = build_policy(
        &store,
        forge_stack.reader.clone(),
        clock.clone(),
        ids.clone(),
    );
    let repo = Arc::new(
        build_repo(
            RepoConfig::new(test_dir.join("repo")),
            RepoDeps {
                store: store.clone(),
                clock: clock.clone(),
                ids: ids.clone(),
                policy: policy.clone(),
                cas: Arc::new(store.cas()),
            },
        )
        .await
        .expect("real GitRepo"),
    );
    let reader = build_forge_reader(&forge_stack, repo.clone());
    reader
        .sync_mirror(&project_id(), MirrorWant::All, &test_actor_ctx())
        .await
        .expect("seed mirror from the bare remote");
    let executor = build_forge_executor(
        &forge_stack,
        ForgeExecutorDeps {
            policy: policy.clone(),
            ids: ids.clone(),
            options: ExecutorOptions {
                lease: Duration::from_millis(10),
                http_timeout: Duration::from_secs(5),
                git_timeout: Duration::from_secs(5),
                ..ExecutorOptions::default()
            },
            faults: None,
            repo: repo.clone(),
        },
    )
    .expect("real forge executor");
    let sandbox = Arc::new(
        GoldenSandbox::new(store.cas(), base, clock.clone())
            .with_patch(TEST_PATCH.as_bytes().to_vec()),
    );
    let inbox: Arc<dyn InboxPort> = build_notify(&store, ids.clone());
    let worker = Arc::new(StubWorkerAdapter::new());

    let engine = EngineRuntime::new(
        store.clone(),
        EnginePorts {
            inbox: inbox.clone(),
            outbox: forge_stack.forge.clone(),
            policy: policy.clone(),
            forge: reader.clone(),
            repo: repo.clone(),
            sandbox: sandbox.clone(),
            worker: worker.clone(),
            cas: Arc::new(store.cas()),
            clock: clock.clone(),
            entropy: Arc::new(OsEntropy),
            snapshots: None,
        },
        engine_cfg::runtime_settings(),
    )
    .await
    .expect("engine runtime");

    use Provider::{Absent, Fake, Real};
    let modules = vec![
        if sqlite_dir.is_some() {
            ModuleInfo::new("store", Real, "SQLite store in test directory")
        } else {
            ModuleInfo::new("store", Fake, "in-memory log; SQLite store not wired")
        },
        ModuleInfo::new(
            "engine",
            Real,
            "EngineRuntime; settings via from_settings and ModelsView",
        ),
        ModuleInfo::new("notify", Real, "Notify; production constructor"),
        ModuleInfo::new(
            "policy",
            Real,
            "PolicyEngine; test device, real forge executor",
        ),
        ModuleInfo::new(
            "forge",
            Real,
            "Forge; production constructor against loopback FakeGitHub",
        ),
        ModuleInfo::new(
            "repo",
            Real,
            "GitRepo via build_repo; assemble_real not wired",
        ),
        ModuleInfo::new("sandbox", Fake, "GoldenSandbox; real sandbox needs Podman"),
        ModuleInfo::new("worker", Fake, "StubWorkerAdapter"),
        ModuleInfo::new(
            "models",
            Fake,
            "FakeModelProvider; real models need provider keys",
        ),
        ModuleInfo::new(
            "egress",
            Fake,
            "EgressStub; real egress is a separate process",
        ),
        ModuleInfo::new("conversation", Fake, "FakeConversation"),
        ModuleInfo::new(
            "secrets",
            Real,
            "FileSecrets; M1 Forge/Pki only (Notify joins in M4), never Egress",
        ),
        ModuleInfo::new(
            "pki",
            Real,
            "Pki; test CA, device paired via pairing service, revocation journal in scratch",
        ),
        ModuleInfo::new("api", Absent, "not wired yet"),
    ];

    let assembly = Assembly {
        profile: Profile::Fakes,
        store: store.clone(),
        ids,
        engine,
        inbox,
        policy: policy.clone(),
        outbox: forge_stack.forge.clone(),
        forge: reader,
        repo: repo.clone(),
        locks: repo.clone(),
        sandbox: sandbox.clone(),
        worker,
        models: Arc::new(FakeModelProvider::new()),
        egress: Arc::new(EgressStub::new()),
        conversation: Arc::new(FakeConversation::new()),
        modules,
    };
    let handles = FakeHandles {
        store,
        approvals,
        auto_approve,
        forge: forge_stack.forge.clone(),
        executor,
        dispatcher: forge_stack.dispatcher.clone(),
        github,
        bare,
        pki,
        test_device,
    };
    (assembly, handles)
}

/// 子进程主体：读配置、替身装配、服务到 SIGTERM。启动失败打印到 stderr 并 panic
/// （测试进程以非零码退出），与 gantryd 二进制的失败语义一致。
pub fn child_main(spec: &str) {
    let (config, uid) = spec.split_once('\n').expect("child spec");
    let (uid, auto_approve) = match uid.split_once('\n') {
        Some((uid, "manual")) => (uid, false),
        Some(_) => panic!("invalid child mode"),
        None => (uid, true),
    };
    let config_path = PathBuf::from(config);
    let scratch = config_path
        .parent()
        .expect("config parent")
        .canonicalize()
        .expect("scratch directory");
    let temp = std::env::temp_dir().canonicalize().expect("temp directory");
    assert_eq!(scratch.parent(), Some(temp.as_path()));
    assert_eq!(std::fs::metadata(&scratch).unwrap().mode() & 0o777, 0o700);
    let config = BootConfig::load(&config_path).expect("child config");
    let sqlite_dir = std::env::var_os(CHILD_SQLITE_ENV).map(|dir| {
        let dir = PathBuf::from(dir);
        let metadata = std::fs::symlink_metadata(&dir).expect("SQLite directory");
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        let inherited = std::fs::File::from(
            rustix::io::dup(std::io::stdin()).expect("duplicate parent handle"),
        )
        .metadata()
        .expect("parent SQLite directory handle");
        assert!(inherited.is_dir());
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (inherited.dev(), inherited.ino())
        );
        assert_eq!(
            dir.canonicalize().expect("SQLite directory"),
            scratch.join("sqlite")
        );
        reject_symlinks(&dir);
        dir
    });
    let uid: u32 = uid.parse().expect("child uid");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let external = match std::env::var(CHILD_FORGE_ENV) {
        Ok(spec) => {
            let mut parts = spec.split('\n');
            let control_base = parts.next().expect("control base").to_owned();
            let api_base = parts.next().expect("api base").to_owned();
            let git_base = PathBuf::from(parts.next().expect("git base"));
            let base = GitOid::parse(parts.next().expect("base oid")).expect("base oid");
            let bare = PathBuf::from(parts.next().expect("bare path"));
            ExternalFacts {
                control_base,
                api_base,
                git_base,
                base,
                bare,
            }
        }
        Err(_) => panic!("missing {CHILD_FORGE_ENV}"),
    };
    runtime.block_on(async {
        let (assembly, handles) = assemble_variant(
            &config,
            &scratch,
            sqlite_dir.as_deref(),
            auto_approve,
            external,
        )
        .await;
        let assembly = Arc::new(assembly);
        let commands: Vec<Arc<dyn CommandSet>> = vec![Arc::new(
            fake_cmds::FakeCommands::new(assembly.clone(), handles).await,
        )];
        if let Err(error) = gantryd::serve(&config, assembly, commands, ControlAuth { uid }).await {
            eprintln!("gantryd: {error}");
            panic!("gantryd failed to start");
        }
    });
}

fn reject_symlinks(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read SQLite directory") {
        let entry = entry.expect("SQLite entry");
        let ty = entry.file_type().expect("SQLite entry type");
        assert!(!ty.is_symlink(), "SQLite directory contains a symlink");
        if ty.is_dir() {
            reject_symlinks(&entry.path());
        } else {
            assert!(ty.is_file(), "unexpected SQLite directory entry");
            assert_eq!(
                entry.metadata().expect("SQLite file metadata").nlink(),
                1,
                "SQLite directory contains a hard link"
            );
        }
    }
}
