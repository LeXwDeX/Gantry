//! 证书参数（pki.md §7.1、§10）。全部是引导配置的建议默认值，集中在这里，不散落。

use std::time::Duration;

const DAY: u64 = 24 * 60 * 60;

/// 有效期与时钟余量。算法（ECDSA P-256 / `ecdsa-with-SHA256`）是强制项，不可配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertPolicy {
    /// `pki.root_validity`，仅 `bootstrap`。
    pub root_validity: Duration,
    /// `pki.intermediate_validity`。
    pub intermediate_validity: Duration,
    /// `pki.server_cert_validity`。
    pub server_cert_validity: Duration,
    /// `pki.server_cert_renew_before`：剩余不超过此值时自动续签。
    pub server_cert_renew_before: Duration,
    /// `pki.client_cert_validity`。
    pub client_cert_validity: Duration,
    /// `pki.not_before_skew`：`notBefore = 签发时刻 − skew`。
    pub not_before_skew: Duration,
}

impl Default for CertPolicy {
    fn default() -> Self {
        Self {
            root_validity: Duration::from_secs(3650 * DAY),
            intermediate_validity: Duration::from_secs(730 * DAY),
            server_cert_validity: Duration::from_secs(90 * DAY),
            server_cert_renew_before: Duration::from_secs(30 * DAY),
            client_cert_validity: Duration::from_secs(30 * DAY),
            not_before_skew: Duration::from_secs(5 * 60),
        }
    }
}
