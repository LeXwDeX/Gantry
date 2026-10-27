//! 安装时引导（pki.md §6.6，`gantry pki bootstrap-ca`，命令名 [提议]）。
//!
//! 在 `out_dir`（必须是内存盘：tmpfs / ramfs）里一次产出：
//! `root.key.enc`（口令加密 PKCS#8）、`root.crt`、`intermediate.key`、`intermediate.crt`、
//! `server.key`、`server.crt`。根私钥只在内存里生成与加密，不写其它位置，返回前丢弃（零化）。
//! 之后的导出、清除内存盘、把私钥交给 `secrets` 由 `deploy` 编排（deploy.md §7.3）。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use gantry_core::ids::{Clock, Entropy};
use gantry_core::secret::SecretString;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use zeroize::Zeroizing;

use crate::ca::Authority;
use crate::config::CertPolicy;
use crate::rng::EntropyRng;
use crate::PkiError;

pub const ROOT_KEY_ENC: &str = "root.key.enc";
pub const ROOT_CRT: &str = "root.crt";
pub const INTERMEDIATE_KEY: &str = "intermediate.key";
pub const INTERMEDIATE_CRT: &str = "intermediate.crt";
pub const SERVER_KEY: &str = "server.key";
pub const SERVER_CRT: &str = "server.crt";

/// 全部产出文件名，按写入顺序。
pub const OUTPUTS: [&str; 6] = [
    ROOT_KEY_ENC,
    ROOT_CRT,
    INTERMEDIATE_KEY,
    INTERMEDIATE_CRT,
    SERVER_KEY,
    SERVER_CRT,
];

pub struct BootstrapParams {
    /// 服务端叶子证书的 SAN（DNS 名 / IP）。
    pub server_names: Vec<String>,
    /// 加密 `root.key.enc` 的口令。
    pub root_passphrase: SecretString,
    pub install_id: String,
    pub out_dir: PathBuf,
    pub policy: CertPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapReport {
    /// `"sha256:"` + 根证书 DER 的 SHA-256（配对串里的 `ca_fingerprint`）。
    pub root_fingerprint: String,
    pub intermediate_serial: String,
    pub server_cert_serial: String,
}

/// 判断目录是否在内存文件系统上。生产用 [`StatfsProbe`]；测试可替换。
pub trait MemoryFsProbe {
    fn is_memory_fs(&self, dir: &Path) -> io::Result<bool>;
}

/// Linux 上用 `statfs` 的 `f_type` 判断 tmpfs / ramfs；其它平台一律视为否。
pub struct StatfsProbe;

impl MemoryFsProbe for StatfsProbe {
    #[cfg(target_os = "linux")]
    fn is_memory_fs(&self, dir: &Path) -> io::Result<bool> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        const TMPFS_MAGIC: i64 = 0x0102_1994;
        const RAMFS_MAGIC: i64 = 0x8584_58f6;

        let path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` 是以 NUL 结尾的有效 C 字符串，`stat` 指向足够大的可写内存。
        let rc = unsafe { libc::statfs(path.as_ptr(), &mut stat) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        let kind = stat.f_type as i64;
        Ok(kind == TMPFS_MAGIC || kind == RAMFS_MAGIC)
    }

    #[cfg(not(target_os = "linux"))]
    fn is_memory_fs(&self, _dir: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

/// 生产入口：`out_dir` 必须是 tmpfs / ramfs。
pub fn run(
    params: BootstrapParams,
    clock: &dyn Clock,
    entropy: &dyn Entropy,
) -> Result<BootstrapReport, PkiError> {
    run_with(params, clock, entropy, &StatfsProbe)
}

/// 与 [`run`] 相同，但内存盘判断可替换（非 Linux 的开发机测试用）。
pub fn run_with(
    params: BootstrapParams,
    clock: &dyn Clock,
    entropy: &dyn Entropy,
    probe: &dyn MemoryFsProbe,
) -> Result<BootstrapReport, PkiError> {
    if params.install_id.is_empty() || params.install_id.chars().any(char::is_control) {
        return Err(PkiError::Config(
            "install_id must be non-empty printable text",
        ));
    }
    if params.root_passphrase.is_empty() {
        return Err(PkiError::Config("root_passphrase must not be empty"));
    }
    let out_dir = &params.out_dir;
    if !out_dir.is_dir() {
        return Err(PkiError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{out_dir:?} is not a directory"),
        )));
    }
    if !probe.is_memory_fs(out_dir)? {
        return Err(PkiError::NotMemoryFs(out_dir.clone()));
    }
    for name in OUTPUTS {
        let path = out_dir.join(name);
        if path.symlink_metadata().is_ok() {
            return Err(PkiError::OutputExists(path));
        }
    }

    let now = clock.now();
    let policy = &params.policy;
    let (root, root_secret) = Authority::new_root(&params.install_id, now, policy, entropy)?;
    let root_key_enc = Zeroizing::new(
        root_secret
            .to_pkcs8_encrypted_pem(
                &mut EntropyRng(entropy),
                params.root_passphrase.expose_secret().as_bytes(),
                LineEnding::LF,
            )
            .map_err(|error| PkiError::Crypto(format!("encrypt root key: {error}")))?
            .to_string(),
    );
    drop(root_secret);

    let (intermediate, intermediate_key) = root.issue_intermediate(1, now, policy, entropy)?;
    let (server, server_key) =
        intermediate.issue_server_leaf(&params.server_names, now, policy, entropy)?;

    // 证书可公开；私钥文件统一 0600、只新建不覆盖。
    write_new(out_dir, ROOT_KEY_ENC, root_key_enc.as_bytes())?;
    write_new(out_dir, ROOT_CRT, root.certificate().pem.as_bytes())?;
    write_new(out_dir, INTERMEDIATE_KEY, intermediate_key.as_bytes())?;
    write_new(
        out_dir,
        INTERMEDIATE_CRT,
        intermediate.certificate().pem.as_bytes(),
    )?;
    write_new(out_dir, SERVER_KEY, server_key.as_bytes())?;
    write_new(out_dir, SERVER_CRT, server.pem.as_bytes())?;

    Ok(BootstrapReport {
        root_fingerprint: root.certificate().fingerprint().to_string(),
        intermediate_serial: intermediate.certificate().serial.to_string(),
        server_cert_serial: server.serial.to_string(),
    })
}

fn write_new(dir: &Path, name: &str, contents: &[u8]) -> Result<(), PkiError> {
    let path = dir.join(name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            PkiError::OutputExists(path.clone())
        } else {
            PkiError::Io(error)
        }
    })?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}
