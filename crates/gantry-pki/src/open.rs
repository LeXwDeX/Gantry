//! 总入口 `Pki`（pki.md §6.1）：打开已引导的 PKI、启动自检、TLS 材料与服务端证书续签。
//!
//! 启动顺序：读 `ca_dir` 的三张证书 → 根私钥自检（服务端上不得有与根证书公钥匹配的私钥，§8）→
//! 经 `secrets` 取中间 CA 私钥 → 首次启动把 CA 与服务端证书登记进 `pki_ca` / `pki_server_cert` →
//! 吊销日志核对（日志里有、库里仍 active 的设备 → 拒绝启动，P-67）→ 组装目录、配对、管理与 TLS。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::{Clock, Entropy, IdGen};
use gantry_core::ports::{ActorCtx, Expect, SecretSink, SecretValue};
use gantry_core::secret::SecretName;
use gantry_core::time::Timestamp;
use gantry_store::{Store, WriteTx};
use p256::pkcs8::{DecodePrivateKey, EncodePublicKey};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::admin::StoreAdmin;
use crate::bootstrap::{INTERMEDIATE_CRT, ROOT_CRT, SERVER_CRT};
use crate::ca::{Authority, IssuedCert};
use crate::config::CertPolicy;
use crate::directory::{store_error, StoreDirectory};
use crate::journal::RevocationJournal;
use crate::keys::CertSerial;
use crate::pairing_service::{Issuer, PairingConfig, PairingService};
use crate::store_schema::{CA, MODULE, SERVER_CERT};
use crate::verifier::DeviceCertVerifier;
use crate::PkiError;

/// 中间 CA 私钥在 `secrets`（使用方 `pki`）里的名字。`deploy` 引导时按此名写入。
pub const INTERMEDIATE_KEY_SECRET: &str = "ca.intermediate.key";
/// 服务端叶子私钥在 `secrets` 里的名字；续签时 `stage` → 写 `pki_server_cert` → `commit`（OQ-K10）。
pub const SERVER_KEY_SECRET: &str = "tls.server.key";

/// 剩余有效期不超过此值时健康转为 `unhealthy`（§9）。
const UNHEALTHY_WITHIN_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 根私钥自检只看这么大的文件。
const MAX_SCAN_FILE_BYTES: u64 = 1 << 20;

/// 引导配置（pki.md §10）。
#[derive(Clone, Debug)]
pub struct PkiConfig {
    /// `pki.ca_dir`：`root.crt`、`intermediate.crt`、`server.crt`（deploy.md §7.3）。
    pub ca_dir: PathBuf,
    /// 根私钥自检要扫描的目录（数据目录、`secrets/` 等，§8）。
    pub root_key_scan_dirs: Vec<PathBuf>,
    /// `pki.revocations_journal` 与备份目标 `backup.journal_target`。
    pub journal: RevocationJournal,
    /// `pki.server_names`：服务端叶子 SAN；变化触发重新签发。
    pub server_names: Vec<String>,
    pub cert_policy: CertPolicy,
    pub pairing: PairingConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// 续签失败但旧证书仍有较长有效期。
    Degraded,
    /// 续签失败且剩余不超过 7 天。
    Unhealthy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerRenewal {
    NotDue,
    Renewed {
        serial: CertSerial,
        not_after: Timestamp,
    },
}

/// `pki_ca` 的一行；键为 `root` 或 `intermediate/<serial>`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaRow {
    pub kind: CaKind,
    pub serial: CertSerial,
    /// `sha256:` + 证书 DER 的十六进制摘要。
    pub fingerprint: String,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
    pub status: CaStatus,
    pub cert_pem: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaKind {
    Root,
    Intermediate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaStatus {
    Current,
    Previous,
    Retired,
}

/// `pki_server_cert` 的一行；键为序列号。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCertRow {
    pub serial: CertSerial,
    pub not_before: Timestamp,
    pub not_after: Timestamp,
    /// `[叶子, 中间 CA, 根]`。
    pub chain_pem: Vec<String>,
    pub server_names: Vec<String>,
    pub status: CaStatus,
}

/// `api` 构造 `ServerConfig` 用（§6.4）。
#[derive(Clone)]
pub struct TlsMaterial {
    pub client_verifier: Arc<DeviceCertVerifier>,
    pub server_cert_resolver: Arc<ServerCertResolver>,
    pub root_cert_pem: String,
    pub ca_fingerprint: String,
}

/// 可热替换的服务端证书；续签后新握手拿到新叶子，已建立的连接不受影响。
#[derive(Debug)]
pub struct ServerCertResolver {
    current: RwLock<Arc<CertifiedKey>>,
}

impl ServerCertResolver {
    fn new(key: Arc<CertifiedKey>) -> Self {
        Self {
            current: RwLock::new(key),
        }
    }

    fn replace(&self, key: Arc<CertifiedKey>) {
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = key;
    }

    pub fn current(&self) -> Arc<CertifiedKey> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ResolvesServerCert for ServerCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

pub struct Pki {
    store: Store,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    secrets: Arc<dyn SecretSink>,
    ids: IdGen,
    config: PkiConfig,
    issuer: Arc<Issuer>,
    root: IssuedCert,
    directory: Arc<StoreDirectory>,
    admin: Arc<StoreAdmin>,
    pairing: Arc<PairingService>,
    tls: TlsMaterial,
    health: Mutex<Health>,
}

impl Pki {
    pub async fn open(
        config: PkiConfig,
        store: Store,
        secrets: Arc<dyn SecretSink>,
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
    ) -> Result<Pki, PkiError> {
        let read = |name: &str| -> Result<String, PkiError> {
            fs::read_to_string(config.ca_dir.join(name)).map_err(PkiError::Io)
        };
        let root = IssuedCert::from_der(pem_der(&read(ROOT_CRT)?)?)?;
        let intermediate_pem = read(INTERMEDIATE_CRT)?;
        let initial_server = IssuedCert::from_der(pem_der(&read(SERVER_CRT)?)?)?;

        check_no_root_key(&root, &config.root_key_scan_dirs)?;

        let intermediate_key = read_secret(secrets.as_ref(), INTERMEDIATE_KEY_SECRET)?.ok_or(
            PkiError::Config("intermediate CA key is missing from secrets"),
        )?;
        let intermediate = Authority::load(&intermediate_pem, &intermediate_key)?;
        let intermediate_der = intermediate.certificate().der.clone();
        let issuer = Arc::new(Issuer::new(intermediate, root.clone()));

        let server_row = {
            let mut tx = store.begin_write().await.map_err(store_error)?;
            register_ca(&mut tx, &root, CaKind::Root)?;
            register_ca(
                &mut tx,
                issuer.intermediate().certificate(),
                CaKind::Intermediate,
            )?;
            let row = match current_server_row(&mut tx)? {
                Some(row) => row,
                None => {
                    let row = ServerCertRow {
                        serial: initial_server.serial.clone(),
                        not_before: initial_server.not_before,
                        not_after: initial_server.not_after,
                        chain_pem: issuer.chain(&initial_server).to_vec(),
                        server_names: config.server_names.clone(),
                        status: CaStatus::Current,
                    };
                    tx.module_conn(MODULE)
                        .put(SERVER_CERT, row.serial.as_str(), &row)
                        .map_err(store_error)?;
                    row
                }
            };
            tx.commit().await.map_err(store_error)?;
            row
        };
        let server_key = read_secret(secrets.as_ref(), SERVER_KEY_SECRET)?
            .ok_or(PkiError::Config("server key is missing from secrets"))?;
        let resolver = Arc::new(ServerCertResolver::new(certified_key(
            &server_row.chain_pem,
            &server_key,
        )?));

        let directory = Arc::new(StoreDirectory::new(store.clone(), clock.clone()));
        let admin = Arc::new(StoreAdmin::new(
            store.clone(),
            clock.clone(),
            entropy.clone(),
            issuer.clone(),
            config.journal.clone(),
            config.cert_policy.clone(),
        ));
        let pending = admin.pending_revocations().await?;
        if !pending.is_empty() {
            return Err(PkiError::PendingRevocations(pending.len()));
        }
        let pairing = Arc::new(PairingService::new(
            store.clone(),
            clock.clone(),
            entropy.clone(),
            issuer.clone(),
            config.pairing.clone(),
        ));
        let verifier = Arc::new(DeviceCertVerifier::new(
            &root.der,
            &[intermediate_der],
            directory.clone(),
        )?);
        let tls = TlsMaterial {
            client_verifier: verifier,
            server_cert_resolver: resolver,
            root_cert_pem: root.pem.clone(),
            ca_fingerprint: issuer.ca_fingerprint(),
        };
        let ids = IdGen::new(clock.clone(), entropy.clone());
        Ok(Pki {
            store,
            clock,
            entropy,
            secrets,
            ids,
            config,
            issuer,
            root,
            directory,
            admin,
            pairing,
            tls,
            health: Mutex::new(Health::Ok),
        })
    }

    pub fn directory(&self) -> Arc<StoreDirectory> {
        self.directory.clone()
    }

    pub fn admin(&self) -> Arc<StoreAdmin> {
        self.admin.clone()
    }

    pub fn pairing(&self) -> Arc<PairingService> {
        self.pairing.clone()
    }

    pub fn tls(&self) -> TlsMaterial {
        self.tls.clone()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn root_certificate(&self) -> &IssuedCert {
        &self.root
    }

    pub fn health(&self) -> Health {
        *self
            .health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 后台节拍（`gantryd` 定期调用，建议每 6 小时；配对会话到期可更频繁）：
    /// 关闭过期的配对窗口，并在需要时续签服务端证书。续签失败不返回错误，只更新健康状态。
    pub async fn tick(&self) -> Result<ServerRenewal, PkiError> {
        self.pairing.expire_sessions().await?;
        match self.renew_server_cert_if_due().await {
            Ok(outcome) => {
                self.set_health(Health::Ok);
                Ok(outcome)
            }
            Err(error) => {
                let remaining =
                    self.tls_not_after().await?.as_millis() - self.clock.now().as_millis();
                self.set_health(if remaining <= UNHEALTHY_WITHIN_MS {
                    Health::Unhealthy
                } else {
                    Health::Degraded
                });
                tracing::warn!(%error, remaining_ms = remaining, "server certificate renewal failed");
                Ok(ServerRenewal::NotDue)
            }
        }
    }

    /// 剩余不超过 `server_cert_renew_before` 或 SAN 配置变化时签发新的服务端叶子（§7.7）：
    /// 新私钥先 `stage` 进 `secrets`，证书入库后 `commit`，再热替换 resolver。
    pub async fn renew_server_cert_if_due(&self) -> Result<ServerRenewal, PkiError> {
        let now = self.clock.now();
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let current = current_server_row(&mut tx)?
            .ok_or(PkiError::Config("no current server certificate"))?;
        let renew_before =
            i64::try_from(self.config.cert_policy.server_cert_renew_before.as_millis())
                .unwrap_or(i64::MAX);
        let due = current.not_after.as_millis() - now.as_millis() <= renew_before
            || current.server_names != self.config.server_names;
        if !due {
            tx.rollback().await.map_err(store_error)?;
            return Ok(ServerRenewal::NotDue);
        }

        let (leaf, key_pem) = self.issuer.intermediate().issue_server_leaf(
            &self.config.server_names,
            now,
            &self.config.cert_policy,
            self.entropy.as_ref(),
        )?;
        let name = secret_name(SERVER_KEY_SECRET)?;
        let ctx = self.system_ctx()?;
        let staged = self
            .secrets
            .stage(
                &name,
                SecretValue::from_bytes(key_pem.as_bytes().to_vec()),
                Expect::Any,
                &ctx,
            )
            .map_err(|error| PkiError::Secrets(error.to_string()))?;
        let row = ServerCertRow {
            serial: leaf.serial.clone(),
            not_before: leaf.not_before,
            not_after: leaf.not_after,
            chain_pem: self.issuer.chain(&leaf).to_vec(),
            server_names: self.config.server_names.clone(),
            status: CaStatus::Current,
        };
        let written = (|| -> Result<(), PkiError> {
            let mut conn = tx.module_conn(MODULE);
            let mut old = current.clone();
            old.status = CaStatus::Previous;
            conn.put(SERVER_CERT, old.serial.as_str(), &old)
                .map_err(store_error)?;
            conn.put(SERVER_CERT, row.serial.as_str(), &row)
                .map_err(store_error)?;
            Ok(())
        })();
        let key = written.and_then(|()| certified_key(&row.chain_pem, &key_pem));
        let key = match key {
            Ok(key) => key,
            Err(error) => {
                let _ = self.secrets.abort(&staged, &ctx);
                tx.rollback().await.map_err(store_error)?;
                return Err(error);
            }
        };
        if let Err(error) = tx.commit().await {
            let _ = self.secrets.abort(&staged, &ctx);
            return Err(store_error(error));
        }
        self.secrets
            .commit(&staged, &ctx)
            .map_err(|error| PkiError::Secrets(error.to_string()))?;
        self.tls.server_cert_resolver.replace(key);
        tracing::info!(serial = %leaf.serial, not_after = ?leaf.not_after, "server certificate renewed");
        Ok(ServerRenewal::Renewed {
            serial: leaf.serial,
            not_after: leaf.not_after,
        })
    }

    async fn tls_not_after(&self) -> Result<Timestamp, PkiError> {
        let mut tx = self.store.begin_write().await.map_err(store_error)?;
        let row = current_server_row(&mut tx)?;
        tx.rollback().await.map_err(store_error)?;
        row.map(|row| row.not_after)
            .ok_or(PkiError::Config("no current server certificate"))
    }

    fn set_health(&self, health: Health) {
        *self
            .health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = health;
    }

    fn system_ctx(&self) -> Result<ActorCtx, PkiError> {
        Ok(ActorCtx {
            actor: Actor::new(ActorKind::System, "pki"),
            device: None,
            role: None,
            client_profile: None,
            request_id: self
                .ids
                .next_ulid()
                .map_err(|error| PkiError::Crypto(format!("request id: {error}")))?,
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        })
    }
}

fn register_ca(tx: &mut WriteTx, cert: &IssuedCert, kind: CaKind) -> Result<(), PkiError> {
    let key = match kind {
        CaKind::Root => "root".to_owned(),
        CaKind::Intermediate => format!("intermediate/{}", cert.serial),
    };
    let row = CaRow {
        kind,
        serial: cert.serial.clone(),
        fingerprint: format!("sha256:{}", cert.fingerprint().to_hex()),
        not_before: cert.not_before,
        not_after: cert.not_after,
        status: CaStatus::Current,
        cert_pem: cert.pem.clone(),
    };
    let mut conn = tx.module_conn(MODULE);
    if let Some(existing) = conn.get::<CaRow>(CA, &key).map_err(store_error)? {
        if existing.fingerprint != row.fingerprint {
            return Err(PkiError::Config(
                "CA certificate in ca_dir differs from the registered one",
            ));
        }
        return Ok(());
    }
    conn.insert(CA, &key, &row).map_err(store_error)?;
    Ok(())
}

fn current_server_row(tx: &mut WriteTx) -> Result<Option<ServerCertRow>, PkiError> {
    let conn = tx.module_conn(MODULE);
    let mut after: Option<String> = None;
    loop {
        let page: Vec<(String, ServerCertRow)> = conn
            .scan(
                SERVER_CERT,
                "",
                after.as_deref(),
                gantry_store::MODULE_SCAN_MAX,
            )
            .map_err(store_error)?;
        if let Some((_, row)) = page.iter().find(|(_, row)| row.status == CaStatus::Current) {
            return Ok(Some(row.clone()));
        }
        if page.len() < gantry_store::MODULE_SCAN_MAX as usize {
            return Ok(None);
        }
        after = page.last().map(|(key, _)| key.clone());
    }
}

fn certified_key(chain_pem: &[String], key_pem: &str) -> Result<Arc<CertifiedKey>, PkiError> {
    let chain = chain_pem
        .iter()
        .map(|pem_text| pem_der(pem_text).map(CertificateDer::from))
        .collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|error| PkiError::Crypto(format!("server key: {error}")))?;
    let signer = rustls::crypto::ring::sign::any_ecdsa_type(&key)
        .map_err(|error| PkiError::Crypto(format!("server key: {error}")))?;
    let certified = CertifiedKey::new(chain, signer);
    certified.keys_match().map_err(|error| {
        PkiError::Crypto(format!("server key does not match certificate: {error}"))
    })?;
    Ok(Arc::new(certified))
}

fn pem_der(text: &str) -> Result<Vec<u8>, PkiError> {
    pem::parse(text)
        .map(pem::Pem::into_contents)
        .map_err(|error| PkiError::Crypto(format!("PEM: {error}")))
}

fn secret_name(name: &str) -> Result<SecretName, PkiError> {
    SecretName::parse(name).map_err(|_| PkiError::Config("invalid secret name"))
}

fn read_secret(secrets: &dyn SecretSink, name: &str) -> Result<Option<String>, PkiError> {
    let secret = secrets
        .get(&secret_name(name)?)
        .map_err(|error| PkiError::Secrets(error.to_string()))?;
    secret
        .map(|secret| {
            secret
                .value
                .expose_str()
                .map(str::to_owned)
                .map_err(|_| PkiError::Config("secret is not UTF-8 PEM"))
        })
        .transpose()
}

/// 根私钥自检（§8）：在给定目录里找 PEM 私钥块（PKCS#8 `PRIVATE KEY` 或 SEC1 `EC PRIVATE KEY`），
/// 推导公钥；与根证书公钥匹配 → 拒绝启动。加密的私钥无法比对，不在此列（部署断言另行覆盖）。
fn check_no_root_key(root: &IssuedCert, dirs: &[PathBuf]) -> Result<(), PkiError> {
    let (_, parsed) = X509Certificate::from_der(&root.der)
        .map_err(|error| PkiError::Crypto(format!("parse root certificate: {error}")))?;
    let root_spki = parsed.tbs_certificate.subject_pki.raw.to_vec();
    for dir in dirs {
        if let Some(path) = find_matching_key(dir, &root_spki)? {
            return Err(PkiError::RootKeyPresent(path));
        }
    }
    Ok(())
}

fn find_matching_key(dir: &Path, root_spki: &[u8]) -> Result<Option<PathBuf>, PkiError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PkiError::Io(error)),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if let Some(found) = find_matching_key(&path, root_spki)? {
                return Ok(Some(found));
            }
        } else if file_type.is_file() && entry.metadata()?.len() <= MAX_SCAN_FILE_BYTES {
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if key_matches(&bytes, root_spki) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn key_matches(bytes: &[u8], root_spki: &[u8]) -> bool {
    let Ok(blocks) = pem::parse_many(bytes) else {
        return false;
    };
    blocks.iter().any(|block| {
        let secret = match block.tag() {
            "PRIVATE KEY" => p256::SecretKey::from_pkcs8_der(block.contents()).ok(),
            "EC PRIVATE KEY" => p256::SecretKey::from_sec1_der(block.contents()).ok(),
            _ => None,
        };
        secret
            .and_then(|secret| secret.public_key().to_public_key_der().ok())
            .is_some_and(|spki| spki.as_bytes() == root_spki)
    })
}
