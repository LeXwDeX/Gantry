//! Gantry M0 测试替身。
//!
//! 每个 `core::ports` trait 都有一个内存实现：Forge/Repo/Policy/Inbox/Secrets
//! 覆盖状态语义，Worker/Models/Sandbox/Egress/Engine/Conversation 覆盖运行时
//! 交互；`test_ca` 与 `github` 提供集成测试所需的外部边界替身。
//!
//! 替身只依赖 `core` 的公共类型，不定义与端口重复的类型；真实 IO、Git、
//! GitHub 与证书链不在替身范围内。

pub mod conversation;
pub mod egress;
pub mod engine;
pub mod forge;
pub mod github;
pub mod inbox;
pub mod models;
pub mod policy;
pub mod repo;
pub mod sandbox;
pub mod secrets;
pub mod test_ca;
pub mod worker;
