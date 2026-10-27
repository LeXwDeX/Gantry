#![forbid(unsafe_code)]

//! 出网代理 `gantry-egress`（`docs/modules/egress.md`；控制面契约
//! `docs/interfaces/egress-control.md`、`docs/interfaces/egress-call-terminal.md`）。
//!
//! 已落地：纯规则层（`host` 主机规范化与硬性拒绝表、`netdeny` 拒绝网段、`review` 配置视图
//! 防御性复核、`token` 能力令牌铸造与哈希）、登记表（`registry`，持久化在 `db`）、出网审计
//! （`audit`）、控制面服务（`control`）、模型反向代理（`model_proxy`，上游连接在 `upstream`，用量解析在
//! `usage`）、包仓库 `CONNECT` 白名单（`connect`）。控制面客户端与进程入口在后续切片落地（#94）。

pub mod audit;
pub mod connect;
pub mod control;
pub mod db;
pub mod host;
pub mod model_proxy;
pub mod netdeny;
pub mod registry;
pub mod review;
pub mod token;
pub mod upstream;
pub mod usage;

pub use host::{parse_connect_authority, AllowPattern, Host, HostDenial};
pub use netdeny::{Cidr, DenyNets};
pub use review::{review, ProviderUpstream, ReviewedConfig};
pub use token::{mint_job_token, mint_svc_token, OsEntropy, TokenHash};
