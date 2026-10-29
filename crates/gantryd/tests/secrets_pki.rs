//! `gantryd` 的真实 secrets 构造入口：只打开、不初始化；M1 使用方子集、
//! 命名空间隔离与 Egress / Notify 拒绝。
//!
//! 这些是本机替身级联测：不证明操作系统用户隔离，也不证明真实设备或 Ubuntu 部署。

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::Ulid;
use gantry_core::ports::{ActorCtx, Expect, SecretSink, SecretSource, SecretValue, SecretsError};
use gantry_core::secret::{Consumer, SecretName};
use gantry_secrets::{MasterKey, Secrets, SecretsConfig, StaticMasterKey};
use gantryd::assembly::build_secrets;
use gantryd::config::BootConfig;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> Scratch {
    use std::os::unix::fs::DirBuilderExt;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let dir = std::env::temp_dir().join(format!(
        "gantryd-secrets-{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .expect("scratch dir");
    Scratch(dir)
}

fn key(byte: u8) -> Box<StaticMasterKey> {
    Box::new(StaticMasterKey::new(MasterKey::from_bytes([byte; 32])))
}

fn ctx() -> ActorCtx {
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

fn init(path: &Path, consumers: &[Consumer]) {
    Secrets::init(SecretsConfig::new(path, consumers.to_vec(), key(1))).expect("init secrets");
}

fn config(secrets_dir: PathBuf) -> BootConfig {
    let mut config = BootConfig::parse(
        r#"
[server.api]
public_url = "https://gantry.test"
san = ["gantry.test"]

[forge.webhook]
public_url = "https://hooks.gantry.test/github"

[backup]
recipients = ["age1test"]
journal_target = "/tmp/gantry-test-journal/revocations.journal"
"#,
    )
    .expect("boot config");
    config.server.secrets_dir = secrets_dir;
    config
}

#[test]
fn build_secrets_never_initializes_a_missing_directory() {
    let dir = scratch();
    let path = dir.0.join("secrets");

    let error =
        build_secrets(&config(path.clone()), key(1)).expect_err("missing instance must fail");
    assert!(
        matches!(error, SecretsError::Io(ref error) if error.kind() == std::io::ErrorKind::NotFound),
        "{error:?}"
    );
    assert!(!path.exists(), "build_secrets must not create the instance");
}

#[test]
fn wrong_master_key_is_rejected_without_overwriting_the_instance() {
    let dir = scratch();
    let path = dir.0.join("secrets");
    init(&path, &[Consumer::Forge, Consumer::Pki]);
    let meta = std::fs::read(path.join("secrets.meta")).expect("secrets.meta");

    let error =
        build_secrets(&config(path.clone()), key(2)).expect_err("wrong master key must fail");
    assert!(matches!(error, SecretsError::KeyMismatch), "{error:?}");
    assert_eq!(
        std::fs::read(path.join("secrets.meta")).expect("secrets.meta"),
        meta,
        "a failed open must not rewrite secrets.meta"
    );

    build_secrets(&config(path.clone()), key(1))
        .expect("correct master key still opens the instance");
}

/// M1 实例由 deploy 初始化为 `[Forge, Pki]`（secrets.md §10：Notify 自 M4）。
/// `build_secrets` 必须以同一 M1 子集打开，且 Egress / Notify 都不承载。
#[test]
fn m1_instance_with_forge_and_pki_opens_and_rejects_egress_and_notify() {
    let dir = scratch();
    let path = dir.0.join("secrets");
    init(&path, &[Consumer::Forge, Consumer::Pki]);
    let secrets = build_secrets(&config(path.clone()), key(1)).expect("open M1 secrets");

    for who in [Consumer::Egress, Consumer::Notify] {
        for error in [secrets.reader(who).err(), secrets.scoped(who).err()] {
            assert!(
                matches!(
                    error,
                    Some(SecretsError::Denied {
                        who: denied, ..
                    }) if denied == who
                ),
                "{who:?}: {error:?}"
            );
        }
    }
}

/// M4 加入 Notify 后已迁移的实例仍能用 M1 的 `[Forge, Pki]` 子集打开；M1 实例不承载 Notify。
#[test]
fn notify_migrated_instance_still_opens_with_the_m1_subset() {
    let dir = scratch();
    let path = dir.0.join("secrets");
    init(&path, &[Consumer::Forge, Consumer::Pki, Consumer::Notify]);
    let secrets = build_secrets(&config(path.clone()), key(1)).expect("open with M1 subset");

    assert!(matches!(
        secrets.scoped(Consumer::Notify),
        Err(SecretsError::Denied {
            who: Consumer::Notify,
            ..
        })
    ));
    assert!(matches!(
        secrets.reader(Consumer::Notify),
        Err(SecretsError::Denied {
            who: Consumer::Notify,
            ..
        })
    ));
    assert!(matches!(
        secrets.scoped(Consumer::Egress),
        Err(SecretsError::Denied {
            who: Consumer::Egress,
            ..
        })
    ));
}

#[test]
fn forge_secrets_are_invisible_to_pki() {
    let dir = scratch();
    let path = dir.0.join("secrets");
    init(&path, &[Consumer::Forge, Consumer::Pki]);
    let secrets = build_secrets(&config(path.clone()), key(1)).expect("open secrets");

    let name = SecretName::parse("github.app.private_key").expect("secret name");
    let value = b"test-only-forge-sentinel".to_vec();
    secrets
        .scoped(Consumer::Forge)
        .expect("forge scope")
        .put(
            &name,
            SecretValue::from_bytes(value.clone()),
            Expect::Absent,
            &ctx(),
        )
        .expect("write forge secret");

    assert!(
        secrets
            .scoped(Consumer::Pki)
            .expect("pki scope")
            .get(&name)
            .expect("pki read")
            .is_none(),
        "pki namespace must not see a forge secret"
    );
    assert!(
        secrets
            .reader(Consumer::Pki)
            .expect("pki reader")
            .get(&name)
            .expect("pki read")
            .is_none(),
        "pki reader must not see a forge secret"
    );

    let found = secrets
        .scoped(Consumer::Forge)
        .expect("forge scope")
        .get(&name)
        .expect("forge read")
        .expect("forge secret exists");
    assert_eq!(found.value.expose(), value.as_slice());

    drop(secrets);
}
