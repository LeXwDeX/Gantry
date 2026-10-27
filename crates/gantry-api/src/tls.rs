//! TLS 终止加固（本文 §7.2，api.md §1.2）：构造 `rustls::ServerConfig` 时的硬性要求。
//!
//! | 项 | 要求 | 理由 |
//! | --- | --- | --- |
//! | 协议 | 仅 TLS 1.3 | 契约 |
//! | 客户端证书 | `pki` 的校验器，强制 | 失败即关闭：未配对者在握手层被挡住 |
//! | 会话恢复 | 关闭（不发 ticket、无会话存储） | 恢复会话不重新校验客户端证书，会让已吊销设备继续连上 |
//! | 0-RTT | 关闭 | 早期数据可重放，会放大非幂等 POST |
//! | ALPN | `h2`、`http/1.1` | 契约 |
//!
//! 配对监听器：单向 TLS、同一服务端证书、仅 HTTP/1.1。

use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::server::danger::ClientCertVerifier;
use rustls::server::{NoServerSessionStorage, ResolvesServerCert};
use rustls::time_provider::TimeProvider;
use rustls::ServerConfig;

/// mTLS API 监听器的配置。
pub fn api_server_config(
    provider: Arc<CryptoProvider>,
    time: Arc<dyn TimeProvider>,
    verifier: Arc<dyn ClientCertVerifier>,
    certs: Arc<dyn ResolvesServerCert>,
) -> Result<ServerConfig, rustls::Error> {
    let mut config = ServerConfig::builder_with_details(provider, time)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(certs);
    harden(&mut config);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// 配对监听器的配置：单向 TLS，仅 HTTP/1.1（本文 §7.2）。
pub fn pairing_server_config(
    provider: Arc<CryptoProvider>,
    time: Arc<dyn TimeProvider>,
    certs: Arc<dyn ResolvesServerCert>,
) -> Result<ServerConfig, rustls::Error> {
    let mut config = ServerConfig::builder_with_details(provider, time)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(certs);
    harden(&mut config);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn harden(config: &mut ServerConfig) {
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.max_early_data_size = 0;
}
