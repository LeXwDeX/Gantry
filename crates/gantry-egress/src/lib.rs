#![forbid(unsafe_code)]

//! 出网代理 `gantry-egress`（`docs/modules/egress.md`；控制面契约
//! `docs/interfaces/egress-control.md`、`docs/interfaces/egress-call-terminal.md`）。
//!
//! 本切片落地纯规则层，不做任何 IO：主机规范化与硬性拒绝表（`host`）、拒绝网段
//! （`netdeny`）、配置视图的防御性复核（`review`）、能力令牌的铸造与哈希（`token`）。
//! 登记表与控制面、模型反向代理、`CONNECT` 隧道、审计与进程入口在后续切片落地（#94）。

pub mod db;
pub mod host;
pub mod netdeny;
pub mod registry;
pub mod review;
pub mod token;

pub use host::{parse_connect_authority, AllowPattern, Host, HostDenial};
pub use netdeny::{Cidr, DenyNets};
pub use review::{review, ProviderUpstream, ReviewedConfig};
pub use token::{mint_job_token, mint_svc_token, OsEntropy, TokenHash};
