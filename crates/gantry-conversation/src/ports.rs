//! 本模块消费的依赖端口（conversation.md §3）。
//!
//! - [`EngineApi`]：engine.md §6 的会话侧命令。`conversation` 依赖 `engine`
//!   （裁决 C13 / P-59），反向不行，所以 trait 定义在消费方：引擎就绪后由本
//!   crate 为 `gantry_engine::Engine` 写适配实现。M1 只用到创建与定位；
//!   `supply_spec` / `run_answer` 随 M2 补上。
//! - [`ModelsPort`]：`models` 的直接调用与"是否已配置"判断；生产实现即
//!   [`ModelRouter`]，测试注入替身。

use async_trait::async_trait;
use gantry_core::entities::{Ref, Stage, Tier};
use gantry_core::ids::{DeliveryId, InboxId, ProjectId};
use gantry_core::level::Level;
use gantry_core::ports::engine::{CreateDelivery, EngineError};
use gantry_models::{DirectCall, DirectOutcome, ModelRouter, ModelsError, Purpose, ResolveQuery};

/// 引擎返回的交付单元摘要（engine.md §6 的 `DeliveryHandle`，字段为最小集）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryHandle {
    pub id: DeliveryId,
    pub project_id: ProjectId,
    pub level_cap: Level,
    pub stage: Stage,
    /// 未决的 `spec_needed`；引擎保证每个交付单元同时至多一个。
    pub open_spec_needed: Option<InboxId>,
}

#[async_trait]
pub trait EngineApi: Send + Sync {
    /// 以 `cmd.source_message_id` 幂等：同一条消息重复调用返回同一个交付单元。
    async fn create_delivery(&self, cmd: CreateDelivery) -> Result<DeliveryHandle, EngineError>;

    /// 给定 refs，返回已绑定且未终结的交付单元（0..n 个）。只读。
    async fn locate_deliveries(
        &self,
        project: &ProjectId,
        refs: &[Ref],
    ) -> Result<Vec<DeliveryHandle>, EngineError>;
}

#[async_trait]
pub trait ModelsPort: Send + Sync {
    /// `models.planner` 与 `models.utility` 是否都已配置（§7.2 P2）。
    async fn configured(&self, project: &ProjectId) -> Result<bool, ModelsError>;

    /// `ModelRouter::invoke_direct`。
    async fn invoke_direct(&self, call: DirectCall) -> Result<DirectOutcome, ModelsError>;
}

#[async_trait]
impl ModelsPort for ModelRouter {
    async fn configured(&self, project: &ProjectId) -> Result<bool, ModelsError> {
        for tier in [Tier::Planner, Tier::Utility] {
            let query = ResolveQuery {
                project_id: *project,
                tier,
                level: None,
                critical: false,
                purpose: Purpose::Direct,
            };
            match self.resolve(&query).await {
                Ok(_) => {}
                Err(ModelsError::NotConfigured { .. }) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    async fn invoke_direct(&self, call: DirectCall) -> Result<DirectOutcome, ModelsError> {
        ModelRouter::invoke_direct(self, call).await
    }
}
