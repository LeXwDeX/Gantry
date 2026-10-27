//! 单个密钥文件的信封格式（secrets.md §7.2）。
//!
//! ```text
//! 0  4  魔数 "GSEC"        8  4  key_id
//! 4  1  格式版本 = 1        12 8  created_at（毫秒，大端）
//! 5  1  算法 = 1            20 24 nonce
//! 6  1  种类 1 密钥 / 2 墓碑 44 …  密文 ‖ 16 字节标签
//! 7  1  保留 = 0
//! AAD = 头部前 20 字节 ‖ consumer ‖ 0x00 ‖ name ‖ 0x00 ‖ version（u64 大端）
//! ```

use chacha20poly1305::aead::{AeadCore, AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use gantry_core::ports::secrets::CorruptReason;
use gantry_core::secret::{Consumer, SecretName};
use zeroize::Zeroizing;

use crate::key::{Dek, KeyId};

pub(crate) const MAGIC: &[u8; 4] = b"GSEC";
pub(crate) const FORMAT_V1: u8 = 1;
const ALG_XCHACHA20POLY1305: u8 = 1;
pub(crate) const HEADER_LEN: usize = 20;
const NONCE_LEN: usize = 24;
pub(crate) const TAG_LEN: usize = 16;
pub(crate) const PREFIX_LEN: usize = HEADER_LEN + NONCE_LEN;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Secret = 1,
    Tombstone = 2,
}

/// 解析头部的错误；由调用方补上路径转成 `SecretsError`。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnvelopeError {
    Corrupt(CorruptReason),
    Unsupported(u8),
    KeyMismatch,
}

pub(crate) struct Binding<'a> {
    pub consumer: Consumer,
    pub name: &'a SecretName,
    pub version: u64,
}

fn aad(header: &[u8], binding: &Binding<'_>) -> Vec<u8> {
    let consumer = binding.consumer.as_str().as_bytes();
    let name = binding.name.as_str().as_bytes();
    let mut aad = Vec::with_capacity(HEADER_LEN + consumer.len() + name.len() + 10);
    aad.extend_from_slice(&header[..HEADER_LEN]);
    aad.extend_from_slice(consumer);
    aad.push(0);
    aad.extend_from_slice(name);
    aad.push(0);
    aad.extend_from_slice(&binding.version.to_be_bytes());
    aad
}

/// 封装一个文件。`plaintext` 对墓碑为空。
pub(crate) fn seal(
    dek: &Dek,
    key_id: KeyId,
    kind: Kind,
    created_at_ms: i64,
    binding: &Binding<'_>,
    plaintext: &[u8],
) -> Vec<u8> {
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(MAGIC);
    header[4] = FORMAT_V1;
    header[5] = ALG_XCHACHA20POLY1305;
    header[6] = kind as u8;
    header[7] = 0;
    header[8..12].copy_from_slice(&key_id.0);
    header[12..20].copy_from_slice(&created_at_ms.to_be_bytes());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let cipher = XChaCha20Poly1305::new(dek.bytes().into());
    let mut buf = Zeroizing::new(Vec::with_capacity(plaintext.len() + TAG_LEN));
    buf.extend_from_slice(plaintext);
    cipher
        .encrypt_in_place(&nonce, &aad(&header, binding), &mut *buf)
        .expect("XChaCha20-Poly1305 encryption of a bounded buffer cannot fail");
    let mut out = Vec::with_capacity(PREFIX_LEN + buf.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    out
}

/// 只校验头部的静态字段并取出 `(kind, created_at)`，不解密。
pub(crate) fn peek(data: &[u8]) -> Result<(Kind, i64), EnvelopeError> {
    if data.len() < PREFIX_LEN + TAG_LEN || &data[..4] != MAGIC {
        return Err(EnvelopeError::Corrupt(CorruptReason::HeaderInvalid));
    }
    if data[4] != FORMAT_V1 {
        return Err(EnvelopeError::Unsupported(data[4]));
    }
    if data[5] != ALG_XCHACHA20POLY1305 || data[7] != 0 {
        return Err(EnvelopeError::Corrupt(CorruptReason::HeaderInvalid));
    }
    let kind = match data[6] {
        1 => Kind::Secret,
        2 => Kind::Tombstone,
        _ => return Err(EnvelopeError::Corrupt(CorruptReason::HeaderInvalid)),
    };
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&data[12..20]);
    Ok((kind, i64::from_be_bytes(ts)))
}

/// 校验头部、`key_id` 与种类，再做 AEAD 解密。失败不返回任何部分数据。
pub(crate) fn open(
    dek: &Dek,
    key_id: KeyId,
    expected_kind: Kind,
    binding: &Binding<'_>,
    data: &[u8],
) -> Result<(Zeroizing<Vec<u8>>, i64), EnvelopeError> {
    let (kind, created_at) = peek(data)?;
    if data[8..12] != key_id.0 {
        return Err(EnvelopeError::KeyMismatch);
    }
    if kind != expected_kind {
        return Err(EnvelopeError::Corrupt(CorruptReason::HeaderInvalid));
    }
    let nonce = XNonce::from_slice(&data[HEADER_LEN..PREFIX_LEN]);
    let cipher = XChaCha20Poly1305::new(dek.bytes().into());
    let body = &data[PREFIX_LEN..];
    let mut buf = Zeroizing::new(Vec::with_capacity(body.len()));
    buf.extend_from_slice(body);
    cipher
        .decrypt_in_place(nonce, &aad(&data[..HEADER_LEN], binding), &mut *buf)
        .map_err(|_| EnvelopeError::Corrupt(CorruptReason::AeadFailed))?;
    Ok((buf, created_at))
}
