//! Windows TPM 后端（CNG：Microsoft Platform Crypto Provider，desktop-shell.md §1 第 1 条、§6.5、OQ-S10）。
//!
//! - 两把 ECDSA P-256 持久化密钥都在 TPM 中生成，`NCRYPT_EXPORT_POLICY = 0`（不可导出）。
//! - mTLS 客户端密钥：`NCRYPT_SILENT_FLAG` 使用，永不弹框。
//! - 审批签名密钥：建键时设 `NCRYPT_UI_POLICY`（`PROTECT_KEY | FORCE_HIGH_PROTECTION`），每次使用由系统
//!   要求用户确认（Windows Hello / PIN）；签名前用 `NCRYPT_USE_CONTEXT_PROPERTY` 写入由 payload 生成的文案。
//!
//! **实机待验证**（M1.4，#107）：本机与 CI 都没有带 TPM 2.0 + Windows Hello 的 Windows 设备，
//! 这里只保证 `x86_64-pc-windows-msvc` 能编译；"文案是否与密钥使用绑定"见 OQ-S10。
//! CNG 的 ECDSA 签名格式本身就是 P1363（`r ‖ s`），mTLS 路径再转 DER。

use sha2::{Digest, Sha256};
use windows_sys::core::HRESULT;
use windows_sys::Win32::Foundation::{NTE_BAD_KEYSET, NTE_USER_CANCELLED};
use windows_sys::Win32::Security::Cryptography::{
    NCryptCreatePersistedKey, NCryptDeleteKey, NCryptExportKey, NCryptFinalizeKey,
    NCryptFreeObject, NCryptOpenKey, NCryptOpenStorageProvider, NCryptSetProperty, NCryptSignHash,
    BCRYPT_ECCPUBLIC_BLOB, MS_PLATFORM_CRYPTO_PROVIDER, NCRYPT_ECDSA_P256_ALGORITHM,
    NCRYPT_EXPORT_POLICY_PROPERTY, NCRYPT_KEY_HANDLE, NCRYPT_PROV_HANDLE, NCRYPT_SILENT_FLAG,
    NCRYPT_UI_FORCE_HIGH_PROTECTION_FLAG, NCRYPT_UI_POLICY, NCRYPT_UI_POLICY_PROPERTY,
    NCRYPT_UI_PROTECT_KEY_FLAG, NCRYPT_USE_CONTEXT_PROPERTY,
};

use super::{
    p1363_to_der, DeviceKeys, KeyBackend, KeyError, KeyLabels, KeyStore, P1363Sig, P256PublicKey,
    PresencePrompt,
};

/// `BCRYPT_ECCKEY_BLOB` 头：`dwMagic`、`cbKey` 各 4 字节，随后是 X、Y。
const ECC_BLOB_HEADER: usize = 8;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hr_error(hr: HRESULT) -> KeyError {
    match hr {
        NTE_BAD_KEYSET => KeyError::KeyUnavailable,
        NTE_USER_CANCELLED => KeyError::UserCancelled,
        _ => KeyError::Backend(format!("CNG 0x{:08x}", hr as u32)),
    }
}

fn check(hr: HRESULT) -> Result<(), KeyError> {
    if hr == 0 {
        Ok(())
    } else {
        Err(hr_error(hr))
    }
}

/// 自动释放的 CNG 句柄。
struct Handle(usize);

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                NCryptFreeObject(self.0);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Usage {
    Tls,
    Approval,
}

pub struct TpmKeyStore;

impl TpmKeyStore {
    /// 打不开平台加密提供程序（没有 TPM 或被禁用）→ `HardwareUnavailable`，不降级（OQ-S2）。
    pub fn open() -> Result<Self, KeyError> {
        provider().map(|_| Self)
    }
}

fn provider() -> Result<Handle, KeyError> {
    let mut prov: NCRYPT_PROV_HANDLE = 0;
    let hr = unsafe { NCryptOpenStorageProvider(&mut prov, MS_PLATFORM_CRYPTO_PROVIDER, 0) };
    if hr != 0 {
        return Err(KeyError::HardwareUnavailable(format!(
            "Microsoft Platform Crypto Provider 不可用（0x{:08x}）",
            hr as u32
        )));
    }
    Ok(Handle(prov))
}

fn open_key(name: &str) -> Result<Handle, KeyError> {
    let prov = provider()?;
    let name = wide(name);
    let mut key: NCRYPT_KEY_HANDLE = 0;
    check(unsafe { NCryptOpenKey(prov.0, &mut key, name.as_ptr(), 0, NCRYPT_SILENT_FLAG) })?;
    Ok(Handle(key))
}

fn create_key(name: &str, usage: Usage) -> Result<(), KeyError> {
    let prov = provider()?;
    let wname = wide(name);
    let mut key: NCRYPT_KEY_HANDLE = 0;
    check(unsafe {
        NCryptCreatePersistedKey(
            prov.0,
            &mut key,
            NCRYPT_ECDSA_P256_ALGORITHM,
            wname.as_ptr(),
            0,
            0,
        )
    })?;
    let key = Handle(key);
    // 不可导出（S-1）。
    let export_policy: u32 = 0;
    check(unsafe {
        NCryptSetProperty(
            key.0,
            NCRYPT_EXPORT_POLICY_PROPERTY,
            (&export_policy as *const u32).cast(),
            4,
            0,
        )
    })?;
    if let Usage::Approval = usage {
        // 每次使用都要用户确认（P-47）。
        let title = wide("Gantry 审批签名密钥");
        let description = wide("用于签署 Gantry 审批；每次使用都需要你确认。");
        let policy = NCRYPT_UI_POLICY {
            dwVersion: 1,
            dwFlags: NCRYPT_UI_PROTECT_KEY_FLAG | NCRYPT_UI_FORCE_HIGH_PROTECTION_FLAG,
            pszCreationTitle: title.as_ptr(),
            pszFriendlyName: title.as_ptr(),
            pszDescription: description.as_ptr(),
        };
        check(unsafe {
            NCryptSetProperty(
                key.0,
                NCRYPT_UI_POLICY_PROPERTY,
                (&policy as *const NCRYPT_UI_POLICY).cast(),
                std::mem::size_of::<NCRYPT_UI_POLICY>() as u32,
                0,
            )
        })?;
    }
    check(unsafe { NCryptFinalizeKey(key.0, 0) })
}

fn public_key(key: &Handle) -> Result<P256PublicKey, KeyError> {
    let mut len: u32 = 0;
    check(unsafe {
        NCryptExportKey(
            key.0,
            0,
            BCRYPT_ECCPUBLIC_BLOB,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
            &mut len,
            0,
        )
    })?;
    let mut blob = vec![0u8; len as usize];
    check(unsafe {
        NCryptExportKey(
            key.0,
            0,
            BCRYPT_ECCPUBLIC_BLOB,
            std::ptr::null(),
            blob.as_mut_ptr(),
            len,
            &mut len,
            0,
        )
    })?;
    if blob.len() != ECC_BLOB_HEADER + 64 {
        return Err(KeyError::Backend("ECC 公钥块长度不符".into()));
    }
    let mut point = vec![0x04];
    point.extend_from_slice(&blob[ECC_BLOB_HEADER..]);
    P256PublicKey::from_uncompressed(&point)
}

/// 对 SHA-256 摘要签名，返回 P1363。
fn sign_hash(key: &Handle, msg: &[u8], flags: u32) -> Result<[u8; 64], KeyError> {
    let hash = Sha256::digest(msg);
    let mut sig = [0u8; 64];
    let mut len: u32 = 0;
    check(unsafe {
        NCryptSignHash(
            key.0,
            std::ptr::null(),
            hash.as_ptr(),
            hash.len() as u32,
            sig.as_mut_ptr(),
            64,
            &mut len,
            flags,
        )
    })?;
    if len != 64 {
        return Err(KeyError::Backend("签名长度不符".into()));
    }
    Ok(sig)
}

impl KeyStore for TpmKeyStore {
    fn backend(&self) -> KeyBackend {
        KeyBackend::Tpm
    }

    fn generate(&self) -> Result<Box<dyn DeviceKeys>, KeyError> {
        let labels = KeyLabels::random()?;
        let result = create_key(&labels.tls, Usage::Tls)
            .and_then(|_| create_key(&labels.approval, Usage::Approval))
            .and_then(|_| self.load(&labels));
        if result.is_err() {
            let _ = self.delete(&labels);
        }
        result
    }

    fn load(&self, labels: &KeyLabels) -> Result<Box<dyn DeviceKeys>, KeyError> {
        labels.validate()?;
        let tls = open_key(&labels.tls)?;
        let approval = open_key(&labels.approval)?;
        Ok(Box::new(TpmKeys {
            labels: labels.clone(),
            tls_public: public_key(&tls)?,
            approval_public: public_key(&approval)?,
        }))
    }

    fn delete(&self, labels: &KeyLabels) -> Result<(), KeyError> {
        for name in [&labels.tls, &labels.approval] {
            match open_key(name) {
                Ok(key) => {
                    // NCryptDeleteKey 成功后句柄已释放，不能再 Free。
                    let raw = key.0;
                    std::mem::forget(key);
                    check(unsafe { NCryptDeleteKey(raw, 0) })?;
                }
                Err(KeyError::KeyUnavailable) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

pub struct TpmKeys {
    labels: KeyLabels,
    tls_public: P256PublicKey,
    approval_public: P256PublicKey,
}

impl DeviceKeys for TpmKeys {
    fn backend(&self) -> KeyBackend {
        KeyBackend::Tpm
    }

    fn labels(&self) -> &KeyLabels {
        &self.labels
    }

    fn tls_public_key(&self) -> P256PublicKey {
        self.tls_public
    }

    fn sign_tls(&self, msg: &[u8]) -> Result<Vec<u8>, KeyError> {
        // 每次重新打开：密钥被删（吊销 / 重置）后立即不可用。
        let key = open_key(&self.labels.tls)?;
        Ok(p1363_to_der(&sign_hash(&key, msg, NCRYPT_SILENT_FLAG)?))
    }

    fn approval_public_key(&self) -> P256PublicKey {
        self.approval_public
    }

    fn sign_approval(&self, msg: &[u8], prompt: &PresencePrompt) -> Result<P1363Sig, KeyError> {
        let key = open_key(&self.labels.approval)?;
        let context = wide(&prompt.system_reason());
        check(unsafe {
            NCryptSetProperty(
                key.0,
                NCRYPT_USE_CONTEXT_PROPERTY,
                context.as_ptr().cast(),
                (context.len() * 2) as u32,
                0,
            )
        })?;
        // 不带 SILENT：由系统弹出 Windows Hello / PIN；用户未通过则失败，不存在无提示路径。
        match sign_hash(&key, msg, 0) {
            Ok(sig) => Ok(P1363Sig::from_bytes(sig)),
            Err(KeyError::UserCancelled) => Err(KeyError::UserCancelled),
            Err(KeyError::Backend(m)) => Err(KeyError::PresenceFailed(m)),
            Err(e) => Err(e),
        }
    }
}

/// 实机测试：需要 TPM 2.0 与 Windows Hello，CI 的 Windows 虚拟机没有，默认忽略。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "需要 TPM 2.0（M1.4 实机验证，#107）"]
    fn tpm_keys_round_trip_without_prompt() {
        let store = TpmKeyStore::open().unwrap();
        let keys = store.generate().unwrap();
        assert_ne!(keys.tls_public_key(), keys.approval_public_key());
        let der = keys.sign_tls(b"x").unwrap();
        assert_eq!(der[0], 0x30);
        let labels = keys.labels().clone();
        assert!(store.load(&labels).is_ok());
        store.delete(&labels).unwrap();
        assert_eq!(store.load(&labels).err(), Some(KeyError::KeyUnavailable));
    }
}
