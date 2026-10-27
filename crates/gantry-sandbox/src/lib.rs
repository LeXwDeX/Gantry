//! Gantry `sandbox`：把一次 Attempt 关进一次性、只读根、无凭据、只能经 Egress
//! 出网的容器里运行（docs/modules/sandbox.md）。
//!
//! 两层接口（P-52）：引擎面 `core::ports::SandboxService` 由本模块实现；容器面
//! `SandboxDriver` 只在本模块内部使用。本 crate 目前交付容器面的请求类型
//! [`spec::ContainerSpec`]、参数闸 [`invariants::validate_invariants`] 与 Podman
//! 参数渲染 [`podman::render_create_args`]。

pub mod invariants;
pub mod podman;
pub mod spec;
