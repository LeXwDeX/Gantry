//! 出站连接（`docs/modules/egress.md` §7.3 步 7、§7.5 步 8）：自己解析主机的全部 A / AAAA，
//! 任一落点在拒绝网段即拒绝；否则钉住第一个落点建连（不二次解析，防 DNS 重绑定）。
//! 模型上游再套 TLS（证书与主机名验证、SNI = provider 主机、不降级）。
//!
//! 解析器与拨号器是注入的：生产用系统解析与 TCP，测试用 `FakeResolver` 与把公网地址
//! 映射到本机模拟服务的拨号器，从而在不出网的情况下覆盖重绑定与钉住。

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::host::Host;
use crate::netdeny::DenyNets;

#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &Host) -> io::Result<Vec<IpAddr>>;
}

#[async_trait]
pub trait Dialer: Send + Sync {
    async fn dial(&self, addr: SocketAddr) -> io::Result<TcpStream>;
}

/// 系统解析器（tokio 的 `getaddrinfo` 线程池）。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &Host) -> io::Result<Vec<IpAddr>> {
        Ok(tokio::net::lookup_host((host.as_str(), 0))
            .await?
            .map(|addr| addr.ip())
            .collect())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TcpDialer;

#[async_trait]
impl Dialer for TcpDialer {
    async fn dial(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect(addr).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// 解析结果落在拒绝网段（`hard.private_ip`）。
    #[error("resolved to a denied address")]
    Denied,
    #[error("no addresses for host")]
    NoAddress,
    #[error("connect timed out")]
    Timeout,
    #[error("connect failed: {0}")]
    Io(#[from] io::Error),
    #[error("tls handshake failed: {0}")]
    Tls(io::Error),
}

/// 解析 → 校验落点 → 钉住 → 建连。
#[derive(Clone)]
pub struct Connector {
    resolver: Arc<dyn Resolver>,
    dialer: Arc<dyn Dialer>,
    deny: DenyNets,
    timeout: Duration,
}

impl Connector {
    pub fn new(
        resolver: Arc<dyn Resolver>,
        dialer: Arc<dyn Dialer>,
        deny: DenyNets,
        timeout: Duration,
    ) -> Self {
        Self {
            resolver,
            dialer,
            deny,
            timeout,
        }
    }

    /// 返回连接与实际连上的 IP（审计 `resolved_ip`）。
    pub async fn connect(
        &self,
        host: &Host,
        port: u16,
    ) -> Result<(TcpStream, IpAddr), ConnectError> {
        let addrs = tokio::time::timeout(self.timeout, self.resolver.resolve(host))
            .await
            .map_err(|_| ConnectError::Timeout)??;
        if addrs.iter().any(|ip| self.deny.is_denied(*ip)) {
            return Err(ConnectError::Denied);
        }
        let ip = *addrs.first().ok_or(ConnectError::NoAddress)?;
        let stream =
            tokio::time::timeout(self.timeout, self.dialer.dial(SocketAddr::new(ip, port)))
                .await
                .map_err(|_| ConnectError::Timeout)??;
        Ok((stream, ip))
    }
}

/// 建 TLS 客户端配置：只信任给定的根证书集合（生产为系统根证书）。
pub fn tls_config(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// 系统根证书（`rustls-native-certs`）；读不到任何根证书时返回空集合（之后的握手全部失败，
/// 失败闭合）。
pub fn system_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    roots
}

/// PEM 根证书（测试 CA 或部署指定的额外根）。
pub fn roots_from_pem(pem: &str) -> io::Result<rustls::RootCertStore> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()) {
        let cert = cert.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        roots
            .add(cert)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    }
    Ok(roots)
}

/// 与 provider 主机建立已验证的 TLS 连接。
pub async fn connect_tls(
    connector: &Connector,
    tls: &Arc<rustls::ClientConfig>,
    host: &Host,
) -> Result<(TlsStream<TcpStream>, IpAddr), ConnectError> {
    let (tcp, ip) = connector.connect(host, 443).await?;
    let name = ServerName::try_from(host.as_str().to_owned())
        .map_err(|e| ConnectError::Tls(io::Error::new(io::ErrorKind::InvalidInput, e)))?;
    let stream = tokio::time::timeout(
        connector.timeout,
        TlsConnector::from(tls.clone()).connect(name, tcp),
    )
    .await
    .map_err(|_| ConnectError::Timeout)?
    .map_err(ConnectError::Tls)?;
    Ok((stream, ip))
}
