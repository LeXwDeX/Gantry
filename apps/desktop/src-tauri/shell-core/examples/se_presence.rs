//! 手动验证：Secure Enclave 审批签名密钥的 Touch ID 路径（desktop-shell.md §12 真机手测清单）。
//!
//! 需要人在场，CI 不跑。用法见 `apps/desktop/src-tauri/probe/README.md`：
//!   cargo run -p gantry-desktop-shell --example se_presence -- <密钥目录>
//! 第一次运行生成两把密钥；之后再运行（相当于重启应用）复用同一对密钥。

#[cfg(target_os = "macos")]
fn main() {
    use gantry_desktop_shell::keys::{KeyLabels, KeyStore, PresencePrompt, SecureEnclaveKeyStore};
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, VerifyingKey};

    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("用法：se_presence <密钥目录>"),
    );
    let store = SecureEnclaveKeyStore::open(&dir).expect("打开密钥目录");
    let labels_file = dir.join("labels.txt");
    let keys = match std::fs::read_to_string(&labels_file) {
        Ok(text) => {
            let (tls, approval) = text.trim().split_once(' ').expect("labels.txt 格式");
            let labels = KeyLabels {
                tls: tls.into(),
                approval: approval.into(),
            };
            println!("复用已有密钥 {labels:?}");
            store.load(&labels).expect("加载密钥")
        }
        Err(_) => {
            let keys = store.generate().expect("生成密钥");
            let labels = keys.labels();
            std::fs::write(&labels_file, format!("{} {}", labels.tls, labels.approval)).unwrap();
            println!("已生成密钥 {labels:?}");
            keys
        }
    };
    println!(
        "tls_pubkey_fpr      = {}",
        keys.tls_public_key().fingerprint()
    );
    println!(
        "approval_pubkey_fpr = {}",
        keys.approval_public_key().fingerprint()
    );

    keys.sign_tls(b"tls").expect("mTLS 客户端密钥应无提示签名");
    println!("PASS  mTLS 客户端密钥无提示签名");

    let msg = b"gantry/approval/v1\n{\"probe\":true}";
    let prompt = PresencePrompt {
        title: "Gantry 探针".into(),
        summary: "merge acme/web#12 @ 9f2c4e7a1b3d".into(),
        detail: vec![],
    };
    println!("即将弹出 Touch ID，提示文案应含 9f2c4e7a1b3d …");
    match keys.sign_approval(msg, &prompt) {
        Ok(sig) => {
            let vk = VerifyingKey::from_sec1_bytes(keys.approval_public_key().as_bytes()).unwrap();
            vk.verify(msg, &Signature::from_slice(sig.as_bytes()).unwrap())
                .expect("验签");
            println!(
                "PASS  用户在场后审批签名成功且验签通过：{}",
                sig.to_base64url()
            );
        }
        Err(e) => println!("INFO  审批签名未完成：{e:?}（取消应为 UserCancelled）"),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("仅 macOS");
}
