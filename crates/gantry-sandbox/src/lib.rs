//! Gantry `sandbox`：把一次 Attempt 关进一次性、只读根、无凭据、只能经 Egress
//! 出网的容器里运行（docs/modules/sandbox.md）。
//!
//! 两层接口（P-52）：引擎面 `core::ports::SandboxService` 由本模块实现；容器面
//! [`driver::SandboxDriver`] 只在本模块内部使用，实现有 [`podman_driver::PodmanDriver`]
//! 与测试用的 `fake::FakeDriver`（`testing` feature）。本 crate 目前交付容器面的请求类型
//! [`spec::ContainerSpec`]、参数闸 [`invariants::validate_invariants`]、Podman
//! 参数渲染 [`podman::render_create_args`]，staging 与材料打包 [`stage`]、进度采集 [`progress`]，以及先停后取的产物提取规则 [`intake`]。

pub mod driver;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod intake;
pub mod invariants;
pub mod podman;
pub mod podman_driver;
pub mod progress;
pub mod spec;
pub mod stage;
