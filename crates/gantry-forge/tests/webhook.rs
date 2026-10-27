//! Webhook 接收的验收：forge.md §12 A 组"重复投递"（落盘一行，首次 202、二次 200）、
//! "签名错误 / 缺头 / 超大 body"、"`gantry/` 分支的 `push` 落盘前丢弃"、"监听绑定非回环地址时
//! 启动失败"，以及 §12 C 组"订阅清单与 FA §9.3 逐行一致"。全部对 `FakeGitHub`（A 级）。

use std::sync::Arc;
use std::time::Duration;

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{Actor, ActorKind, MergeMethod, Project};
use gantry_core::ids::{Id, IdGen, IdKind, Prj, Ulid};
use gantry_core::ports::{ActorCtx, Expect, SecretSink, SecretValue};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::github::FakeGitHub;
use gantry_fakes::secrets::MemorySecretStore;
use gantry_forge::webhook::subscriptions::SUBSCRIPTIONS;
use gantry_forge::webhook::{INBOX_TABLE, MODULE};
use gantry_forge::{
    module_spec, ForgeConfig, InboxRow, InboxStatus, ProjectBinding, WebhookDeps, WebhookError,
    WebhookOptions, WebhookReceiver, WebhookStats,
};
use gantry_store::{Store, StoreDeps};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MS: i64 = 1_700_000_000_000;
const APP_ID: u64 = 1234;
const INSTALLATION: i64 = 42;
const REPO: &str = "acme/shop-api";
const CURRENT: &str = "current-secret";
const PREVIOUS: &str = "previous-secret";

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS as u64, n).expect("test id parts")
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(MS as u64, 1).expect("request id"),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn project() -> Project {
    Project {
        id: id::<Prj>(1),
        repo: RepoSlug::parse(REPO).expect("slug"),
        installation_id: INSTALLATION,
        bot_user_id: 9,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: Timestamp::from_millis(MS),
        updated_at: Timestamp::from_millis(MS),
    }
}

fn name(value: &str) -> SecretName {
    SecretName::parse(value).expect("secret name")
}

struct Harness {
    store: Store,
    secrets: MemorySecretStore,
    receiver: WebhookReceiver,
}

async fn harness_with(options: WebhookOptions, seed_secrets: bool) -> Harness {
    let clock = Arc::new(FakeClock::new(MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids,
        },
        &[module_spec()],
    )
    .await
    .expect("store");
    let secrets = MemorySecretStore::new(Timestamp::from_millis(MS));
    secrets.allow(Consumer::Forge);
    if seed_secrets {
        let writer = secrets.scoped(Consumer::Forge).expect("forge secrets");
        for (secret, value) in [
            ("webhook_secret", CURRENT),
            ("webhook_secret_previous", PREVIOUS),
        ] {
            writer
                .put(
                    &name(secret),
                    SecretValue::from_bytes(value.as_bytes().to_vec()),
                    Expect::Absent,
                    &ctx(),
                )
                .expect("put secret");
        }
    }
    let receiver = WebhookReceiver::bind(WebhookDeps {
        store: store.clone(),
        config: Arc::new(ForgeConfig::default().with_project(ProjectBinding {
            project: project(),
            granted: Default::default(),
        })),
        secrets: Arc::new(secrets.reader(Consumer::Forge).expect("reader")),
        secret: name("webhook_secret"),
        previous_secret: Some(name("webhook_secret_previous")),
        app_id: APP_ID,
        clock,
        options,
    })
    .await
    .expect("bind");
    Harness {
        store,
        secrets,
        receiver,
    }
}

fn loopback() -> WebhookOptions {
    WebhookOptions {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..WebhookOptions::default()
    }
}

async fn harness() -> Harness {
    harness_with(loopback(), true).await
}

fn rows(store: &Store) -> Vec<InboxRow> {
    store
        .snapshot_sync()
        .unwrap()
        .module_conn(MODULE)
        .scan::<InboxRow>(INBOX_TABLE, "", None, 200)
        .unwrap()
        .into_iter()
        .map(|(_, row)| row)
        .collect()
}

fn github_for(receiver: &WebhookReceiver, secret: &str) -> FakeGitHub {
    let github = FakeGitHub::start();
    github.set_app_id(APP_ID);
    github.register_webhook_with_secret(receiver.url(), secret);
    github
}

/// `FakeGitHub` 的投递是阻塞调用。
async fn deliver(github: Arc<FakeGitHub>, event: &'static str, payload: Value) -> (String, u16) {
    tokio::task::spawn_blocking(move || {
        let delivery = github.deliver_webhook(event, &payload).expect("deliver");
        (delivery[0].delivery_id.clone(), delivery[0].status)
    })
    .await
    .expect("join")
}

fn pr_opened() -> Value {
    json!({
        "action": "opened",
        "number": 57,
        "installation": {"id": INSTALLATION},
        "repository": {"full_name": REPO},
        "pull_request": {"number": 57},
        "sender": {"id": 9}
    })
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    format!(
        "sha256={}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// 手写请求，覆盖 `FakeGitHub` 不会发出的畸形请求。
async fn raw(receiver: &WebhookReceiver, headers: &[(&str, String)], body: &[u8]) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(receiver.local_addr())
        .await
        .expect("connect");
    let mut head = String::from("POST /webhook HTTP/1.1\r\nhost: localhost\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    // 413 在读 body 之前返回；对端可能已关闭，写失败可忽略。
    let _ = stream.write_all(body).await;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn signed_headers(
    event: &str,
    delivery: &str,
    secret: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    vec![
        ("x-github-event", event.to_owned()),
        ("x-github-delivery", delivery.to_owned()),
        ("x-hub-signature-256", sign(secret, body)),
        ("x-github-hook-installation-target-id", APP_ID.to_string()),
        ("content-length", body.len().to_string()),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_delivery_is_stored_once_202_then_200() {
    let h = harness().await;
    let github = Arc::new(github_for(&h.receiver, CURRENT));
    let (delivery, status) = deliver(github.clone(), "pull_request", pr_opened()).await;
    assert_eq!(status, 202);
    let again = {
        let github = github.clone();
        let delivery = delivery.clone();
        tokio::task::spawn_blocking(move || github.redeliver(&delivery).unwrap()[0].status)
            .await
            .unwrap()
    };
    assert_eq!(again, 200);

    let stored = rows(&h.store);
    assert_eq!(stored.len(), 1);
    let row = &stored[0];
    assert_eq!(row.delivery_id, delivery);
    assert_eq!(row.event, "pull_request");
    assert_eq!(row.action.as_deref(), Some("opened"));
    assert_eq!(row.installation_id, Some(INSTALLATION));
    assert_eq!(row.repo.as_deref(), Some(REPO));
    assert_eq!(row.status, InboxStatus::Received);
    assert_eq!(row.received_at, Timestamp::from_millis(MS));
    let body: Value = serde_json::from_str(row.body.as_deref().unwrap()).unwrap();
    assert_eq!(body, pr_opened());
    assert_eq!(WebhookStats::get(&h.receiver.stats().accepted), 1);
    assert_eq!(WebhookStats::get(&h.receiver.stats().duplicates), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn previous_secret_is_accepted_and_both_wrong_is_401() {
    let h = harness().await;
    let previous = Arc::new(github_for(&h.receiver, PREVIOUS));
    assert_eq!(deliver(previous, "pull_request", pr_opened()).await.1, 202);
    let wrong = Arc::new(github_for(&h.receiver, "nope"));
    assert_eq!(deliver(wrong, "pull_request", pr_opened()).await.1, 401);
    assert_eq!(rows(&h.store).len(), 1);
    assert_eq!(WebhookStats::get(&h.receiver.stats().bad_signature), 1);
}

#[tokio::test]
async fn bad_signature_missing_header_and_oversize_are_rejected_without_storing() {
    let h = harness_with(
        WebhookOptions {
            max_body_bytes: 1024,
            ..loopback()
        },
        true,
    )
    .await;
    let body = serde_json::to_vec(&pr_opened()).unwrap();

    let mut tampered = signed_headers("pull_request", "d-1", CURRENT, &body);
    tampered[2].1 = sign(CURRENT, b"something else");
    assert_eq!(raw(&h.receiver, &tampered, &body).await, 401);

    let mut missing = signed_headers("pull_request", "d-2", CURRENT, &body);
    missing.remove(1); // 无 X-GitHub-Delivery
    assert_eq!(raw(&h.receiver, &missing, &body).await, 400);

    let big = vec![b' '; 2048];
    let oversize = signed_headers("pull_request", "d-3", CURRENT, &big);
    assert_eq!(raw(&h.receiver, &oversize, &big).await, 413);

    // 旧的 SHA-1 头不被接受。
    let mut sha1 = signed_headers("pull_request", "d-4", CURRENT, &body);
    sha1[2] = ("x-hub-signature", "sha1=00".into());
    assert_eq!(raw(&h.receiver, &sha1, &body).await, 400);

    assert!(rows(&h.store).is_empty());
    // 正确签名的同一请求能通过，说明拒绝不是别的原因。
    let ok = signed_headers("pull_request", "d-5", CURRENT, &body);
    assert_eq!(raw(&h.receiver, &ok, &body).await, 202);
}

#[tokio::test]
async fn filtered_deliveries_return_200_without_storing() {
    let h = harness().await;
    let send = |event: &'static str, payload: Value, delivery: &'static str| {
        let body = serde_json::to_vec(&payload).unwrap();
        let headers = signed_headers(event, delivery, CURRENT, &body);
        let receiver = &h.receiver;
        async move { raw(receiver, &headers, &body).await }
    };
    assert_eq!(send("ping", json!({"zen": "hi"}), "p-1").await, 200);
    let mut assigned = pr_opened();
    assigned["action"] = json!("assigned");
    assert_eq!(send("pull_request", assigned, "p-2").await, 200);
    assert_eq!(send("workflow_run", pr_opened(), "p-3").await, 200);
    let mut stranger = pr_opened();
    stranger["installation"]["id"] = json!(777);
    assert_eq!(send("pull_request", stranger, "p-4").await, 200);
    let push = |branch: &str| {
        json!({
            "ref": format!("refs/heads/{branch}"),
            "after": "a".repeat(40),
            "installation": {"id": INSTALLATION},
            "repository": {"full_name": REPO}
        })
    };
    assert_eq!(send("push", push("gantry/42-fix"), "p-5").await, 200);
    assert!(rows(&h.store).is_empty());

    // 投递目标不是本 App：同样 200 不落盘。
    let body = serde_json::to_vec(&pr_opened()).unwrap();
    let mut foreign = signed_headers("pull_request", "p-6", CURRENT, &body);
    foreign[3].1 = "999".into();
    assert_eq!(raw(&h.receiver, &foreign, &body).await, 200);
    assert!(rows(&h.store).is_empty());

    let stats = h.receiver.stats();
    assert_eq!(WebhookStats::get(&stats.pings), 1);
    assert_eq!(WebhookStats::get(&stats.unsubscribed), 3);
    assert_eq!(WebhookStats::get(&stats.unbound_installation), 1);
    assert_eq!(WebhookStats::get(&stats.foreign_target), 1);

    // 默认分支的 push 与 installation 事件（不属于任何 Project 也接收）落盘。
    assert_eq!(send("push", push("main"), "p-7").await, 202);
    assert_eq!(
        send(
            "installation",
            json!({"action": "deleted", "installation": {"id": 777}}),
            "p-8"
        )
        .await,
        202
    );
    assert_eq!(rows(&h.store).len(), 2);
}

#[tokio::test]
async fn missing_webhook_secret_closes_verification() {
    let h = harness_with(loopback(), false).await;
    let body = serde_json::to_vec(&pr_opened()).unwrap();
    let headers = signed_headers("pull_request", "s-1", CURRENT, &body);
    assert_eq!(raw(&h.receiver, &headers, &body).await, 503);
    assert!(rows(&h.store).is_empty());
    drop(h.secrets);
}

#[tokio::test]
async fn binding_a_non_loopback_address_fails() {
    let clock = Arc::new(FakeClock::new(MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(
        StoreDeps {
            clock: clock.clone(),
            ids,
        },
        &[module_spec()],
    )
    .await
    .unwrap();
    let secrets = MemorySecretStore::new(Timestamp::from_millis(MS));
    secrets.allow(Consumer::Forge);
    let result = WebhookReceiver::bind(WebhookDeps {
        store,
        config: Arc::new(ForgeConfig::default()),
        secrets: Arc::new(secrets.reader(Consumer::Forge).unwrap()),
        secret: name("webhook_secret"),
        previous_secret: None,
        app_id: APP_ID,
        clock,
        options: WebhookOptions {
            listen: "0.0.0.0:0".parse().unwrap(),
            read_timeout: Duration::from_secs(1),
            ..WebhookOptions::default()
        },
    })
    .await;
    assert!(matches!(result, Err(WebhookError::NonLoopback(_))));
}

/// §12 C：订阅清单常量表与 FA §9.3 逐行一致。
#[test]
fn subscriptions_match_forge_actions_9_3() {
    let doc = include_str!("../../../docs/interfaces/forge-actions.md");
    let section = doc
        .split("### 9.3 订阅事件")
        .nth(1)
        .and_then(|rest| rest.split("\n**不订阅**").next())
        .expect("FA §9.3");
    let mut documented = Vec::new();
    for line in section.lines().filter(|line| line.starts_with("| `")) {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        let events: Vec<&str> = cells[1]
            .split('、')
            .map(|event| event.trim().trim_matches('`'))
            .collect();
        let actions: Vec<&str> = cells[3]
            .split_whitespace()
            .filter(|action| *action != "…" && *action != "全部" && *action != "——")
            .collect();
        for event in events {
            if event != "ping" {
                documented.push((event, actions.clone()));
            }
        }
    }
    let table: Vec<&str> = SUBSCRIPTIONS.iter().map(|s| s.event).collect();
    let doc_events: Vec<&str> = documented.iter().map(|(event, _)| *event).collect();
    assert_eq!(table, doc_events, "事件集合与顺序");
    for (event, actions) in documented {
        let subscription = SUBSCRIPTIONS.iter().find(|s| s.event == event).unwrap();
        for action in &actions {
            assert!(
                subscription.actions.contains(action),
                "{event}.{action} 在文档里但不在常量表"
            );
        }
        if !subscription.actions.is_empty() {
            // 常量表多出的 action 只允许出现在文档用省略号标注的行。
            let extra: Vec<_> = subscription
                .actions
                .iter()
                .filter(|action| !actions.contains(action))
                .collect();
            assert!(
                extra.is_empty()
                    || section.contains(&format!("`{event}` |")) && section.contains('…'),
                "{event} 多出 {extra:?}"
            );
        }
    }
}
