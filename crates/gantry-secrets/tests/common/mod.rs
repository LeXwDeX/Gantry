//! 测试共用：临时目录、随机主密钥、`ActorCtx`。不含任何固定密钥。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::Ulid;
use gantry_core::ports::ActorCtx;
use gantry_core::secret::{Consumer, SecretName};
use gantry_secrets::{MasterKey, MasterKeySource, Secrets, SecretsConfig, StaticMasterKey};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "{}-{nanos:x}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("gantry-secrets-{tag}-{}", unique()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 一把随机主密钥，可多次取出同值副本。
pub struct Key(StaticMasterKey);

impl Key {
    pub fn random() -> Self {
        Self(StaticMasterKey::new(MasterKey::generate()))
    }

    pub fn source(&self) -> Box<dyn MasterKeySource> {
        Box::new(StaticMasterKey::new(self.0.load().unwrap()))
    }
}

pub fn cfg(dir: &Path, consumers: &[Consumer], key: &Key) -> SecretsConfig {
    SecretsConfig::new(dir, consumers.to_vec(), key.source())
}

/// 新建并打开一个实例；实例目录是 `<tmp>/inst`。
pub fn fresh(consumers: &[Consumer]) -> (TempDir, Key, Secrets) {
    let tmp = TempDir::new("inst");
    let key = Key::random();
    let dir = tmp.path().join("inst");
    Secrets::init(cfg(&dir, consumers, &key)).unwrap();
    let secrets = Secrets::open(cfg(&dir, consumers, &key)).unwrap();
    (tmp, key, secrets)
}

pub fn inst_dir(tmp: &TempDir) -> PathBuf {
    tmp.path().join("inst")
}

pub fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "secrets-test"),
        device: None,
        role: None,
        client_profile: None,
        request_id: Ulid::from_parts(1_700_000_000_000, 1).unwrap(),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

pub fn name(s: &str) -> SecretName {
    SecretName::parse(s).unwrap()
}

/// 测试现场生成的哨兵明文（每次调用不同）。
pub fn sentinel() -> String {
    format!("SENTINEL-{}", unique())
}

/// 在 `haystack` 中查找 `needle`。
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
