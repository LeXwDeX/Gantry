//! `TestCA`：测试用的证书与设备钥匙，**不是** M1 的 `pki` 实现。
//!
//! 使用系统 `openssl` 生成 P-256 根 / 中间 / 服务端 / 设备证书，以及一把
//! 独立的审批签名密钥。所有私钥只存在于运行时临时目录，`Drop` 时删除；
//! 本模块不提交任何密钥，也不自行实现密码学算法。
//!
//! 签名输出为 ES256 的原始 `r ‖ s`（64 字节，IEEE P1363），与 api.md §6.3
//! 一致；`approval_pubkey` 为未压缩点的 base64url（无填充）。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gantry_core::canon::{sha256, Sha256Digest};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaError {
    message: String,
}

impl CaError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CaError {}

/// 设备叶子证书及其私钥。私钥只在 `TestCA` 的临时目录中。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCert {
    pub common_name: String,
    pub serial: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub chain_pem: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceEku {
    ClientAuth,
    ServerAuth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceIssuer {
    Intermediate,
    ForeignRoot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCertSpec {
    pub common_name: String,
    pub not_before: Option<String>,
    pub not_after: Option<String>,
    pub eku: DeviceEku,
    pub issuer: DeviceIssuer,
}

impl Default for DeviceCertSpec {
    fn default() -> Self {
        Self {
            common_name: "dev_test".into(),
            not_before: None,
            not_after: None,
            eku: DeviceEku::ClientAuth,
            issuer: DeviceIssuer::Intermediate,
        }
    }
}

/// M0 的测试 CA。名称保留 `CA` 以匹配里程碑里的 `TestCA`。
#[allow(clippy::upper_case_acronyms)]
pub struct TestCA {
    dir: PathBuf,
    root_pem: String,
    root_der: Vec<u8>,
    intermediate_pem: String,
    server_pem: String,
    server_key_pem: String,
    device: DeviceCert,
    approval_public_pem: String,
    approval_public_der: Vec<u8>,
    approval_public_key: String,
    counter: AtomicU64,
}

impl TestCA {
    pub fn new() -> Result<Self, CaError> {
        let dir = temp_dir();
        fs::create_dir_all(&dir)
            .map_err(|error| CaError::new(format!("create test CA dir: {error}")))?;
        set_mode(&dir, 0o700)?;
        let mut ca = Self {
            dir,
            root_pem: String::new(),
            root_der: Vec::new(),
            intermediate_pem: String::new(),
            server_pem: String::new(),
            server_key_pem: String::new(),
            device: DeviceCert {
                common_name: String::new(),
                serial: String::new(),
                cert_pem: String::new(),
                key_pem: String::new(),
                chain_pem: String::new(),
            },
            approval_public_pem: String::new(),
            approval_public_der: Vec::new(),
            approval_public_key: String::new(),
            counter: AtomicU64::new(1),
        };
        ca.bootstrap()?;
        Ok(ca)
    }

    pub fn generate() -> Result<Self, CaError> {
        Self::new()
    }

    fn bootstrap(&mut self) -> Result<(), CaError> {
        self.gen_key("root.key")?;
        run(
            &self.dir,
            &[
                "req",
                "-x509",
                "-new",
                "-key",
                "root.key",
                "-out",
                "root.crt",
                "-days",
                "3650",
                "-sha256",
                "-subj",
                "/CN=Gantry Test Root",
                "-addext",
                "basicConstraints=critical,CA:TRUE,pathlen:1",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
                "-addext",
                "subjectKeyIdentifier=hash",
            ],
        )?;
        self.gen_key("inter.key")?;
        self.gen_csr("inter.key", "inter.csr", "Gantry Test Intermediate")?;
        fs::write(
            self.dir.join("inter.ext"),
            "basicConstraints=critical,CA:TRUE,pathlen:0\n\
             keyUsage=critical,keyCertSign,cRLSign\n\
             subjectKeyIdentifier=hash\n\
             authorityKeyIdentifier=keyid,issuer\n",
        )
        .map_err(|error| CaError::new(format!("write intermediate extensions: {error}")))?;
        run(
            &self.dir,
            &[
                "x509",
                "-req",
                "-in",
                "inter.csr",
                "-CA",
                "root.crt",
                "-CAkey",
                "root.key",
                "-CAcreateserial",
                "-CAserial",
                "root.srl",
                "-days",
                "730",
                "-sha256",
                "-extfile",
                "inter.ext",
                "-out",
                "inter.crt",
            ],
        )?;

        self.gen_key("server.key")?;
        self.gen_csr("server.key", "server.csr", "localhost")?;
        fs::write(
            self.dir.join("server.ext"),
            "basicConstraints=critical,CA:FALSE\n\
             keyUsage=critical,digitalSignature\n\
             extendedKeyUsage=serverAuth\n\
             subjectAltName=DNS:localhost,IP:127.0.0.1\n\
             subjectKeyIdentifier=hash\n\
             authorityKeyIdentifier=keyid,issuer\n",
        )
        .map_err(|error| CaError::new(format!("write server extensions: {error}")))?;
        run(
            &self.dir,
            &[
                "x509",
                "-req",
                "-in",
                "server.csr",
                "-CA",
                "inter.crt",
                "-CAkey",
                "inter.key",
                "-CAcreateserial",
                "-CAserial",
                "inter.srl",
                "-days",
                "90",
                "-sha256",
                "-extfile",
                "server.ext",
                "-out",
                "server.crt",
            ],
        )?;

        self.gen_key("approval.key")?;
        set_mode(&self.dir.join("approval.key"), 0o600)?;
        run(
            &self.dir,
            &[
                "pkey",
                "-in",
                "approval.key",
                "-pubout",
                "-out",
                "approval.pub.pem",
            ],
        )?;
        run(
            &self.dir,
            &[
                "pkey",
                "-in",
                "approval.key",
                "-pubout",
                "-outform",
                "DER",
                "-out",
                "approval.pub.der",
            ],
        )?;

        let root_pem = read_string(&self.dir.join("root.crt"))?;
        let root_der = read_der(&self.dir.join("root.crt"))?;
        let intermediate_pem = read_string(&self.dir.join("inter.crt"))?;
        let server_pem = read_string(&self.dir.join("server.crt"))?;
        let server_key_pem = read_string(&self.dir.join("server.key"))?;
        let approval_public_pem = read_string(&self.dir.join("approval.pub.pem"))?;
        let approval_public_der = fs::read(self.dir.join("approval.pub.der"))
            .map_err(|error| CaError::new(format!("read approval public key: {error}")))?;
        let approval_public_key = approval_public_key_from_spki(&approval_public_der)?;

        self.root_pem = root_pem;
        self.root_der = root_der;
        self.intermediate_pem = intermediate_pem;
        self.server_pem = server_pem;
        self.server_key_pem = server_key_pem;
        self.approval_public_pem = approval_public_pem;
        self.approval_public_der = approval_public_der;
        self.approval_public_key = approval_public_key;
        self.device = self.issue_device_with(DeviceCertSpec::default())?;
        Ok(())
    }

    pub fn root_dir(&self) -> &Path {
        &self.dir
    }

    pub fn root_cert_pem(&self) -> &str {
        &self.root_pem
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.root_pem
    }

    pub fn intermediate_cert_pem(&self) -> &str {
        &self.intermediate_pem
    }

    pub fn server_cert_pem(&self) -> &str {
        &self.server_pem
    }

    pub fn server_key_pem(&self) -> &str {
        &self.server_key_pem
    }

    pub fn device(&self) -> &DeviceCert {
        &self.device
    }

    pub fn device_cert_pem(&self) -> &str {
        &self.device.cert_pem
    }

    pub fn device_key_pem(&self) -> &str {
        &self.device.key_pem
    }

    /// `[叶子, 中间 CA, 根]`。
    pub fn device_chain_pem(&self) -> &str {
        &self.device.chain_pem
    }

    pub fn approval_public_key_pem(&self) -> &str {
        &self.approval_public_pem
    }

    pub fn approval_public_key(&self) -> &str {
        &self.approval_public_key
    }

    pub fn approval_pubkey_fingerprint(&self) -> Sha256Digest {
        sha256(&self.approval_public_der)
    }

    pub fn root_fingerprint(&self) -> Sha256Digest {
        sha256(&self.root_der)
    }

    pub fn ca_fingerprint(&self) -> Sha256Digest {
        self.root_fingerprint()
    }

    pub fn device_pubkey_fingerprint(&self) -> Result<Sha256Digest, CaError> {
        let spki = self.spki_from_cert(&self.device.cert_pem)?;
        Ok(sha256(&spki))
    }

    pub fn issue_expired(&self) -> Result<DeviceCert, CaError> {
        self.issue_expired_device()
    }

    pub fn issue_expired_device(&self) -> Result<DeviceCert, CaError> {
        self.issue_device_with(DeviceCertSpec {
            common_name: "dev_expired".into(),
            not_before: Some("20200101000000Z".into()),
            not_after: Some("20200102000000Z".into()),
            ..DeviceCertSpec::default()
        })
    }

    pub fn issue_not_yet_valid(&self) -> Result<DeviceCert, CaError> {
        self.issue_not_yet_valid_device()
    }

    pub fn issue_not_yet_valid_device(&self) -> Result<DeviceCert, CaError> {
        self.issue_device_with(DeviceCertSpec {
            common_name: "dev_not_yet_valid".into(),
            not_before: Some("20990101000000Z".into()),
            not_after: Some("20990102000000Z".into()),
            ..DeviceCertSpec::default()
        })
    }

    pub fn issue_from_foreign_ca(&self) -> Result<DeviceCert, CaError> {
        self.issue_foreign_ca_device()
    }

    pub fn issue_foreign_ca_device(&self) -> Result<DeviceCert, CaError> {
        self.issue_device_with(DeviceCertSpec {
            common_name: "dev_foreign".into(),
            issuer: DeviceIssuer::ForeignRoot,
            ..DeviceCertSpec::default()
        })
    }

    pub fn issue_server_auth_only(&self) -> Result<DeviceCert, CaError> {
        self.issue_server_auth_only_device()
    }

    pub fn issue_server_auth_only_device(&self) -> Result<DeviceCert, CaError> {
        self.issue_device_with(DeviceCertSpec {
            common_name: "dev_server_auth".into(),
            eku: DeviceEku::ServerAuth,
            ..DeviceCertSpec::default()
        })
    }

    pub fn issue_device_with(&self, spec: DeviceCertSpec) -> Result<DeviceCert, CaError> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let key_name = format!("device-{counter}.key");
        let csr_name = format!("device-{counter}.csr");
        let cert_name = format!("device-{counter}.crt");
        let ext_name = format!("device-{counter}.ext");
        self.gen_key(&key_name)?;
        set_mode(&self.dir.join(&key_name), 0o600)?;
        self.gen_csr(&key_name, &csr_name, &spec.common_name)?;

        let eku = match spec.eku {
            DeviceEku::ClientAuth => "clientAuth",
            DeviceEku::ServerAuth => "serverAuth",
        };
        fs::write(
            self.dir.join(&ext_name),
            format!(
                "basicConstraints=critical,CA:FALSE\n\
                 keyUsage=critical,digitalSignature\n\
                 extendedKeyUsage={eku}\n\
                 subjectKeyIdentifier=hash\n\
                 authorityKeyIdentifier=keyid,issuer\n"
            ),
        )
        .map_err(|error| CaError::new(format!("write device extensions: {error}")))?;

        let (ca_cert, ca_key, serial, issuer_pem) = match spec.issuer {
            DeviceIssuer::Intermediate => (
                "inter.crt",
                "inter.key",
                "inter-device.srl",
                self.intermediate_pem.clone(),
            ),
            DeviceIssuer::ForeignRoot => {
                self.ensure_foreign_root()?;
                (
                    "foreign.crt",
                    "foreign.key",
                    "foreign.srl",
                    read_string(&self.dir.join("foreign.crt"))?,
                )
            }
        };
        let mut args = vec![
            "x509".to_owned(),
            "-req".to_owned(),
            "-in".to_owned(),
            csr_name.clone(),
            "-CA".to_owned(),
            ca_cert.to_owned(),
            "-CAkey".to_owned(),
            ca_key.to_owned(),
            "-CAcreateserial".to_owned(),
            "-CAserial".to_owned(),
            serial.to_owned(),
            "-sha256".to_owned(),
            "-extfile".to_owned(),
            ext_name.clone(),
            "-out".to_owned(),
            cert_name.clone(),
        ];
        match (&spec.not_before, &spec.not_after) {
            (Some(not_before), Some(not_after)) => {
                args.push("-not_before".into());
                args.push(not_before.clone());
                args.push("-not_after".into());
                args.push(not_after.clone());
            }
            (Some(not_before), None) => {
                args.push("-days".into());
                args.push("90".into());
                args.push("-not_before".into());
                args.push(not_before.clone());
            }
            (None, Some(not_after)) => {
                args.push("-not_after".into());
                args.push(not_after.clone());
            }
            (None, None) => {
                args.push("-days".into());
                args.push("90".into());
            }
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&self.dir, &refs)?;
        let cert_pem = read_string(&self.dir.join(&cert_name))?;
        let key_pem = read_string(&self.dir.join(&key_name))?;
        let serial = cert_serial(&cert_pem)?;
        let chain_pem = format!("{cert_pem}{issuer_pem}{}", self.root_pem);
        Ok(DeviceCert {
            common_name: spec.common_name,
            serial,
            cert_pem,
            key_pem,
            chain_pem,
        })
    }

    /// 对任意字节签名，返回 64 字节 `r ‖ s`。
    pub fn sign(&self, message: &[u8]) -> Result<[u8; 64], CaError> {
        self.sign_approval(message)
    }

    pub fn verify(&self, message: &[u8], signature: &[u8]) -> Result<bool, CaError> {
        self.verify_approval(message, signature)
    }

    pub fn sign_approval(&self, message: &[u8]) -> Result<[u8; 64], CaError> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let msg_name = format!("approval-{counter}.msg");
        let sig_name = format!("approval-{counter}.der");
        fs::write(self.dir.join(&msg_name), message)
            .map_err(|error| CaError::new(format!("write signing message: {error}")))?;
        run(
            &self.dir,
            &[
                "dgst",
                "-sha256",
                "-sign",
                "approval.key",
                "-out",
                &sig_name,
                &msg_name,
            ],
        )?;
        let der = fs::read(self.dir.join(&sig_name))
            .map_err(|error| CaError::new(format!("read signature: {error}")))?;
        let signature = der_to_p1363(&der)?;
        let _ = fs::remove_file(self.dir.join(&msg_name));
        let _ = fs::remove_file(self.dir.join(&sig_name));
        Ok(signature)
    }

    pub fn sign_approval_base64url(&self, message: &[u8]) -> Result<String, CaError> {
        let signature = self.sign_approval(message)?;
        Ok(base64url(&signature))
    }

    pub fn verify_approval(&self, message: &[u8], signature: &[u8]) -> Result<bool, CaError> {
        if signature.len() != 64 {
            return Ok(false);
        }
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let msg_name = format!("verify-{counter}.msg");
        let sig_name = format!("verify-{counter}.der");
        fs::write(self.dir.join(&msg_name), message)
            .map_err(|error| CaError::new(format!("write verify message: {error}")))?;
        let der = p1363_to_der(signature)?;
        fs::write(self.dir.join(&sig_name), der)
            .map_err(|error| CaError::new(format!("write verify signature: {error}")))?;
        let output = Command::new("openssl")
            .current_dir(&self.dir)
            .args([
                "dgst",
                "-sha256",
                "-verify",
                "approval.pub.pem",
                "-signature",
                &sig_name,
                &msg_name,
            ])
            .output()
            .map_err(|error| CaError::new(format!("run openssl verify: {error}")))?;
        let _ = fs::remove_file(self.dir.join(&msg_name));
        let _ = fs::remove_file(self.dir.join(&sig_name));
        Ok(output.status.success())
    }

    fn gen_key(&self, name: &str) -> Result<(), CaError> {
        run(
            &self.dir,
            &[
                "genpkey",
                "-algorithm",
                "EC",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-out",
                name,
            ],
        )?;
        set_mode(&self.dir.join(name), 0o600)?;
        Ok(())
    }

    fn gen_csr(&self, key: &str, out: &str, common_name: &str) -> Result<(), CaError> {
        run(
            &self.dir,
            &[
                "req",
                "-new",
                "-key",
                key,
                "-out",
                out,
                "-subj",
                &format!("/CN={common_name}"),
            ],
        )
    }

    fn ensure_foreign_root(&self) -> Result<(), CaError> {
        if self.dir.join("foreign.crt").exists() {
            return Ok(());
        }
        self.gen_key("foreign.key")?;
        run(
            &self.dir,
            &[
                "req",
                "-x509",
                "-new",
                "-key",
                "foreign.key",
                "-out",
                "foreign.crt",
                "-days",
                "365",
                "-sha256",
                "-subj",
                "/CN=Foreign Test Root",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
                "-addext",
                "subjectKeyIdentifier=hash",
            ],
        )
    }

    fn spki_from_cert(&self, cert_pem: &str) -> Result<Vec<u8>, CaError> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let cert_name = format!("spki-{counter}.crt");
        let key_name = format!("spki-{counter}.pem");
        let der_name = format!("spki-{counter}.der");
        fs::write(self.dir.join(&cert_name), cert_pem)
            .map_err(|error| CaError::new(format!("write spki certificate: {error}")))?;
        run(
            &self.dir,
            &[
                "x509", "-in", &cert_name, "-pubkey", "-noout", "-out", &key_name,
            ],
        )?;
        run(
            &self.dir,
            &[
                "pkey", "-pubin", "-in", &key_name, "-outform", "DER", "-out", &der_name,
            ],
        )?;
        let der = fs::read(self.dir.join(&der_name))
            .map_err(|error| CaError::new(format!("read spki: {error}")))?;
        let _ = fs::remove_file(self.dir.join(&cert_name));
        let _ = fs::remove_file(self.dir.join(&key_name));
        let _ = fs::remove_file(self.dir.join(&der_name));
        Ok(der)
    }
}

impl Drop for TestCA {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "gantry-testca-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), CaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| CaError::new(format!("set permissions on {path:?}: {error}")))?;
    }
    Ok(())
}

fn read_string(path: &Path) -> Result<String, CaError> {
    fs::read_to_string(path).map_err(|error| CaError::new(format!("read {path:?}: {error}")))
}

fn read_der(cert_path: &Path) -> Result<Vec<u8>, CaError> {
    let output = Command::new("openssl")
        .args([
            "x509",
            "-in",
            cert_path.to_string_lossy().as_ref(),
            "-outform",
            "DER",
        ])
        .output()
        .map_err(|error| CaError::new(format!("run openssl x509: {error}")))?;
    if !output.status.success() {
        return Err(CaError::new(format!(
            "openssl x509 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn cert_serial(cert_pem: &str) -> Result<String, CaError> {
    let dir = temp_dir();
    fs::create_dir_all(&dir)
        .map_err(|error| CaError::new(format!("create serial dir: {error}")))?;
    let cert = dir.join("cert.pem");
    fs::write(&cert, cert_pem)
        .map_err(|error| CaError::new(format!("write serial cert: {error}")))?;
    let output = Command::new("openssl")
        .args([
            "x509",
            "-in",
            cert.to_string_lossy().as_ref(),
            "-noout",
            "-serial",
        ])
        .output()
        .map_err(|error| CaError::new(format!("run openssl x509 serial: {error}")))?;
    let _ = fs::remove_dir_all(&dir);
    if !output.status.success() {
        return Err(CaError::new("openssl x509 -serial failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .trim()
        .strip_prefix("serial=")
        .unwrap_or(text.trim())
        .to_owned())
}

fn run(dir: &Path, args: &[&str]) -> Result<(), CaError> {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|error| CaError::new(format!("run openssl {:?}: {error}", args.first())))?;
    if !output.status.success() {
        return Err(CaError::new(format!(
            "openssl {:?} failed: {}",
            args.first(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn approval_public_key_from_spki(spki: &[u8]) -> Result<String, CaError> {
    // P-256 SPKI 末尾是 65 字节未压缩点；只做格式提取，不做曲线运算。
    if spki.len() < 65 || spki[spki.len() - 65] != 0x04 {
        return Err(CaError::new(
            "approval public key is not an uncompressed P-256 point",
        ));
    }
    Ok(base64url(&spki[spki.len() - 65..]))
}

fn der_to_p1363(der: &[u8]) -> Result<[u8; 64], CaError> {
    let mut index = 0;
    expect_tag(der, &mut index, 0x30)?;
    let _length = read_len(der, &mut index)?;
    expect_tag(der, &mut index, 0x02)?;
    let r_len = read_len(der, &mut index)?;
    let r = take(der, &mut index, r_len)?;
    expect_tag(der, &mut index, 0x02)?;
    let s_len = read_len(der, &mut index)?;
    let s = take(der, &mut index, s_len)?;
    if index != der.len() {
        return Err(CaError::new("unexpected trailing bytes in ECDSA signature"));
    }
    let mut out = [0u8; 64];
    copy_integer(r, &mut out[..32])?;
    copy_integer(s, &mut out[32..])?;
    Ok(out)
}

fn p1363_to_der(signature: &[u8]) -> Result<Vec<u8>, CaError> {
    if signature.len() != 64 {
        return Err(CaError::new("ECDSA signature must be 64 bytes"));
    }
    let r = integer_bytes(&signature[..32]);
    let s = integer_bytes(&signature[32..]);
    let mut content = Vec::new();
    content.push(0x02);
    content.push(r.len() as u8);
    content.extend_from_slice(&r);
    content.push(0x02);
    content.push(s.len() as u8);
    content.extend_from_slice(&s);
    let mut der = Vec::with_capacity(content.len() + 2);
    der.push(0x30);
    der.push(content.len() as u8);
    der.extend_from_slice(&content);
    Ok(der)
}

fn integer_bytes(value: &[u8]) -> Vec<u8> {
    let mut start = 0;
    while start + 1 < value.len() && value[start] == 0 {
        start += 1;
    }
    let trimmed = &value[start..];
    if trimmed[0] & 0x80 != 0 {
        let mut out = Vec::with_capacity(trimmed.len() + 1);
        out.push(0);
        out.extend_from_slice(trimmed);
        out
    } else {
        trimmed.to_vec()
    }
}

fn copy_integer(value: &[u8], out: &mut [u8]) -> Result<(), CaError> {
    let mut start = 0;
    while start + 1 < value.len() && value[start] == 0 {
        start += 1;
    }
    let value = &value[start..];
    if value.len() > out.len() {
        return Err(CaError::new("ECDSA integer is wider than 32 bytes"));
    }
    let offset = out.len() - value.len();
    out[offset..].copy_from_slice(value);
    Ok(())
}

fn expect_tag(bytes: &[u8], index: &mut usize, tag: u8) -> Result<(), CaError> {
    let Some(byte) = bytes.get(*index) else {
        return Err(CaError::new("truncated ECDSA signature"));
    };
    if *byte != tag {
        return Err(CaError::new("unexpected ECDSA signature tag"));
    }
    *index += 1;
    Ok(())
}

fn read_len(bytes: &[u8], index: &mut usize) -> Result<usize, CaError> {
    let Some(first) = bytes.get(*index).copied() else {
        return Err(CaError::new("truncated ECDSA length"));
    };
    *index += 1;
    if first & 0x80 == 0 {
        return Ok(first as usize);
    }
    let count = (first & 0x7f) as usize;
    if count == 0 || count > 2 {
        return Err(CaError::new("unsupported ECDSA length encoding"));
    }
    let mut length = 0usize;
    for _ in 0..count {
        let Some(byte) = bytes.get(*index).copied() else {
            return Err(CaError::new("truncated ECDSA length"));
        };
        *index += 1;
        length = (length << 8) | byte as usize;
    }
    Ok(length)
}

fn take<'a>(bytes: &'a [u8], index: &mut usize, length: usize) -> Result<&'a [u8], CaError> {
    let start = *index;
    let end = start
        .checked_add(length)
        .ok_or_else(|| CaError::new("ECDSA length overflow"))?;
    if end > bytes.len() {
        return Err(CaError::new("truncated ECDSA integer"));
    }
    *index = end;
    Ok(&bytes[start..end])
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let value = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(value & 0x3f) as usize] as char);
        }
    }
    out
}
