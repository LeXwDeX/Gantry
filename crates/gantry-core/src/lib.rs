//! Gantry 领域核心：无 IO 的类型、值对象、实体、状态机与契约基础实现。
#![forbid(unsafe_code)]

pub mod canon;
pub mod entities;
pub mod ids;
pub mod json_strict;
pub mod level;
pub mod machines;
pub mod secret;
pub mod text;
pub mod time;

// 以下模块由并行契约节点填充内容；本节点只保证模块存在且可编译。
pub mod egress;
pub mod events;
pub mod forge;
pub mod ports;
pub mod settings;
pub mod worker;

#[cfg(feature = "testing")]
pub mod testing;

pub use canon::{sha256, GitOid, RepoSlug, Sha256Digest};
pub use entities::*;
pub use ids::*;
pub use level::*;
pub use secret::*;
pub use text::*;
pub use time::*;
