//! macOS Secure Enclave 后端。
//!
//! 未签名 / ad-hoc 签名的开发构建没有钥匙串访问组授权，`kSecAttrIsPermanent = true` 的 SE 密钥
//! 建不出来（`-34018`，见 `probe/README.md`）。所以两把密钥都建成**非持久** SE 密钥，把 SE 返回的
//! 包装块（`kSecAttrTokenOID`，即 CryptoKit 的 `dataRepresentation`）写进 `key_dir`：包装块由本机
//! SE 加密，离开这台机器的 SE 无法解开，不是私钥明文；删除包装块即销毁密钥。
//!
//! - mTLS 客户端密钥：访问控制 `privateKeyUsage`，加载时绑定"不允许交互"的 `LAContext`，
//!   保证永远不会弹框（若意外需要交互则直接失败）。
//! - 审批签名密钥：访问控制 `privateKeyUsage | userPresence`；每次签名用带提示文案的新 `LAContext`
//!   加载，由系统弹 Touch ID / 密码，未通过则失败。
//!
//! `sign_approval` 会阻塞到用户响应，调用方须放在非主线程。

use std::ffi::c_void;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::error::CFError;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{kCFAllocatorDefault, CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::error::CFErrorRef;
use objc2::rc::Retained;
use objc2_foundation::NSString;
use objc2_local_authentication::LAContext;
use security_framework_sys::access_control::{
    kSecAccessControlPrivateKeyUsage, kSecAccessControlUserPresence,
    kSecAttrAccessibleWhenUnlockedThisDeviceOnly, SecAccessControlCreateWithFlags,
};
use security_framework_sys::base::SecKeyRef;
use security_framework_sys::item::{
    kSecAttrAccessControl, kSecAttrIsPermanent, kSecAttrKeyClass, kSecAttrKeyClassPrivate,
    kSecAttrKeySizeInBits, kSecAttrKeyType, kSecAttrKeyTypeECSECPrimeRandom, kSecAttrTokenID,
    kSecAttrTokenIDSecureEnclave, kSecPrivateKeyAttrs, kSecUseAuthenticationContext,
};
use security_framework_sys::key::{
    kSecKeyAlgorithmECDSASignatureMessageX962SHA256, SecKeyCopyAttributes,
    SecKeyCopyExternalRepresentation, SecKeyCopyPublicKey, SecKeyCreateRandomKey,
    SecKeyCreateSignature, SecKeyCreateWithData,
};

use super::{
    der_to_p1363, DeviceKeys, KeyBackend, KeyError, KeyLabels, KeyStore, P1363Sig, P256PublicKey,
    PresencePrompt,
};

/// `kSecAttrTokenOID` 不在公开头文件里，键名是 `"toid"`（CryptoKit 的 `dataRepresentation` 即此值）。
const TOKEN_OID_KEY: &str = "toid";
const BLOB_EXT: &str = "seblob";
const LA_ERROR_DOMAIN: &str = "com.apple.LocalAuthentication";
const LA_ERROR_USER_CANCEL: isize = -2;
const ERR_SEC_USER_CANCELED: isize = -128;

#[derive(Clone, Copy)]
enum Usage {
    Tls,
    Approval,
}

/// 加载密钥时绑定的认证上下文。
enum Interaction<'a> {
    /// 不允许任何交互：需要用户在场的操作直接失败。
    Forbidden,
    /// 允许系统弹框，文案为给定的一行。
    Prompt(&'a str),
}

pub struct SecureEnclaveKeyStore {
    dir: PathBuf,
}

impl SecureEnclaveKeyStore {
    /// `dir` 不存在时创建，权限 0700。
    pub fn open(dir: &Path) -> Result<Self, KeyError> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| KeyError::Backend(format!("创建密钥目录：{e}")))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| KeyError::Backend(format!("设置密钥目录权限：{e}")))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn blob_path(&self, label: &str) -> PathBuf {
        self.dir.join(format!("{label}.{BLOB_EXT}"))
    }

    fn write_blob(&self, label: &str, blob: &[u8]) -> Result<(), KeyError> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.blob_path(label);
        let tmp = self.dir.join(format!("{label}.{BLOB_EXT}.tmp"));
        let io = |e: std::io::Error| KeyError::Backend(format!("写入密钥包装块：{e}"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io)?;
        file.write_all(blob).map_err(io)?;
        file.sync_all().map_err(io)?;
        fs::rename(&tmp, &path).map_err(io)
    }

    fn read_blob(&self, label: &str) -> Result<Vec<u8>, KeyError> {
        fs::read(self.blob_path(label)).map_err(|_| KeyError::KeyUnavailable)
    }

    fn generate_one(&self, label: &str, usage: Usage) -> Result<P256PublicKey, KeyError> {
        let key = create_random_key(usage)?;
        let blob = token_oid(&key)?;
        let public = public_key(&key)?;
        self.write_blob(label, &blob)?;
        Ok(public)
    }

    /// 从包装块恢复，绑定给定的认证上下文。
    fn restore(&self, label: &str, interaction: Interaction<'_>) -> Result<SecKey, KeyError> {
        let blob = self.read_blob(label)?;
        restore_key(&blob, interaction)
    }
}

impl KeyStore for SecureEnclaveKeyStore {
    fn backend(&self) -> KeyBackend {
        KeyBackend::SecureEnclave
    }

    fn generate(&self) -> Result<Box<dyn DeviceKeys>, KeyError> {
        let labels = KeyLabels::random()?;
        let result = self
            .generate_one(&labels.tls, Usage::Tls)
            .and_then(|_| self.generate_one(&labels.approval, Usage::Approval))
            .and_then(|_| self.load(&labels));
        if result.is_err() {
            let _ = self.delete(&labels);
        }
        result
    }

    fn load(&self, labels: &KeyLabels) -> Result<Box<dyn DeviceKeys>, KeyError> {
        labels.validate()?;
        // 两把都用"不允许交互"的上下文恢复，只为取公钥，不会弹框。
        let tls = self.restore(&labels.tls, Interaction::Forbidden)?;
        let approval = self.restore(&labels.approval, Interaction::Forbidden)?;
        Ok(Box::new(SecureEnclaveKeys {
            store: SecureEnclaveKeyStore {
                dir: self.dir.clone(),
            },
            labels: labels.clone(),
            tls_public: public_key(&tls)?,
            approval_public: public_key(&approval)?,
            tls,
        }))
    }

    fn delete(&self, labels: &KeyLabels) -> Result<(), KeyError> {
        for label in [&labels.tls, &labels.approval] {
            match fs::remove_file(self.blob_path(label)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(KeyError::Backend(format!("删除密钥包装块：{e}"))),
            }
        }
        Ok(())
    }
}

pub struct SecureEnclaveKeys {
    store: SecureEnclaveKeyStore,
    labels: KeyLabels,
    tls_public: P256PublicKey,
    approval_public: P256PublicKey,
    tls: SecKey,
}

impl SecureEnclaveKeys {
    /// 不允许交互地尝试用审批签名密钥签名。只用于验证访问控制确实生效（必须失败）。
    #[cfg(test)]
    fn sign_approval_without_presence(&self, msg: &[u8]) -> Result<P1363Sig, KeyError> {
        let key = self
            .store
            .restore(&self.labels.approval, Interaction::Forbidden)?;
        sign_der(&key, msg)
            .and_then(|der| der_to_p1363(&der).map_err(|e| KeyError::Backend(e.to_string())))
    }
}

impl DeviceKeys for SecureEnclaveKeys {
    fn backend(&self) -> KeyBackend {
        KeyBackend::SecureEnclave
    }

    fn labels(&self) -> &KeyLabels {
        &self.labels
    }

    fn tls_public_key(&self) -> P256PublicKey {
        self.tls_public
    }

    fn sign_tls(&self, msg: &[u8]) -> Result<Vec<u8>, KeyError> {
        // 包装块被删后立即不可用（吊销 / identity_reset 删除密钥，S-10）。
        if !self.store.blob_path(&self.labels.tls).exists() {
            return Err(KeyError::KeyUnavailable);
        }
        sign_der(&self.tls, msg)
    }

    fn approval_public_key(&self) -> P256PublicKey {
        self.approval_public
    }

    fn sign_approval(&self, msg: &[u8], prompt: &PresencePrompt) -> Result<P1363Sig, KeyError> {
        let reason = prompt.system_reason();
        let key = self
            .store
            .restore(&self.labels.approval, Interaction::Prompt(&reason))?;
        let der = sign_der(&key, msg)?;
        der_to_p1363(&der).map_err(|e| KeyError::Backend(e.to_string()))
    }
}

// ---- Security.framework 薄封装 ----

/// 拥有一个 `SecKeyRef`（+1 引用计数）。
struct SecKey(SecKeyRef);

// SecKey 对象线程安全（Security.framework 文档：SecKeyRef 可跨线程使用）。
unsafe impl Send for SecKey {}
unsafe impl Sync for SecKey {}

impl Drop for SecKey {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) }
    }
}

fn cf_key(key: core_foundation_sys::string::CFStringRef) -> CFString {
    unsafe { CFString::wrap_under_get_rule(key) }
}

fn take_error(err: CFErrorRef) -> CFError {
    unsafe { CFError::wrap_under_create_rule(err) }
}

fn describe(err: &CFError) -> String {
    format!("{} {}: {}", err.domain(), err.code(), err.description())
}

fn presence_error(err: CFError) -> KeyError {
    let domain = err.domain().to_string();
    let code = err.code();
    if (domain == LA_ERROR_DOMAIN && code == LA_ERROR_USER_CANCEL) || code == ERR_SEC_USER_CANCELED
    {
        KeyError::UserCancelled
    } else if domain == LA_ERROR_DOMAIN {
        KeyError::PresenceFailed(describe(&err))
    } else {
        KeyError::Backend(describe(&err))
    }
}

fn access_control(usage: Usage) -> Result<CFType, KeyError> {
    let flags = match usage {
        Usage::Tls => kSecAccessControlPrivateKeyUsage,
        Usage::Approval => kSecAccessControlPrivateKeyUsage | kSecAccessControlUserPresence,
    };
    let mut err: CFErrorRef = std::ptr::null_mut();
    let acl = unsafe {
        SecAccessControlCreateWithFlags(
            kCFAllocatorDefault,
            kSecAttrAccessibleWhenUnlockedThisDeviceOnly as CFTypeRef,
            flags,
            &mut err,
        )
    };
    if acl.is_null() {
        return Err(KeyError::Backend(describe(&take_error(err))));
    }
    Ok(unsafe { CFType::wrap_under_create_rule(acl as CFTypeRef) })
}

fn create_random_key(usage: Usage) -> Result<SecKey, KeyError> {
    let private_attrs = CFDictionary::from_CFType_pairs(&[
        (
            cf_key(unsafe { kSecAttrIsPermanent }),
            CFBoolean::false_value().as_CFType(),
        ),
        (
            cf_key(unsafe { kSecAttrAccessControl }),
            access_control(usage)?,
        ),
    ]);
    let attrs = CFDictionary::from_CFType_pairs(&[
        (
            cf_key(unsafe { kSecAttrKeyType }),
            cf_key(unsafe { kSecAttrKeyTypeECSECPrimeRandom }).as_CFType(),
        ),
        (
            cf_key(unsafe { kSecAttrKeySizeInBits }),
            CFNumber::from(256i32).as_CFType(),
        ),
        (
            cf_key(unsafe { kSecAttrTokenID }),
            cf_key(unsafe { kSecAttrTokenIDSecureEnclave }).as_CFType(),
        ),
        (
            cf_key(unsafe { kSecPrivateKeyAttrs }),
            private_attrs.as_CFType(),
        ),
    ]);
    let mut err: CFErrorRef = std::ptr::null_mut();
    let key = unsafe { SecKeyCreateRandomKey(attrs.as_concrete_TypeRef(), &mut err) };
    if key.is_null() {
        // 没有 SE（虚拟机、旧机型）或 SE 拒绝：fail-closed，不降级（OQ-S2）。
        return Err(KeyError::HardwareUnavailable(describe(&take_error(err))));
    }
    Ok(SecKey(key))
}

fn token_oid(key: &SecKey) -> Result<Vec<u8>, KeyError> {
    let attrs: CFDictionary<CFString, CFType> = unsafe {
        CFDictionary::wrap_under_create_rule(SecKeyCopyAttributes(key.0) as CFDictionaryRef)
    };
    let value = attrs
        .find(CFString::new(TOKEN_OID_KEY))
        .and_then(|v| v.downcast::<CFData>())
        .ok_or_else(|| KeyError::Backend("SE 密钥属性里没有 toid".into()))?;
    Ok(value.bytes().to_vec())
}

fn public_key(key: &SecKey) -> Result<P256PublicKey, KeyError> {
    let public = unsafe { SecKeyCopyPublicKey(key.0) };
    if public.is_null() {
        return Err(KeyError::Backend("取不到公钥".into()));
    }
    let public = SecKey(public);
    let mut err: CFErrorRef = std::ptr::null_mut();
    let data = unsafe { SecKeyCopyExternalRepresentation(public.0, &mut err) };
    if data.is_null() {
        return Err(KeyError::Backend(describe(&take_error(err))));
    }
    let data = unsafe { CFData::wrap_under_create_rule(data) };
    P256PublicKey::from_uncompressed(data.bytes())
}

fn la_context(interaction: Interaction<'_>) -> Retained<LAContext> {
    let ctx = unsafe { LAContext::new() };
    match interaction {
        Interaction::Forbidden => unsafe { ctx.setInteractionNotAllowed(true) },
        Interaction::Prompt(reason) => unsafe {
            ctx.setLocalizedReason(&NSString::from_str(reason))
        },
    }
    ctx
}

fn restore_key(blob: &[u8], interaction: Interaction<'_>) -> Result<SecKey, KeyError> {
    let ctx = la_context(interaction);
    // LAContext 是 Objective-C 对象，CF 容器对它 CFRetain / CFRelease 是合法的（toll-free 语义）。
    let ctx_cf = unsafe { CFType::wrap_under_get_rule(Retained::as_ptr(&ctx) as *const c_void) };
    let attrs = CFDictionary::from_CFType_pairs(&[
        (
            cf_key(unsafe { kSecAttrKeyType }),
            cf_key(unsafe { kSecAttrKeyTypeECSECPrimeRandom }).as_CFType(),
        ),
        (
            cf_key(unsafe { kSecAttrKeyClass }),
            cf_key(unsafe { kSecAttrKeyClassPrivate }).as_CFType(),
        ),
        (
            cf_key(unsafe { kSecAttrKeySizeInBits }),
            CFNumber::from(256i32).as_CFType(),
        ),
        (
            cf_key(unsafe { kSecAttrTokenID }),
            cf_key(unsafe { kSecAttrTokenIDSecureEnclave }).as_CFType(),
        ),
        (
            CFString::new(TOKEN_OID_KEY),
            CFData::from_buffer(blob).as_CFType(),
        ),
        (cf_key(unsafe { kSecUseAuthenticationContext }), ctx_cf),
    ]);
    let empty = CFData::from_buffer(&[]);
    let mut err: CFErrorRef = std::ptr::null_mut();
    let key = unsafe {
        SecKeyCreateWithData(
            empty.as_concrete_TypeRef(),
            attrs.as_concrete_TypeRef(),
            &mut err,
        )
    };
    if key.is_null() {
        // 包装块损坏、来自另一台机器或 SE 已重置。
        drop(take_error(err));
        return Err(KeyError::KeyUnavailable);
    }
    Ok(SecKey(key))
}

fn sign_der(key: &SecKey, msg: &[u8]) -> Result<Vec<u8>, KeyError> {
    let data = CFData::from_buffer(msg);
    let mut err: CFErrorRef = std::ptr::null_mut();
    let sig = unsafe {
        SecKeyCreateSignature(
            key.0,
            kSecKeyAlgorithmECDSASignatureMessageX962SHA256,
            data.as_concrete_TypeRef(),
            &mut err,
        )
    };
    if sig.is_null() {
        return Err(presence_error(take_error(err)));
    }
    Ok(unsafe { CFData::wrap_under_create_rule(sig) }
        .bytes()
        .to_vec())
}

/// 硬件测试：需要本机有 Secure Enclave，且不会弹 Touch ID（只用"不允许交互"的路径）。
/// CI 的 macOS 虚拟机没有 SE，所以默认忽略；本机用
/// `cargo test -p gantry-desktop-shell --all-features -- --ignored secure_enclave` 运行。
#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{DerSignature, VerifyingKey};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gantry-se-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    #[ignore = "需要本机 Secure Enclave"]
    fn secure_enclave_keys_round_trip_without_prompt() {
        let dir = temp_dir("roundtrip");
        let store = SecureEnclaveKeyStore::open(&dir).unwrap();
        let keys = store.generate().unwrap();
        assert_eq!(keys.backend(), KeyBackend::SecureEnclave);
        assert_ne!(keys.tls_public_key(), keys.approval_public_key());

        // mTLS 客户端密钥：无提示签名，DER，可用公钥验证。
        let der = keys.sign_tls(b"tls handshake").unwrap();
        let vk = VerifyingKey::from_sec1_bytes(keys.tls_public_key().as_bytes()).unwrap();
        vk.verify(b"tls handshake", &DerSignature::from_bytes(&der).unwrap())
            .unwrap();

        // 包装块权限只给当前用户。
        use std::os::unix::fs::PermissionsExt;
        for label in [&keys.labels().tls, &keys.labels().approval] {
            let mode = fs::metadata(store.blob_path(label))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // 新的 store 实例（相当于重启应用）按引用名恢复同一对密钥。
        let labels = keys.labels().clone();
        let tls_public = keys.tls_public_key();
        drop(keys);
        let store = SecureEnclaveKeyStore::open(&dir).unwrap();
        let reloaded = store.load(&labels).unwrap();
        assert_eq!(reloaded.tls_public_key(), tls_public);
        reloaded.sign_tls(b"after restart").unwrap();

        // 删除后不可再加载，旧句柄也不能再签。
        store.delete(&labels).unwrap();
        store.delete(&labels).unwrap();
        assert_eq!(store.load(&labels).err(), Some(KeyError::KeyUnavailable));
        assert_eq!(
            reloaded.sign_tls(b"x").err(),
            Some(KeyError::KeyUnavailable)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "需要本机 Secure Enclave"]
    fn secure_enclave_approval_key_requires_presence() {
        let dir = temp_dir("presence");
        let store = SecureEnclaveKeyStore::open(&dir).unwrap();
        let keys = store.generate().unwrap();
        let labels = keys.labels().clone();
        let se = SecureEnclaveKeys {
            store: SecureEnclaveKeyStore { dir: dir.clone() },
            labels: labels.clone(),
            tls_public: keys.tls_public_key(),
            approval_public: keys.approval_public_key(),
            tls: store.restore(&labels.tls, Interaction::Forbidden).unwrap(),
        };
        match se.sign_approval_without_presence(b"approval") {
            Err(KeyError::PresenceFailed(msg)) => assert!(msg.contains("-1004"), "{msg}"),
            other => panic!("审批签名密钥在不允许交互时必须失败，实际 {other:?}"),
        }
        store.delete(&labels).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "需要本机 Secure Enclave"]
    fn secure_enclave_rejects_foreign_blob() {
        let dir = temp_dir("foreign");
        let store = SecureEnclaveKeyStore::open(&dir).unwrap();
        let labels = KeyLabels::random().unwrap();
        store.write_blob(&labels.tls, &[0x30; 64]).unwrap();
        store.write_blob(&labels.approval, &[0x30; 64]).unwrap();
        assert_eq!(store.load(&labels).err(), Some(KeyError::KeyUnavailable));
        let _ = fs::remove_dir_all(&dir);
    }
}
