//! `FakeEngine`：记录调用、按脚本返回，不重现引擎逻辑。
//!
//! 三个端口一起实现：`DeliveryControl`、`HealPort`、`ClassifyPort`。每个方法
//! 都记录 `ActorCtx`；默认返回「未脚本化」错误，避免测试把假成功当成真行为。

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use gantry_core::entities::{Delivery, Node};
use gantry_core::ids::{DeliveryId, NodeId};
use gantry_core::level::Level;
use gantry_core::ports::context::{ActorCtx, ModuleError};
use gantry_core::ports::engine::{
    CancelWhy, CiClassifyRequest, ClassifyError, ClassifyPort, DeliveryControl, EngineError,
    FindingClassifyRequest, FindingTriage, HealOutcome, HealPort, HealRequest, HealTicket,
};
use gantry_core::worker::CiClassification;

/// 可以单独脚本化返回值的交付控制操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryOp {
    SetLevelCap(DeliveryId),
    Abandon(DeliveryId),
    Pause(DeliveryId),
    Resume(DeliveryId),
    RetryNode(NodeId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineCall {
    SetLevelCap {
        ctx: ActorCtx,
        id: DeliveryId,
        cap: Level,
    },
    Abandon {
        ctx: ActorCtx,
        id: DeliveryId,
        reason: Option<String>,
    },
    Pause {
        ctx: ActorCtx,
        id: DeliveryId,
    },
    Resume {
        ctx: ActorCtx,
        id: DeliveryId,
    },
    RetryNode {
        ctx: ActorCtx,
        id: NodeId,
    },
    RequestHeal {
        req: HealRequest,
        ctx: ActorCtx,
    },
    HealOutcome {
        ticket: HealTicket,
    },
    HealCancel {
        ticket: HealTicket,
        why: CancelWhy,
        ctx: ActorCtx,
    },
    ClassifyCi {
        req: CiClassifyRequest,
        ctx: ActorCtx,
    },
    ClassifyFinding {
        req: FindingClassifyRequest,
        ctx: ActorCtx,
    },
}

impl EngineCall {
    pub fn op(&self) -> &'static str {
        match self {
            Self::SetLevelCap { .. } => "set_level_cap",
            Self::Abandon { .. } => "abandon",
            Self::Pause { .. } => "pause",
            Self::Resume { .. } => "resume",
            Self::RetryNode { .. } => "retry_node",
            Self::RequestHeal { .. } => "request_heal",
            Self::HealOutcome { .. } => "heal_outcome",
            Self::HealCancel { .. } => "heal_cancel",
            Self::ClassifyCi { .. } => "classify_ci",
            Self::ClassifyFinding { .. } => "classify_finding",
        }
    }
}

#[derive(Default)]
struct State {
    delivery_scripts: BTreeMap<DeliveryOp, Result<Delivery, ModuleError>>,
    retry_script: Option<Result<Node, ModuleError>>,
    heal_ticket_script: Option<Result<HealTicket, EngineError>>,
    heal_outcome_script: Option<Result<Option<HealOutcome>, EngineError>>,
    ci_script: Option<Result<CiClassification, ClassifyError>>,
    finding_script: Option<Result<FindingTriage, ClassifyError>>,
    calls: Vec<EngineCall>,
}

/// M0 的引擎替身。
#[derive(Default)]
pub struct FakeEngine {
    state: Mutex<State>,
}

impl FakeEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn script_delivery(&self, op: DeliveryOp, delivery: Delivery) {
        self.script_delivery_result(op, Ok(delivery));
    }

    pub fn script_delivery_error(&self, op: DeliveryOp, error: ModuleError) {
        self.script_delivery_result(op, Err(error));
    }

    fn script_delivery_result(&self, op: DeliveryOp, result: Result<Delivery, ModuleError>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.delivery_scripts.insert(op, result);
    }

    pub fn script_retry_node(&self, node: Node) {
        self.retry_script(Ok(node));
    }

    pub fn script_retry_node_error(&self, error: ModuleError) {
        self.retry_script(Err(error));
    }

    fn retry_script(&self, result: Result<Node, ModuleError>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retry_script = Some(result);
    }

    pub fn script_heal_ticket(&self, ticket: HealTicket) {
        self.heal_ticket_result(Ok(ticket));
    }

    pub fn script_heal_error(&self, error: EngineError) {
        self.heal_ticket_result(Err(error));
    }

    fn heal_ticket_result(&self, result: Result<HealTicket, EngineError>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .heal_ticket_script = Some(result);
    }

    pub fn script_heal_outcome(&self, outcome: Option<HealOutcome>) {
        self.heal_outcome_result(Ok(outcome));
    }

    pub fn script_heal_outcome_error(&self, error: EngineError) {
        self.heal_outcome_result(Err(error));
    }

    fn heal_outcome_result(&self, result: Result<Option<HealOutcome>, EngineError>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .heal_outcome_script = Some(result);
    }

    pub fn script_ci(&self, classification: CiClassification) {
        self.ci_result(Ok(classification));
    }

    pub fn script_ci_error(&self, error: ClassifyError) {
        self.ci_result(Err(error));
    }

    fn ci_result(&self, result: Result<CiClassification, ClassifyError>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ci_script = Some(result);
    }

    pub fn script_finding(&self, triage: FindingTriage) {
        self.finding_result(Ok(triage));
    }

    pub fn script_finding_error(&self, error: ClassifyError) {
        self.finding_result(Err(error));
    }

    fn finding_result(&self, result: Result<FindingTriage, ClassifyError>) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .finding_script = Some(result);
    }

    pub fn calls(&self) -> Vec<EngineCall> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .calls
            .clone()
    }
}

#[async_trait]
impl DeliveryControl for FakeEngine {
    async fn set_level_cap(
        &self,
        ctx: ActorCtx,
        id: DeliveryId,
        cap: Level,
    ) -> Result<Delivery, ModuleError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::SetLevelCap { ctx, id, cap });
        state
            .delivery_scripts
            .get(&DeliveryOp::SetLevelCap(id))
            .cloned()
            .unwrap_or(Err(ModuleError::NotFound))
    }

    async fn abandon(
        &self,
        ctx: ActorCtx,
        id: DeliveryId,
        reason: Option<String>,
    ) -> Result<Delivery, ModuleError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::Abandon { ctx, id, reason });
        state
            .delivery_scripts
            .get(&DeliveryOp::Abandon(id))
            .cloned()
            .unwrap_or(Err(ModuleError::NotFound))
    }

    async fn pause(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::Pause { ctx, id });
        state
            .delivery_scripts
            .get(&DeliveryOp::Pause(id))
            .cloned()
            .unwrap_or(Err(ModuleError::NotFound))
    }

    async fn resume(&self, ctx: ActorCtx, id: DeliveryId) -> Result<Delivery, ModuleError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::Resume { ctx, id });
        state
            .delivery_scripts
            .get(&DeliveryOp::Resume(id))
            .cloned()
            .unwrap_or(Err(ModuleError::NotFound))
    }

    async fn retry_node(&self, ctx: ActorCtx, id: NodeId) -> Result<Node, ModuleError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::RetryNode { ctx, id });
        state
            .retry_script
            .clone()
            .unwrap_or(Err(ModuleError::NotFound))
    }
}

#[async_trait]
impl HealPort for FakeEngine {
    async fn request_heal(
        &self,
        req: HealRequest,
        ctx: &ActorCtx,
    ) -> Result<HealTicket, EngineError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::RequestHeal {
            req: req.clone(),
            ctx: ctx.clone(),
        });
        if let Some(script) = &state.heal_ticket_script {
            return script.clone();
        }
        Ok(HealTicket {
            round_id: req.round_id,
            delivery_id: req.delivery_id,
            workflow_id: None,
            node_id: None,
        })
    }

    async fn outcome(&self, ticket: &HealTicket) -> Result<Option<HealOutcome>, EngineError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::HealOutcome {
            ticket: ticket.clone(),
        });
        state.heal_outcome_script.clone().unwrap_or(Ok(None))
    }

    async fn cancel(
        &self,
        ticket: &HealTicket,
        why: CancelWhy,
        ctx: &ActorCtx,
    ) -> Result<(), EngineError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::HealCancel {
            ticket: ticket.clone(),
            why,
            ctx: ctx.clone(),
        });
        Ok(())
    }
}

#[async_trait]
impl ClassifyPort for FakeEngine {
    async fn classify_ci(
        &self,
        req: CiClassifyRequest,
        ctx: &ActorCtx,
    ) -> Result<CiClassification, ClassifyError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::ClassifyCi {
            req,
            ctx: ctx.clone(),
        });
        state.ci_script.clone().unwrap_or_else(|| {
            Err(ClassifyError::Unavailable {
                message: "FakeEngine has no scripted ci classification".into(),
            })
        })
    }

    async fn classify_finding(
        &self,
        req: FindingClassifyRequest,
        ctx: &ActorCtx,
    ) -> Result<FindingTriage, ClassifyError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.calls.push(EngineCall::ClassifyFinding {
            req,
            ctx: ctx.clone(),
        });
        state.finding_script.clone().unwrap_or_else(|| {
            Err(ClassifyError::Unavailable {
                message: "FakeEngine has no scripted finding classification".into(),
            })
        })
    }
}
