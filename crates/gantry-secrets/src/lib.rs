//! Gantry 密钥存储（secrets.md）。
//!
//! 每个进程（`gantryd`、`egress`）各开一个实例：目录、操作系统用户、主密钥都不同。
//! 实例内按使用方（`Consumer`）派生不同的数据密钥，业务模块只拿到绑定单个使用方的
//! [`SecretReader`]（只读）或 [`ScopedSecrets`]（读写）；跨使用方的管理操作只在
//! [`SecretAdmin`] 上。
//!
//! 本库**没有**任何把值导出到环境变量、挂载或文件的 API（I1）；值只以
//! `SecretValue` 在进程内传递（I6）。同步 API，不依赖 `tokio`、`store` 或网络。

//!
//! 服务端只跑在 Unix（Ubuntu）上：非 Unix 平台本 crate 为空，以便工作区在 Windows 上照常编译。

#![cfg(unix)]
#![forbid(unsafe_code)]

mod backup;
mod envelope;
mod fsx;
mod key;
mod namespace;

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gantry_core::ids::Ulid;
use gantry_core::ports::secrets::{
    Expect, PutOutcome, Secret, SecretMeta, SecretSink, SecretSource, SecretValue, SecretVersion,
    SecretsError, StagedSecret,
};
use gantry_core::ports::ActorCtx;
use gantry_core::secret::{Consumer, SecretName};
use serde::{Deserialize, Serialize};

pub use backup::{
    BackupEntry, BackupEntryKind, BackupIdentity, BackupManifest, BackupRecipient, ImportMode,
    ImportReport,
};
pub use key::{KeyFile, MasterKey, MasterKeySource, StaticMasterKey, SystemdCredential};

use namespace::{Namespace, Root};

/// 默认保留的已提交版本数（secrets.md §10，建议值）。
pub const DEFAULT_RETAIN_VERSIONS: u32 = 2;
/// 单个值的默认上限（secrets.md §10，建议值）。
pub const DEFAULT_MAX_VALUE_BYTES: usize = 65536;

const META_FORMAT: u32 = 1;
const DENIED_NAME: &str = "instance";

/// 实例配置（引导配置，归 `deploy`）。
pub struct SecretsConfig {
    pub dir: PathBuf,
    /// 本实例承载的使用方。
    pub consumers: Vec<Consumer>,
    pub master_key: Box<dyn MasterKeySource>,
    pub retain_versions: u32,
    pub max_value_bytes: usize,
}

impl SecretsConfig {
    pub fn new(
        dir: impl Into<PathBuf>,
        consumers: Vec<Consumer>,
        master_key: Box<dyn MasterKeySource>,
    ) -> Self {
        Self {
            dir: dir.into(),
            consumers,
            master_key,
            retain_versions: DEFAULT_RETAIN_VERSIONS,
            max_value_bytes: DEFAULT_MAX_VALUE_BYTES,
        }
    }
}

impl fmt::Debug for SecretsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsConfig")
            .field("dir", &self.dir)
            .field("consumers", &self.consumers)
            .field("retain_versions", &self.retain_versions)
            .field("max_value_bytes", &self.max_value_bytes)
            .finish_non_exhaustive()
    }
}

/// `secrets.meta`：明文、无机密。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceMeta {
    format: u32,
    instance_id: String,
    key_id: String,
    consumers: Vec<Consumer>,
    created_at: i64,
}

fn denied(who: Consumer) -> SecretsError {
    SecretsError::Denied {
        who,
        name: SecretName::parse(DENIED_NAME).expect("static name is valid"),
    }
}

fn invalid(msg: &str) -> SecretsError {
    SecretsError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        msg.to_owned(),
    ))
}

/// 一个已打开的实例。持有本实例全部使用方的 DEK，但不持有主密钥；只应由组装代码持有。
pub struct Secrets {
    root: Arc<Root>,
    deks: BTreeMap<Consumer, Arc<key::Dek>>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secrets")
            .field("dir", &self.root.dir)
            .field("consumers", &self.deks.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Secrets {
    /// 创建实例目录（0700）、`secrets.meta`、写锁文件与各使用方目录。目录已存在则拒绝。
    pub fn init(cfg: SecretsConfig) -> Result<(), SecretsError> {
        if cfg.consumers.is_empty() {
            return Err(invalid("an instance must host at least one consumer"));
        }
        let master = cfg.master_key.load()?;
        fsx::create_dir(&cfg.dir)?;
        let now = namespace::now_ms();
        let mut random = [0u8; 10];
        chacha20poly1305::aead::rand_core::RngCore::fill_bytes(
            &mut chacha20poly1305::aead::OsRng,
            &mut random,
        );
        let mut random80 = 0u128;
        for b in random {
            random80 = (random80 << 8) | u128::from(b);
        }
        let instance_id = Ulid::from_parts(now.max(0) as u64, random80)
            .map_err(|_| invalid("cannot build instance id"))?
            .to_string();
        let mut consumers = cfg.consumers.clone();
        consumers.sort();
        consumers.dedup();
        let meta = InstanceMeta {
            format: META_FORMAT,
            instance_id,
            key_id: master.key_id().to_hex(),
            consumers: consumers.clone(),
            created_at: now,
        };
        let json = serde_json::to_vec_pretty(&meta).map_err(std::io::Error::other)?;
        fsx::write_new(&cfg.dir, fsx::LOCK_FILE, &[])?;
        for consumer in &consumers {
            fsx::create_dir(&cfg.dir.join(consumer.as_str()))?;
        }
        fsx::write_new(&cfg.dir, fsx::META_FILE, &json)?;
        tracing::info!(dir = %cfg.dir.display(), op = "init", result = "ok");
        Ok(())
    }

    /// 打开已有实例：校验权限、格式与 `key_id`，为承载的使用方派生 DEK 后丢弃主密钥。
    /// 不创建、不修复任何东西；仅清理崩溃遗留的临时文件。
    pub fn open(cfg: SecretsConfig) -> Result<Secrets, SecretsError> {
        Self::open_as(cfg, fsx::euid())
    }

    fn open_as(cfg: SecretsConfig, uid: u32) -> Result<Secrets, SecretsError> {
        if let Err(error) = fsx::check_tree(&cfg.dir, uid) {
            tracing::error!(dir = %cfg.dir.display(), error = %error, op = "open", result = "insecure");
            return Err(error);
        }
        let raw = fsx::read_file(&cfg.dir.join(fsx::META_FILE), 64 * 1024)?
            .ok_or_else(|| invalid("secrets.meta is missing"))?;
        let meta: InstanceMeta =
            serde_json::from_slice(&raw).map_err(|_| SecretsError::Corrupt {
                path: cfg.dir.join(fsx::META_FILE),
                reason: gantry_core::ports::secrets::CorruptReason::HeaderInvalid,
            })?;
        if meta.format != META_FORMAT {
            return Err(SecretsError::UnsupportedFormat {
                found: u8::try_from(meta.format).unwrap_or(u8::MAX),
            });
        }
        let master = cfg.master_key.load()?;
        let key_id = master.key_id();
        if key_id.to_hex() != meta.key_id {
            tracing::error!(dir = %cfg.dir.display(), op = "open", result = "key_mismatch");
            return Err(SecretsError::KeyMismatch);
        }
        let mut deks = BTreeMap::new();
        for consumer in &cfg.consumers {
            if !meta.consumers.contains(consumer) {
                return Err(invalid(
                    "configured consumer is not hosted by this instance",
                ));
            }
            deks.insert(
                *consumer,
                Arc::new(master.derive_dek(&meta.instance_id, *consumer)),
            );
        }
        drop(master);
        let root = Arc::new(Root {
            dir: cfg.dir.clone(),
            instance_id: meta.instance_id,
            key_id,
            retain: cfg.retain_versions.max(1) as usize,
            max_value: cfg.max_value_bytes,
        });
        let secrets = Secrets { root, deks };
        secrets.recover()?;
        tracing::info!(dir = %cfg.dir.display(), op = "open", result = "ok");
        Ok(secrets)
    }

    /// 启动清理：删除 `.tmp.*`；`commit` 在硬链接后、删除暂存名前崩溃时，同版本的
    /// `.staged` 与 `.sec` 并存，删除多余的 `.staged`（提交已完成）。其余暂存项原样保留。
    fn recover(&self) -> Result<(), SecretsError> {
        let _lock = fsx::lock(&self.root.dir)?;
        fsx::remove_tmp_files(&self.root.dir)?;
        for consumer in self.deks.keys() {
            let dir = self.root.dir.join(consumer.as_str());
            fsx::remove_tmp_files(&dir)?;
            let entries = fsx::list_entries(&dir)?;
            for (name, v, kind) in &entries {
                let committed = entries.contains(&(name.clone(), *v, fsx::FileKind::Sec));
                if *kind == fsx::FileKind::Staged && committed {
                    std::fs::remove_file(dir.join(fsx::file_name(name, *v, *kind)))?;
                }
            }
        }
        Ok(())
    }

    fn namespace(&self, who: Consumer) -> Result<Namespace, SecretsError> {
        let dek = self.deks.get(&who).ok_or_else(|| {
            tracing::warn!(consumer = who.as_str(), op = "handle", result = "denied");
            denied(who)
        })?;
        Ok(Namespace {
            root: Arc::clone(&self.root),
            who,
            dek: Arc::clone(dek),
        })
    }

    /// 只读句柄。实例未承载该使用方 → `Denied`。
    pub fn reader(&self, who: Consumer) -> Result<SecretReader, SecretsError> {
        Ok(SecretReader(self.namespace(who)?))
    }

    /// 读写句柄。实例未承载该使用方 → `Denied`。
    pub fn scoped(&self, who: Consumer) -> Result<ScopedSecrets, SecretsError> {
        Ok(ScopedSecrets(self.namespace(who)?))
    }

    /// 跨使用方的管理句柄，只交给组装代码与 `deploy` 的入口。
    pub fn admin(&self) -> SecretAdmin {
        SecretAdmin {
            namespaces: self
                .deks
                .iter()
                .map(|(who, dek)| Namespace {
                    root: Arc::clone(&self.root),
                    who: *who,
                    dek: Arc::clone(dek),
                })
                .collect(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.root.dir
    }
}

/// 只读句柄：只含一个使用方的 DEK。
pub struct SecretReader(Namespace);

/// 读写句柄：只含一个使用方的 DEK；写入只落在自己的命名空间。
pub struct ScopedSecrets(Namespace);

impl fmt::Debug for SecretReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretReader")
            .field("consumer", &self.0.who)
            .finish()
    }
}

impl fmt::Debug for ScopedSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedSecrets")
            .field("consumer", &self.0.who)
            .finish()
    }
}

macro_rules! impl_source {
    ($ty:ty) => {
        impl SecretSource for $ty {
            fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError> {
                self.0.get(name)
            }

            fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError> {
                self.0.version(name)
            }

            fn present(&self, name: &SecretName) -> Result<bool, SecretsError> {
                Ok(self.0.get(name)?.is_some())
            }

            fn get_version(
                &self,
                name: &SecretName,
                version: SecretVersion,
            ) -> Result<Option<Secret>, SecretsError> {
                self.0.get_version(name, version)
            }
        }
    };
}

impl_source!(SecretReader);
impl_source!(ScopedSecrets);

impl SecretSink for ScopedSecrets {
    fn put(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        _ctx: &ActorCtx,
    ) -> Result<PutOutcome, SecretsError> {
        self.0.put(name, value, expect)
    }

    fn stage(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        _ctx: &ActorCtx,
    ) -> Result<StagedSecret, SecretsError> {
        self.0.stage(name, value, expect)
    }

    fn commit(&self, staged: &StagedSecret, _ctx: &ActorCtx) -> Result<SecretMeta, SecretsError> {
        self.0.commit(staged)
    }

    fn abort(&self, staged: &StagedSecret, _ctx: &ActorCtx) -> Result<(), SecretsError> {
        self.0.abort(staged)
    }

    fn staged(&self) -> Result<Vec<StagedSecret>, SecretsError> {
        self.0.staged()
    }

    fn delete(
        &self,
        name: &SecretName,
        expect: Expect,
        _ctx: &ActorCtx,
    ) -> Result<Option<SecretMeta>, SecretsError> {
        self.0.delete(name, expect)
    }

    fn list(&self) -> Result<Vec<SecretMeta>, SecretsError> {
        self.0.list()
    }

    fn check_refs(&self, refs: &[SecretName]) -> Result<Vec<SecretName>, SecretsError> {
        self.0.check_refs(refs)
    }
}

/// 跨使用方的管理操作：列出全部暂存项、加密备份与恢复（`rewrap` 属 M3）。
pub trait SecretAdminOps: Send + Sync {
    fn staged_all(&self) -> Result<Vec<StagedSecret>, SecretsError>;

    /// 把本实例每个使用方的当前版本与墓碑导出为一个 age 加密流（§7.6）。
    fn export_backup(
        &self,
        to: &[BackupRecipient],
        out: &mut dyn Write,
    ) -> Result<BackupManifest, SecretsError>;

    /// 先完整解密校验，再以原版本号、本机主密钥重新加密写入。
    fn import_backup(
        &self,
        id: &BackupIdentity,
        input: &mut dyn Read,
        mode: ImportMode,
    ) -> Result<ImportReport, SecretsError>;
}

/// 管理句柄：持有本实例全部使用方的命名空间。
pub struct SecretAdmin {
    namespaces: Vec<Namespace>,
}

impl fmt::Debug for SecretAdmin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretAdmin")
            .field(
                "consumers",
                &self.namespaces.iter().map(|ns| ns.who).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SecretAdminOps for SecretAdmin {
    fn staged_all(&self) -> Result<Vec<StagedSecret>, SecretsError> {
        let mut out = Vec::new();
        for ns in &self.namespaces {
            out.extend(ns.staged()?);
        }
        Ok(out)
    }

    fn export_backup(
        &self,
        to: &[BackupRecipient],
        out: &mut dyn Write,
    ) -> Result<BackupManifest, SecretsError> {
        let instance_id = self
            .namespaces
            .first()
            .map(|ns| ns.root.instance_id.clone())
            .unwrap_or_default();
        backup::export(&self.namespaces, &instance_id, to, out)
    }

    fn import_backup(
        &self,
        id: &BackupIdentity,
        input: &mut dyn Read,
        mode: ImportMode,
    ) -> Result<ImportReport, SecretsError> {
        backup::import(&self.namespaces, id, input, mode)
    }
}

/// 进程加固：`RLIMIT_CORE = 0`；Linux 上另设 `PR_SET_DUMPABLE = 0`。
/// `gantryd` 与 `egress` 的 `main` 在读取任何密钥之前调用。
pub fn harden_process() -> Result<(), SecretsError> {
    use rustix::process::{setrlimit, Resource, Rlimit};
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(std::io::Error::from)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(std::io::Error::from)?;
    Ok(())
}

// 编译期检查：句柄实现端口 trait。
const _: fn() = || {
    fn assert_source<T: SecretSource>() {}
    fn assert_sink<T: SecretSink>() {}
    assert_source::<SecretReader>();
    assert_sink::<ScopedSecrets>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_rejects_foreign_owner() {
        let dir = std::env::temp_dir().join(format!("gantry-secrets-uid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = MasterKey::generate();
        let bytes = StaticMasterKey::new(key);
        let cfg = |d: &Path| {
            SecretsConfig::new(
                d,
                vec![Consumer::Forge],
                Box::new(StaticMasterKey::new(bytes.load().unwrap())),
            )
        };
        Secrets::init(cfg(&dir)).unwrap();
        assert!(Secrets::open_as(cfg(&dir), fsx::euid()).is_ok());
        let foreign = fsx::euid().wrapping_add(1);
        assert!(matches!(
            Secrets::open_as(cfg(&dir), foreign),
            Err(SecretsError::InsecurePermissions { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
