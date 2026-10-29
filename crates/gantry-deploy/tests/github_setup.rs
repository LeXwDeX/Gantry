//! deploy.md §12「GitHub App 创建 (M1)」中 deploy 侧的条目（A 级，替身控制通道与探测）。
//! 回调本身（`state` 不符 / 重放 → 404、落盘、审计事件）归 `forge`（forge.md §12）。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use gantry_deploy::control::{ControlClient, ControlError};
use gantry_deploy::github_setup::{self, Env, SetupArgs, SetupError};
use gantry_deploy::manifest::{self, Entry, Stage};
use gantry_deploy::probe::{HttpProbe, Method};
use serde_json::{json, Value};

const STATE: &str = "Zx9_k2mQ-4vB7nR1tY6wE3aS";
const HOST: &str = "hooks.example.net";
const SENTINEL_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----SENTINEL";

/// 入口探测替身：按 (方法, 路径) 返回状态码。缺省是一个就绪的入口。
struct FakeEntry(HashMap<(Method, String), Result<u16, String>>);

impl FakeEntry {
    fn ready() -> Self {
        let mut m = HashMap::new();
        m.insert((Method::Post, "/webhook".into()), Ok(401));
        m.insert((Method::Get, "/setup/github/callback".into()), Ok(404));
        m.insert((Method::Get, "/v1/me".into()), Ok(404));
        m.insert((Method::Post, "/v1/pair".into()), Ok(404));
        Self(m)
    }

    fn with(mut self, method: Method, path: &str, result: Result<u16, String>) -> Self {
        self.0.insert((method, path.into()), result);
        self
    }
}

impl HttpProbe for FakeEntry {
    fn status(&self, method: Method, url: &str) -> Result<u16, String> {
        let path = url
            .strip_prefix(&format!("https://{HOST}"))
            .expect("只探测公网入口");
        self.0
            .get(&(method, path.to_owned()))
            .cloned()
            .unwrap_or(Err("未配置".into()))
    }
}

/// 控制通道替身：记录调用，按脚本应答。
struct FakeControl {
    begin: Result<Value, ControlError>,
    statuses: RefCell<VecDeque<Value>>,
    calls: RefCell<Vec<(String, Value)>>,
}

impl FakeControl {
    fn new(statuses: Vec<Value>) -> Self {
        Self {
            begin: Ok(json!({ "state": STATE, "expires_at": "2026-10-27T12:15:00Z" })),
            statuses: RefCell::new(statuses.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn cmds(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|(c, _)| c.clone()).collect()
    }
}

impl ControlClient for FakeControl {
    fn call(&self, cmd: &str, args: Value) -> Result<Value, ControlError> {
        self.calls.borrow_mut().push((cmd.to_owned(), args));
        match cmd {
            "github.setup.begin" => self.begin.clone(),
            "github.setup.status" => Ok(self
                .statuses
                .borrow_mut()
                .pop_front()
                .unwrap_or(json!({ "status": "pending" }))),
            other => panic!("未预期的命令 {other}"),
        }
    }
}

fn granted_app() -> Value {
    let permissions: serde_json::Map<String, Value> = Stage::M1
        .permissions()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), json!(v)))
        .collect();
    json!({
        "app_id": 424242,
        "slug": "gantry-hooks",
        "html_url": "https://github.com/settings/apps/gantry-hooks",
        "permissions": permissions,
        "events": Stage::M1.events(),
    })
}

fn args(page: Option<std::path::PathBuf>) -> SetupArgs {
    SetupArgs::parse(
        [
            "--webhook-url",
            &format!("https://{HOST}/webhook"),
            "--app-owner",
            "org:acme",
        ]
        .into_iter()
        .map(String::from)
        .chain(
            page.map(|p| vec!["--page".to_owned(), p.display().to_string()])
                .into_iter()
                .flatten(),
        ),
    )
    .unwrap()
}

fn run(
    args: &SetupArgs,
    control: &FakeControl,
    probe: &FakeEntry,
) -> (Result<github_setup::CreatedApp, SetupError>, String, usize) {
    let mut out = Vec::new();
    let mut sleeps = 0;
    let mut sleep = |_d: Duration| sleeps += 1;
    let result = github_setup::run(
        args,
        Env {
            control,
            probe,
            out: &mut out,
            sleep: &mut sleep,
        },
    );
    (result, String::from_utf8(out).unwrap(), sleeps)
}

fn page_manifest(html: &str) -> Value {
    let start =
        html.find("name=\"manifest\" value=\"").unwrap() + "name=\"manifest\" value=\"".len();
    let end = start + html[start..].find('"').unwrap();
    let raw = html[start..end]
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn happy_path_writes_page_polls_and_prints_install_link() {
    let dir = std::env::temp_dir().join(format!("gantry-deploy-setup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let page_path = dir.join("setup.html");
    let _ = std::fs::remove_file(&page_path);
    let control = FakeControl::new(vec![
        json!({ "status": "pending" }),
        json!({ "status": "pending" }),
        json!({ "status": "completed", "app": granted_app() }),
    ]);
    let (result, out, sleeps) = run(
        &args(Some(page_path.clone())),
        &control,
        &FakeEntry::ready(),
    );
    let app = result.unwrap();
    assert_eq!(app.app_id, 424242);
    assert_eq!(
        app.install_url,
        "https://github.com/apps/gantry-hooks/installations/new"
    );
    assert_eq!(sleeps, 2);
    assert!(out.contains("只选择需要的仓库"), "{out}");
    assert_eq!(
        control.cmds(),
        [
            "github.setup.begin",
            "github.setup.status",
            "github.setup.status",
            "github.setup.status"
        ]
    );
    let begin_args = &control.calls.borrow()[0].1;
    assert_eq!(begin_args["webhook_url"], format!("https://{HOST}/webhook"));
    assert_eq!(
        begin_args["redirect_url"],
        format!("https://{HOST}/setup/github/callback")
    );

    let html = std::fs::read_to_string(&page_path).unwrap();
    assert!(html.contains(&format!(
        "action=\"https://github.com/organizations/acme/settings/apps/new?state={STATE}\""
    )));
    let manifest = page_manifest(&html);
    assert_eq!(
        manifest["hook_attributes"]["url"],
        format!("https://{HOST}/webhook")
    );
    assert_eq!(
        manifest["redirect_url"],
        format!("https://{HOST}/setup/github/callback")
    );
    assert_eq!(manifest["public"], false);
    assert_eq!(manifest["request_oauth_on_install"], false);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn m1_manifest_permissions_and_events_match_forge_actions_subset() {
    let entry = Entry::parse(&format!("https://{HOST}/webhook")).unwrap();
    let m1 = manifest::build(Stage::M1, &entry, "gantry");
    let perms = m1["default_permissions"].as_object().unwrap();
    let keys: Vec<&str> = perms.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["metadata", "contents", "pull_requests", "issues", "checks"]
    );
    assert_eq!(perms["metadata"], "read");
    assert_eq!(perms["contents"], "write");
    assert_eq!(perms["checks"], "read");
    let events: Vec<&str> = m1["default_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    for absent in ["workflows", "administration", "actions", "merge_queues"] {
        assert!(!perms.contains_key(absent), "M1 不得含 {absent}");
    }
    assert!(!events.contains(&"merge_group"));

    let m2 = manifest::build(Stage::M2, &entry, "gantry");
    let perms = m2["default_permissions"].as_object().unwrap();
    assert_eq!(perms["actions"], "write");
    assert_eq!(perms["merge_queues"], "read");
    assert!(m2["default_events"]
        .as_array()
        .unwrap()
        .contains(&json!("merge_group")));
    for forbidden in manifest::FORBIDDEN_PERMISSIONS {
        assert!(!perms.contains_key(*forbidden));
    }
}

#[test]
fn refuses_when_entry_not_ready_and_registers_no_session() {
    // 入口没把 /webhook 转到 forge（例如隧道还没配好）。
    let probe = FakeEntry::ready().with(Method::Post, "/webhook", Err("连接被拒绝".into()));
    let control = FakeControl::new(vec![]);
    let (result, _, _) = run(&args(None), &control, &probe);
    assert!(
        matches!(result, Err(SetupError::EntryNotReady(_))),
        "{result:?}"
    );
    assert!(control.cmds().is_empty(), "入口未就绪时不得登记会话");

    // 入口把 API 路径也透出去了：同样拒绝。
    let probe = FakeEntry::ready().with(Method::Get, "/v1/me", Ok(401));
    let (result, _, _) = run(&args(None), &FakeControl::new(vec![]), &probe);
    assert!(
        matches!(result, Err(SetupError::EntryNotReady(_))),
        "{result:?}"
    );
}

#[test]
fn refuses_after_bootstrap_period() {
    let mut control = FakeControl::new(vec![]);
    control.begin = Err(ControlError::Rejected {
        code: "not_bootstrap_period".into(),
        message: "已有 approver 设备".into(),
    });
    let (result, out, _) = run(&args(None), &control, &FakeEntry::ready());
    assert!(
        matches!(result, Err(SetupError::NotBootstrapPeriod(_))),
        "{result:?}"
    );
    assert!(!out.contains("<form"), "被拒时不生成提交页");
}

#[test]
fn persist_failure_prints_html_url_and_steps_but_never_secrets() {
    let control = FakeControl::new(vec![json!({
        "status": "failed",
        "app": { "html_url": "https://github.com/settings/apps/gantry-hooks" },
        "error": { "code": "persist_failed", "message": "secrets 写入失败" },
    })]);
    let (result, out, _) = run(&args(None), &control, &FakeEntry::ready());
    assert!(
        matches!(&result, Err(SetupError::Failed { code, .. }) if code == "persist_failed"),
        "{result:?}"
    );
    assert!(out.contains("https://github.com/settings/apps/gantry-hooks"));
    assert!(out.contains("重新运行 gantry github setup"));
    assert!(!out.contains("PRIVATE KEY"));
}

#[test]
fn response_carrying_secret_fields_is_refused_not_printed() {
    let mut app = granted_app();
    app["pem"] = json!(SENTINEL_PEM);
    let control = FakeControl::new(vec![json!({ "status": "completed", "app": app })]);
    let (result, out, _) = run(&args(None), &control, &FakeEntry::ready());
    assert!(matches!(result, Err(SetupError::Protocol(_))), "{result:?}");
    assert!(!out.contains("SENTINEL"));
    assert!(!format!("{result:?}").contains("SENTINEL"));
}

#[test]
fn granted_permissions_must_equal_manifest() {
    let mut app = granted_app();
    app["permissions"]["workflows"] = json!("write");
    let control = FakeControl::new(vec![json!({ "status": "completed", "app": app })]);
    let (result, _, _) = run(&args(None), &control, &FakeEntry::ready());
    match result {
        Err(SetupError::PermissionMismatch(detail)) => {
            assert!(detail.contains("workflows"), "{detail}")
        }
        other => panic!("{other:?}"),
    }

    let mut app = granted_app();
    app["events"] = json!(["push"]);
    let control = FakeControl::new(vec![json!({ "status": "completed", "app": app })]);
    let (result, _, _) = run(&args(None), &control, &FakeEntry::ready());
    assert!(
        matches!(result, Err(SetupError::PermissionMismatch(_))),
        "{result:?}"
    );
}

#[test]
fn expired_session_asks_to_rerun() {
    let control = FakeControl::new(vec![
        json!({ "status": "pending" }),
        json!({ "status": "expired" }),
    ]);
    let (result, _, _) = run(&args(None), &control, &FakeEntry::ready());
    assert_eq!(result, Err(SetupError::Expired));
}

#[test]
fn malformed_state_is_a_protocol_error() {
    let mut control = FakeControl::new(vec![]);
    control.begin = Ok(json!({ "state": "short\"><script>" }));
    let (result, out, _) = run(&args(None), &control, &FakeEntry::ready());
    assert!(matches!(result, Err(SetupError::Protocol(_))), "{result:?}");
    assert!(!out.contains("<script>"));
}

#[test]
fn argument_validation() {
    let parse = |v: &[&str]| SetupArgs::parse(v.iter().map(|s| s.to_string()));
    assert!(matches!(parse(&[]), Err(SetupError::Usage(_))));
    assert!(matches!(
        parse(&[
            "--webhook-url",
            "https://h/webhook",
            "--github-web",
            "http://127.0.0.1:9"
        ]),
        Err(SetupError::Usage(_))
    ));
    assert!(parse(&[
        "--webhook-url",
        "https://h/webhook",
        "--test-mode",
        "--github-web",
        "http://127.0.0.1:9"
    ])
    .is_ok());

    for bad in [
        "http://h.example/webhook",
        "https://h.example/hook",
        "https://h.example/webhook?x=1",
        "https://user@h.example/webhook",
        "https://h.example:x/webhook",
        "https://h..example/webhook",
    ] {
        assert!(Entry::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(
        Entry::parse("https://H.Example:8443/webhook")
            .unwrap()
            .redirect_url(),
        "https://h.example:8443/setup/github/callback"
    );
    for bad in ["org:", "org:-x", "org:a b", "team:x", "org:a\"b"] {
        assert!(manifest::AppOwner::parse(bad).is_err(), "{bad}");
    }
    let entry = Entry::parse("https://hooks.example.net/webhook").unwrap();
    assert_eq!(manifest::app_name(None, &entry).unwrap(), "gantry-hooks");
    assert!(manifest::app_name(Some("x\"<y"), &entry).is_err());
    assert!(manifest::app_name(Some(&"a".repeat(35)), &entry).is_err());
}

#[test]
fn page_escapes_manifest_values() {
    let html = gantry_deploy::page::render(
        "https://github.com/settings/apps/new?state=s&x=1",
        &json!({ "name": "a\"><script>" }),
    );
    assert!(!html.contains("\"><script>"));
    assert!(html.contains("state=s&amp;x=1"));
    assert_eq!(page_manifest(&html)["name"], "a\"><script>");
}

#[test]
fn page_file_is_private_and_refuses_overwrite() {
    let dir = std::env::temp_dir().join(format!("gantry-deploy-page-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let page_path = dir.join("private.html");
    let _ = std::fs::remove_file(&page_path);

    let control = FakeControl::new(vec![
        json!({ "status": "pending" }),
        json!({ "status": "completed", "app": granted_app() }),
    ]);
    run(
        &args(Some(page_path.clone())),
        &control,
        &FakeEntry::ready(),
    )
    .0
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&page_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let before = std::fs::read_to_string(&page_path).unwrap();
    let control = FakeControl::new(vec![json!({ "status": "pending" })]);
    let (result, _, _) = run(
        &args(Some(page_path.clone())),
        &control,
        &FakeEntry::ready(),
    );
    assert!(matches!(result, Err(SetupError::Io(_))));
    assert_eq!(std::fs::read_to_string(&page_path).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn page_file_rejects_symlink() {
    let dir = std::env::temp_dir().join(format!("gantry-deploy-symlink-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("target.html");
    let link = dir.join("link.html");
    std::fs::write(&target, "sentinel").unwrap();
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let control = FakeControl::new(vec![json!({ "status": "pending" })]);
    let (result, _, _) = run(&args(Some(link)), &control, &FakeEntry::ready());
    assert!(matches!(result, Err(SetupError::Io(_))));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "sentinel");
    let _ = std::fs::remove_dir_all(&dir);
}
