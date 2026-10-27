//! App 鉴权的验收：forge.md §7.12、§12 A 组"安装出现 `workflows` 权限"与"令牌不出现在日志"，
//! FA §9.4–§9.5。全部对 `FakeGitHub`（回环 HTTP，A 级）；RSA 测试密钥在运行时用系统
//! `openssl` 生成，不进仓库。

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use gantry_core::canon::{GitOid, RepoSlug};
use gantry_core::entities::{Actor, ActorKind, MergeMethod, Priority, Project};
use gantry_core::forge::{ActionPayload, CloseIssuePayload, CreatePrPayload, IssueStateReason};
use gantry_core::ids::{Id, IdGen, IdKind, Prj, Ulid};
use gantry_core::ports::{
    ActorCtx, EnqueueError, EnqueueRequest, EnqueuedBy, EnqueuedByModule, Expect, OutboxPort,
    SecretSink, SecretValue,
};
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_fakes::github::{FakeGitHub, ScriptedFailure};
use gantry_fakes::secrets::MemorySecretStore;
use gantry_forge::{
    AppAuthDeps, AppAuthOptions, AppTokenProvider, AuthError, Forge, ForgeConfig, Permission,
    ProjectBinding, TokenProvider, TokenPurpose,
};
use gantry_store::{Store, StoreDeps};
use serde_json::{json, Value};

const MS: i64 = 1_700_000_000_000;
const APP_ID: u64 = 1234;
const INSTALLATION: i64 = 42;

fn id<K: IdKind>(n: u128) -> Id<K> {
    Id::from_parts(MS as u64, n).expect("test id parts")
}

fn ts() -> Timestamp {
    Timestamp::from_millis(MS)
}

fn slug() -> RepoSlug {
    RepoSlug::parse("acme/shop-api").expect("slug")
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
        repo: slug(),
        installation_id: INSTALLATION,
        bot_user_id: 9,
        default_branch: "main".into(),
        allowed_merge_methods: vec![MergeMethod::Squash],
        merge_queue_enabled: false,
        created_at: ts(),
        updated_at: ts(),
    }
}

fn config(granted: BTreeSet<Permission>) -> ForgeConfig {
    ForgeConfig::default().with_project(ProjectBinding {
        project: project(),
        granted,
    })
}

/// 运行时生成的 RSA 密钥对（PEM）。
struct KeyPair {
    private_pem: Vec<u8>,
    public_pem: Vec<u8>,
}

fn openssl(args: &[&str], dir: &Path) -> Vec<u8> {
    let output = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run openssl");
    assert!(
        output.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn keypair() -> KeyPair {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gantry-forge-app-auth-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("key dir");
    openssl(&["genrsa", "-out", "app.pem", "2048"], &dir);
    let public_pem = openssl(&["rsa", "-in", "app.pem", "-pubout"], &dir);
    let private_pem = std::fs::read(dir.join("app.pem")).expect("read key");
    std::fs::remove_dir_all(&dir).ok();
    KeyPair {
        private_pem,
        public_pem,
    }
}

struct Harness {
    github: FakeGitHub,
    clock: Arc<FakeClock>,
    secrets: MemorySecretStore,
    keys: KeyPair,
    provider: AppTokenProvider,
}

fn key_name() -> SecretName {
    SecretName::parse("github_app_private_key").expect("secret name")
}

fn harness(granted: BTreeSet<Permission>) -> Harness {
    let github = FakeGitHub::start();
    github.set_installation(
        INSTALLATION,
        json!({"metadata":"read","contents":"write","pull_requests":"write","issues":"write","checks":"read"}),
    );
    github.set_token_expires_at("2023-11-14T23:13:20Z"); // MS + 1h
    let clock = Arc::new(FakeClock::new(MS));
    let secrets = MemorySecretStore::new(ts());
    secrets.allow(Consumer::Forge);
    let keys = keypair();
    secrets
        .scoped(Consumer::Forge)
        .expect("forge secrets")
        .put(
            &key_name(),
            SecretValue::from_bytes(keys.private_pem.clone()),
            Expect::Absent,
            &ctx(),
        )
        .expect("put key");
    let provider = AppTokenProvider::new(
        Arc::new(config(granted)),
        AppAuthDeps {
            app_id: APP_ID,
            secrets: Arc::new(secrets.reader(Consumer::Forge).expect("reader")),
            private_key: key_name(),
            clock: clock.clone(),
            api_base: github.base_url(),
            options: AppAuthOptions::default(),
        },
    );
    Harness {
        github,
        clock,
        secrets,
        keys,
        provider,
    }
}

fn all() -> BTreeSet<Permission> {
    [
        Permission::ContentsWrite,
        Permission::PullRequestsWrite,
        Permission::IssuesWrite,
    ]
    .into_iter()
    .collect()
}

fn token_calls(github: &FakeGitHub) -> Vec<gantry_fakes::github::GithubCall> {
    github
        .recorded_calls()
        .into_iter()
        .filter(|call| call.path.ends_with("/access_tokens"))
        .collect()
}

#[tokio::test]
async fn installation_token_is_minted_with_a_verifiable_app_jwt_and_narrowed_scope() {
    let h = harness(all());
    let token = h
        .provider
        .token(&id(1), TokenPurpose::Write)
        .await
        .expect("token");
    assert!(token.expose_secret().starts_with("ghs_"));

    let calls = token_calls(&h.github);
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.method, "POST");
    assert_eq!(
        call.path,
        format!("/app/installations/{INSTALLATION}/access_tokens")
    );
    let jwt = call
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .expect("bearer jwt");
    let key = jsonwebtoken::DecodingKey::from_rsa_pem(&h.keys.public_pem).expect("public key");
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.validate_exp = false; // FakeClock 在过去；只核对声明本身
    validation.required_spec_claims = ["exp", "iat", "iss"]
        .into_iter()
        .map(String::from)
        .collect();
    let claims = jsonwebtoken::decode::<Value>(jwt, &key, &validation)
        .expect("jwt verifies with the app public key")
        .claims;
    let now = MS / 1000;
    assert_eq!(claims["iss"], json!(APP_ID.to_string()));
    assert_eq!(claims["iat"], json!(now - 60), "iat 回拨 60 秒");
    assert_eq!(claims["exp"], json!(now + 9 * 60), "exp = now + jwt_ttl");

    let body: Value = serde_json::from_str(&call.body).expect("json body");
    assert_eq!(body["repositories"], json!(["shop-api"]));
    let scope = body["permissions"].as_object().expect("permissions");
    assert_eq!(scope["contents"], "write");
    assert!(!scope.contains_key("workflows") && !scope.contains_key("administration"));

    // 只读用途单独换令牌，且不含任何 write。
    h.provider
        .token(&id(1), TokenPurpose::Read)
        .await
        .expect("read token");
    let calls = token_calls(&h.github);
    assert_eq!(calls.len(), 2);
    let body: Value = serde_json::from_str(&calls[1].body).expect("json body");
    assert!(body["permissions"]
        .as_object()
        .expect("permissions")
        .values()
        .all(|level| level != "write"));
    // 换令牌不算仓库写入。
    assert_eq!(h.github.write_count(), 0);
}

#[tokio::test]
async fn tokens_are_cached_until_the_refresh_margin_and_invalidate_forces_a_new_one() {
    let h = harness(all());
    let first = h.provider.token(&id(1), TokenPurpose::Write).await.unwrap();
    let again = h.provider.token(&id(1), TokenPurpose::Write).await.unwrap();
    assert_eq!(first.expose_secret(), again.expose_secret());
    assert_eq!(token_calls(&h.github).len(), 1);

    // expires_at = MS + 1h；提前 5 分钟刷新：+54 分钟仍命中缓存，+55 分钟换新。
    h.clock.advance_secs(54 * 60);
    h.provider.token(&id(1), TokenPurpose::Write).await.unwrap();
    assert_eq!(token_calls(&h.github).len(), 1);
    h.clock.advance_secs(60);
    let refreshed = h.provider.token(&id(1), TokenPurpose::Write).await.unwrap();
    assert_eq!(token_calls(&h.github).len(), 2);
    assert_ne!(refreshed.expose_secret(), first.expose_secret());

    h.provider.invalidate(&id(1));
    h.provider.token(&id(1), TokenPurpose::Write).await.unwrap();
    assert_eq!(token_calls(&h.github).len(), 3);
}

#[tokio::test]
async fn concurrent_refresh_is_single_flight() {
    let h = harness(all());
    let provider = Arc::new(h.provider);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let provider = provider.clone();
            tokio::spawn(async move { provider.token(&id(1), TokenPurpose::Write).await })
        })
        .collect();
    for task in tasks {
        task.await.expect("join").expect("token");
    }
    assert_eq!(token_calls(&h.github).len(), 1);
}

#[tokio::test]
async fn missing_private_key_fails_closed_without_calling_github() {
    let h = harness(all());
    h.secrets
        .scoped(Consumer::Forge)
        .unwrap()
        .delete(&key_name(), Expect::Any, &ctx())
        .expect("delete key");
    let error = h
        .provider
        .token(&id(1), TokenPurpose::Write)
        .await
        .expect_err("no key");
    assert_eq!(error, AuthError::CredentialsMissing);
    assert!(h.github.recorded_calls().is_empty());
}

#[tokio::test]
async fn unknown_installation_is_rejected() {
    let h = harness(all());
    h.github.remove_installation(INSTALLATION);
    let error = h
        .provider
        .token(&id(1), TokenPurpose::Write)
        .await
        .expect_err("installation gone");
    assert_eq!(error, AuthError::Rejected);
}

#[tokio::test]
async fn token_never_shows_in_debug_output() {
    let h = harness(all());
    let token = h.provider.token(&id(1), TokenPurpose::Write).await.unwrap();
    let rendered = format!("{:?} {:?}", h.provider, token);
    assert!(!rendered.contains(token.expose_secret()));
    assert!(!rendered.contains("BEGIN"));
}

fn open_store() -> Store {
    let clock = Arc::new(FakeClock::new(MS));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    Store::open_in_memory_sync(StoreDeps { clock, ids }).expect("store")
}

fn request(payload: ActionPayload) -> EnqueueRequest {
    EnqueueRequest {
        project_id: id::<Prj>(1),
        delivery_id: id(2),
        payload,
        requires_approval: None,
        depends_on: vec![],
        priority: Priority::High,
        not_before: None,
        epoch: 0,
        enqueued_by: EnqueuedBy {
            module: EnqueuedByModule::Engine,
            causation_id: None,
        },
    }
}

fn create_pr() -> ActionPayload {
    ActionPayload::CreatePr(CreatePrPayload {
        repo: slug(),
        head: "gantry/42-fix".into(),
        base: "main".into(),
        title: "Fix".into(),
        body: "Fix it.".into(),
        draft: false,
        closes: vec![42],
        expected_head_oid: GitOid::parse("d7d018f867d6c5b78e82906386420d45a4d84486").unwrap(),
        maintainer_can_modify: false,
    })
}

fn close_issue() -> ActionPayload {
    ActionPayload::CloseIssue(CloseIssuePayload {
        repo: slug(),
        number: 42,
        state_reason: IssueStateReason::Completed,
        require_merged_pr: None,
    })
}

async fn enqueue(forge: &Forge, store: &Store, payload: ActionPayload) -> Result<(), EnqueueError> {
    let mut tx = store.begin_write().await.expect("tx");
    let result = forge.enqueue_in(&mut tx, request(payload), &ctx()).await;
    tx.commit().await.expect("commit");
    result.map(|_| ())
}

/// §12 A：安装出现 `workflows` 权限 → 自检拒绝该 Project 写路径并上报；缺权限的动作入队即
/// `PermissionMissing`。
#[tokio::test]
async fn self_check_denies_write_path_on_high_risk_permission() {
    let h = harness(all());
    h.github.set_installation(
        INSTALLATION,
        json!({"metadata":"read","contents":"write","pull_requests":"write","issues":"write","workflows":"write"}),
    );
    let report = h
        .provider
        .self_check(&id(1), true)
        .await
        .expect("self check");
    assert_eq!(report.high_risk, vec!["workflows".to_owned()]);
    assert!(report.write_denied);
    assert!(report.effective().is_empty());
    // 自检用 App JWT，不换安装令牌。
    assert!(token_calls(&h.github).is_empty());

    let store = open_store();
    let forge = Forge::new(store.clone(), config(report.effective()));
    assert!(matches!(
        enqueue(&forge, &store, create_pr()).await,
        Err(EnqueueError::PermissionMissing { .. })
    ));

    // 关掉 deny_extra_high_risk：写路径保留，只上报。
    let report = h.provider.self_check(&id(1), false).await.unwrap();
    assert!(!report.write_denied);
    assert_eq!(report.effective(), all());
}

#[tokio::test]
async fn self_check_reports_missing_permissions_and_gates_those_actions() {
    let h = harness(all());
    h.github.set_installation(
        INSTALLATION,
        json!({"metadata":"read","contents":"write","pull_requests":"write","issues":"read"}),
    );
    let report = h.provider.self_check(&id(1), true).await.unwrap();
    assert!(!report.write_denied);
    assert_eq!(report.missing(), vec![Permission::IssuesWrite]);

    let store = open_store();
    let forge = Forge::new(store.clone(), config(report.effective()));
    enqueue(&forge, &store, create_pr())
        .await
        .expect("create_pr allowed");
    assert!(matches!(
        enqueue(&forge, &store, close_issue()).await,
        Err(EnqueueError::PermissionMissing {
            permission: "issues:write",
            ..
        })
    ));
}

/// #213 评审：缺一项写权限时，写令牌只请求已获批的写权限，其余动作照常可换到令牌。
#[tokio::test]
async fn write_token_requests_only_granted_write_permissions() {
    let granted: BTreeSet<Permission> = [Permission::ContentsWrite, Permission::PullRequestsWrite]
        .into_iter()
        .collect();
    let h = harness(granted);
    h.github.set_installation(
        INSTALLATION,
        json!({"metadata":"read","contents":"write","pull_requests":"write","issues":"read","checks":"read"}),
    );
    h.provider
        .token(&id(1), TokenPurpose::Write)
        .await
        .expect("write token without issues:write");
    let body: Value = serde_json::from_str(&token_calls(&h.github)[0].body).unwrap();
    let scope = body["permissions"].as_object().unwrap();
    assert_eq!(scope.get("contents").and_then(Value::as_str), Some("write"));
    assert_eq!(
        scope.get("pull_requests").and_then(Value::as_str),
        Some("write")
    );
    assert!(!scope.contains_key("issues"));

    // 替身同 GitHub：请求超出获批范围整次被拒，所以上面的收窄是必要的。
    let over = harness(all());
    over.github.set_installation(
        INSTALLATION,
        json!({"metadata":"read","contents":"write","pull_requests":"write","issues":"read","checks":"read"}),
    );
    assert!(over
        .provider
        .token(&id(1), TokenPurpose::Write)
        .await
        .is_err());
}

/// #213 评审：组织级权限（FA §9.2「等」）同样触发写路径拒绝。
#[tokio::test]
async fn organization_level_permissions_are_high_risk() {
    let h = harness(all());
    h.github.set_installation(
        INSTALLATION,
        json!({"metadata":"read","contents":"write","pull_requests":"write","issues":"write",
               "organization_administration":"write","members":"read"}),
    );
    let report = h.provider.self_check(&id(1), true).await.unwrap();
    assert!(report.write_denied);
    assert!(report
        .high_risk
        .contains(&"organization_administration".to_owned()));
    assert!(report.high_risk.contains(&"members".to_owned()));
}

/// #213 评审：换令牌被限流时保留 retry-after，交给调用方等待。
#[tokio::test]
async fn rate_limited_token_exchange_keeps_retry_after() {
    let h = harness(all());
    h.github.push_failure(ScriptedFailure {
        status: 403,
        body: r#"{"message":"You have exceeded a secondary rate limit"}"#.into(),
        retry_after_s: Some(120),
    });
    let error = h
        .provider
        .token(&id(1), TokenPurpose::Write)
        .await
        .expect_err("rate limited");
    assert_eq!(
        error,
        AuthError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(120))
        }
    );
}
