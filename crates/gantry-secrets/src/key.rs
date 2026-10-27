//! 主密钥、主密钥来源与按使用方派生的数据密钥（DEK）。
//!
//! 层级见 secrets.md §7.3：
//! `DEK(consumer) = HKDF-SHA256(ikm = 主密钥, salt = instance_id, info = "gantry/secrets/v1/dek/" ‖ consumer)`，
//! `key_id = SHA-256("gantry/secrets/v1/keyid" ‖ 主密钥)[..4]`。

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::OsRng;
use gantry_core::ports::secrets::SecretsError;
use gantry_core::secret::Consumer;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const MASTER_KEY_LEN: usize = 32;
const KEY_ID_DOMAIN: &[u8] = b"gantry/secrets/v1/keyid";
const DEK_INFO_PREFIX: &[u8] = b"gantry/secrets/v1/dek/";

/// 实例主密钥（32 字节）。不可 `Clone`；`Debug` 不含值；丢弃时零化。
pub struct MasterKey(Zeroizing<[u8; MASTER_KEY_LEN]>);

impl MasterKey {
    pub fn from_bytes(bytes: [u8; MASTER_KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// 从操作系统随机源生成。主密钥的发放归 `deploy`；本函数供安装脚本与测试使用。
    pub fn generate() -> Self {
        let mut key = Zeroizing::new([0u8; MASTER_KEY_LEN]);
        OsRng.fill_bytes(key.as_mut());
        Self(key)
    }

    pub(crate) fn key_id(&self) -> KeyId {
        let mut hasher = Sha256::new();
        hasher.update(KEY_ID_DOMAIN);
        hasher.update(self.0.as_ref());
        let digest = hasher.finalize();
        KeyId([digest[0], digest[1], digest[2], digest[3]])
    }

    pub(crate) fn derive_dek(&self, instance_id: &str, consumer: Consumer) -> Dek {
        let hk = Hkdf::<Sha256>::new(Some(instance_id.as_bytes()), self.0.as_ref());
        let mut info = Vec::with_capacity(DEK_INFO_PREFIX.len() + 8);
        info.extend_from_slice(DEK_INFO_PREFIX);
        info.extend_from_slice(consumer.as_str().as_bytes());
        let mut dek = Zeroizing::new([0u8; 32]);
        hk.expand(&info, dek.as_mut())
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Dek(dek)
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(***)")
    }
}

/// 主密钥标识：写进 `secrets.meta` 与每个文件头，用于在 AEAD 之前先报 `KeyMismatch`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct KeyId(pub(crate) [u8; 4]);

impl KeyId {
    pub(crate) fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// 一个使用方的数据加密密钥。
pub(crate) struct Dek(Zeroizing<[u8; 32]>);

impl Dek {
    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Dek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Dek(***)")
    }
}

/// 主密钥来源。由 `deploy` 选择具体实现（OQ-2）。
pub trait MasterKeySource: Send + Sync {
    fn load(&self) -> Result<MasterKey, SecretsError>;
}

/// systemd credential：读 `$CREDENTIALS_DIRECTORY/<name>`，内容为 32 字节原始密钥。
#[derive(Clone, Debug)]
pub struct SystemdCredential {
    pub name: String,
}

impl MasterKeySource for SystemdCredential {
    fn load(&self) -> Result<MasterKey, SecretsError> {
        let dir = std::env::var_os("CREDENTIALS_DIRECTORY").ok_or_else(|| {
            SecretsError::KeyUnavailable("CREDENTIALS_DIRECTORY is not set".into())
        })?;
        if self.name.is_empty() || self.name.contains('/') || self.name.starts_with('.') {
            return Err(SecretsError::KeyUnavailable(
                "invalid systemd credential name".into(),
            ));
        }
        read_key_file(&PathBuf::from(dir).join(&self.name), false)
    }
}

/// 本地密钥文件：32 字节原始密钥，属主 = 有效 uid、组与其他用户无任何权限（建议 `0400`），
/// 必须位于 `secrets.dir` 之外（`Secrets::open` 检查）。
#[derive(Clone, Debug)]
pub struct KeyFile {
    pub path: PathBuf,
}

impl MasterKeySource for KeyFile {
    fn load(&self) -> Result<MasterKey, SecretsError> {
        read_key_file(&self.path, true)
    }
}

/// 测试与组装代码用：直接持有一把已加载的主密钥，每次 `load` 返回同值的新副本。
pub struct StaticMasterKey(MasterKey);

impl StaticMasterKey {
    pub fn new(key: MasterKey) -> Self {
        Self(key)
    }
}

impl fmt::Debug for StaticMasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticMasterKey(***)")
    }
}

impl MasterKeySource for StaticMasterKey {
    fn load(&self) -> Result<MasterKey, SecretsError> {
        Ok(MasterKey::from_bytes(*self.0 .0))
    }
}

fn read_key_file(path: &std::path::Path, check_owner: bool) -> Result<MasterKey, SecretsError> {
    let file = File::options()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|error| {
            SecretsError::KeyUnavailable(format!("cannot open master key file: {}", error.kind()))
        })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(SecretsError::KeyUnavailable(
            "master key path is not a regular file".into(),
        ));
    }
    if check_owner {
        let mode = meta.mode() & 0o7777;
        if meta.uid() != rustix::process::geteuid().as_raw() || mode & 0o077 != 0 {
            return Err(SecretsError::InsecurePermissions {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    let mut buf = Zeroizing::new(Vec::with_capacity(MASTER_KEY_LEN + 1));
    file.take(MASTER_KEY_LEN as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() != MASTER_KEY_LEN {
        return Err(SecretsError::KeyUnavailable(format!(
            "master key must be exactly {MASTER_KEY_LEN} bytes"
        )));
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    key.copy_from_slice(&buf);
    let master = MasterKey::from_bytes(key);
    zeroize::Zeroize::zeroize(&mut key);
    Ok(master)
}
