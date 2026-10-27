//! 证书与配对（`pki`，docs/modules/pki.md）。
//!
//! 本 crate 按 M1 切片落地。当前包含：
//!
//! - [`ca`]：信任层次（离线根 → 在线中间 CA → 服务端 / 设备叶子）与证书参数（§7.1）；
//! - [`bootstrap`]：安装时一次性产出根 / 中间 / 服务端材料（§6.6）；
//! - [`keys`]：P-256 公钥、指纹与 [`CertSerial`]；
//! - [`csr`]：不可信 CSR 与 `approval_pubkey` 的校验（§7.4.3、§7.4.4）；
//! - [`text`]：设备名校验与终端输出净化（§8 终端注入）；
//! - [`journal`]：吊销日志 `revocations.journal` 的追加、复制与重放筛选（§7.6，P-67、P-80）。
//!
//! 设备注册表、配对、握手校验、吊销等依赖 `store` 的部分在后续切片中加入。

pub mod bootstrap;
pub mod ca;
pub mod config;
pub mod csr;
pub mod journal;
pub mod keys;
mod rng;
pub mod text;

pub use ca::{Authority, IssuedCert};
pub use config::CertPolicy;
pub use keys::{fingerprint, CertSerial, KeyError, P256PublicKey};

use std::path::PathBuf;

/// `pki` 的通用错误。外部表现由调用方（`api`、`deploy`）映射；细节只进日志。
#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("certificate operation failed: {0}")]
    Crypto(String),
    #[error("invalid key material: {0}")]
    Key(#[from] KeyError),
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    #[error("{0:?} is not on a memory filesystem (tmpfs / ramfs)")]
    NotMemoryFs(PathBuf),
    #[error("refusing to overwrite existing {0:?}")]
    OutputExists(PathBuf),
    #[error("timestamp out of range for X.509")]
    TimeRange,
    #[error("pki I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<rcgen::Error> for PkiError {
    fn from(error: rcgen::Error) -> Self {
        Self::Crypto(error.to_string())
    }
}
