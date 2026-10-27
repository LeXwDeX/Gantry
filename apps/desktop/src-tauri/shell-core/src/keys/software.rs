//! 软件密钥替身 `TestSoftwareKeys`（desktop-shell.md §6.5、§12"生物识别路径"）。
//!
//! 只在 `test-software-keys` feature 下编译。行为与硬件后端对齐：两把独立密钥、没有导出接口、
//! 审批签名必须经过 [`PresenceDouble`]（用户在场的替身），未通过就不签名也不降级。
//! 另可模拟"硬件不可用"与"密钥丢失（TPM 清除）"。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};

use super::{
    DeviceKeys, KeyBackend, KeyError, KeyLabels, KeyStore, P1363Sig, P256PublicKey, PresencePrompt,
};

/// 一次用户在场验证的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresenceOutcome {
    Approve,
    Cancel,
    Fail,
}

/// 用户在场的替身：按脚本给出结果（脚本耗尽后用默认值），并记录每次提示文案。
#[derive(Clone)]
pub struct PresenceDouble {
    inner: Arc<Mutex<PresenceState>>,
}

struct PresenceState {
    scripted: VecDeque<PresenceOutcome>,
    default: PresenceOutcome,
    prompts: Vec<PresencePrompt>,
}

impl PresenceDouble {
    /// 默认拒绝：没有显式安排的签名都会失败。
    pub fn new() -> Self {
        Self::with_default(PresenceOutcome::Fail)
    }

    pub fn with_default(default: PresenceOutcome) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PresenceState {
                scripted: VecDeque::new(),
                default,
                prompts: Vec::new(),
            })),
        }
    }

    pub fn push(&self, outcome: PresenceOutcome) {
        self.inner.lock().unwrap().scripted.push_back(outcome);
    }

    /// 迄今弹出过的提示（每次 `sign_approval` 一条）。
    pub fn prompts(&self) -> Vec<PresencePrompt> {
        self.inner.lock().unwrap().prompts.clone()
    }

    fn ask(&self, prompt: &PresencePrompt) -> PresenceOutcome {
        let mut state = self.inner.lock().unwrap();
        state.prompts.push(prompt.clone());
        state
            .scripted
            .pop_front()
            .unwrap_or_else(|| state.default.clone())
    }
}

impl Default for PresenceDouble {
    fn default() -> Self {
        Self::new()
    }
}

/// 软件密钥库。克隆共享同一份状态，便于测试从外部注入故障。
#[derive(Clone)]
pub struct TestSoftwareKeyStore {
    inner: Arc<Mutex<StoreState>>,
    presence: PresenceDouble,
}

struct StoreState {
    hardware_available: bool,
    keys: HashMap<String, SigningKey>,
}

impl TestSoftwareKeyStore {
    pub fn new(presence: PresenceDouble) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StoreState {
                hardware_available: true,
                keys: HashMap::new(),
            })),
            presence,
        }
    }

    pub fn presence(&self) -> &PresenceDouble {
        &self.presence
    }

    /// 模拟没有 Secure Enclave / TPM：此后 `generate` 失败。
    pub fn set_hardware_available(&self, available: bool) {
        self.inner.lock().unwrap().hardware_available = available;
    }

    /// 模拟 TPM 被清除：所有密钥消失。
    pub fn wipe(&self) {
        self.inner.lock().unwrap().keys.clear();
    }

    /// 当前保存的密钥数，用来断言删除生效。
    pub fn key_count(&self) -> usize {
        self.inner.lock().unwrap().keys.len()
    }

    fn get(&self, label: &str) -> Result<SigningKey, KeyError> {
        self.inner
            .lock()
            .unwrap()
            .keys
            .get(label)
            .cloned()
            .ok_or(KeyError::KeyUnavailable)
    }
}

impl KeyStore for TestSoftwareKeyStore {
    fn backend(&self) -> KeyBackend {
        KeyBackend::TestSoftware
    }

    fn generate(&self) -> Result<Box<dyn DeviceKeys>, KeyError> {
        let labels = KeyLabels::random()?;
        {
            let mut state = self.inner.lock().unwrap();
            if !state.hardware_available {
                return Err(KeyError::HardwareUnavailable("替身：硬件不可用".into()));
            }
            let mut rng = rand_core::OsRng;
            state
                .keys
                .insert(labels.tls.clone(), SigningKey::random(&mut rng));
            state
                .keys
                .insert(labels.approval.clone(), SigningKey::random(&mut rng));
        }
        self.load(&labels)
    }

    fn load(&self, labels: &KeyLabels) -> Result<Box<dyn DeviceKeys>, KeyError> {
        labels.validate()?;
        let tls = self.get(&labels.tls)?;
        let approval = self.get(&labels.approval)?;
        Ok(Box::new(TestSoftwareKeys {
            store: self.clone(),
            labels: labels.clone(),
            tls_public: public_of(&tls),
            approval_public: public_of(&approval),
        }))
    }

    fn delete(&self, labels: &KeyLabels) -> Result<(), KeyError> {
        let mut state = self.inner.lock().unwrap();
        state.keys.remove(&labels.tls);
        state.keys.remove(&labels.approval);
        Ok(())
    }
}

fn public_of(key: &SigningKey) -> P256PublicKey {
    let point = key.verifying_key().to_encoded_point(false);
    P256PublicKey::from_uncompressed(point.as_bytes()).expect("p256 未压缩点")
}

/// 每次签名都回到密钥库取私钥，这样 `wipe` / `delete` 之后立即不可用（与硬件一致）。
pub struct TestSoftwareKeys {
    store: TestSoftwareKeyStore,
    labels: KeyLabels,
    tls_public: P256PublicKey,
    approval_public: P256PublicKey,
}

impl DeviceKeys for TestSoftwareKeys {
    fn backend(&self) -> KeyBackend {
        KeyBackend::TestSoftware
    }

    fn labels(&self) -> &KeyLabels {
        &self.labels
    }

    fn tls_public_key(&self) -> P256PublicKey {
        self.tls_public
    }

    fn sign_tls(&self, msg: &[u8]) -> Result<Vec<u8>, KeyError> {
        let sig: Signature = self.store.get(&self.labels.tls)?.sign(msg);
        Ok(sig.to_der().as_bytes().to_vec())
    }

    fn approval_public_key(&self) -> P256PublicKey {
        self.approval_public
    }

    fn sign_approval(&self, msg: &[u8], prompt: &PresencePrompt) -> Result<P1363Sig, KeyError> {
        let key = self.store.get(&self.labels.approval)?;
        match self.store.presence.ask(prompt) {
            PresenceOutcome::Approve => {}
            PresenceOutcome::Cancel => return Err(KeyError::UserCancelled),
            PresenceOutcome::Fail => return Err(KeyError::PresenceFailed("替身：未通过".into())),
        }
        let sig: Signature = key.sign(msg);
        Ok(P1363Sig::from_bytes(sig.to_bytes().into()))
    }
}
