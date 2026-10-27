//! 交付单元执行体（engine.md §7.1、§7.14.1，#71）。
//!
//! 每个输入：拿写租约 → 折叠 `DeliveryState` → `decide` → 事务内执行 `Append` / `Raise` /
//! `Resolve` / `Enqueue` → 提交 → 发起副作用。只要本轮产出过事务内决策，就以 `Tick(Settle)`
//! 再求值，直到某轮不再产出；超过 `MAX_SETTLE` 轮视为引擎缺陷，执行体进入 degraded，
//! 不写事件、不上报（§7.14.1 第 4 步）。
//!
//! 读状态的方式：先 `begin_write` 拿到单写者租约，再从已提交日志折叠（[`load_delivery_state`]）。
//! 租约期间没有别人能提交，读到的正是本事务开始时的状态，与 `tx.read()` 等价（§7.1，#71）。
//!
//! 执行体内存（不持久化，重启后为空，由 `Tick(Recover)` 补齐）：在途集合、本进程判过 `Deny`
//! 的意图、`lock_owner`、提议原文表。
//!
//! 提议原文预取（§7.14.1 第 0 步、§7.14.2）：每轮折叠后若状态里有表中尚无的 `ProposalRef`，
//! 先回滚、放开租约，从 CAS 预取后再重开本轮；`decide` 本身不读 CAS。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{Actor, ActorKind, PullRequestRef};
use gantry_core::ids::{DeliveryId, NodeId, ProjectId, Ulid, WorktreeId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::forge::{EnqueueError, OutboxPort};
use gantry_core::ports::inbox::{InboxPort, RaiseError, ResolveError};
use gantry_core::ports::policy::{ActionDecision, DenyReason};
use gantry_store::{Store, StoreError};

use crate::attempt::{fetch_proposal_text, ResultCas};
use crate::decide::{
    CommandRejection, DecideEnv, DecideSeed, Decision, Effect, EffectKey, EffectReport,
    EngineConsts, EngineSettings, Input, InputIdentity, ModelTable, ObservedData, ProposalRef,
    ProposalTexts, ReservationPolicy, TemplateRegistry, TickReason,
};
use crate::decider::decide;
use crate::engine::{load_delivery_state, EngineError};
use crate::state::{ActionKey, DeliveryState};

/// 发起副作用。结果能马上拿到的返回 `Some`；长时副作用（`RunAttempt`）返回 `None`，
/// 结果之后经 [`DeliveryActor::handle`] 以 `Input::Report` 回流。
#[async_trait]
pub trait EffectRunner: Send + Sync {
    async fn run(
        &self,
        state: &DeliveryState,
        effect: Effect,
        ctx: &ActorCtx,
    ) -> Option<EffectReport>;
}

/// PR 快照的只读来源（§7.14.4 `pr_snapshot`）：由加载方从 store 的 GitHub 快照投影读取（#150）。
#[async_trait]
pub trait PrSnapshots: Send + Sync {
    async fn pull_request(&self, project: &ProjectId, number: u64) -> Option<PullRequestRef>;
}

/// 决策环境中两次决策之间可整体切换的部分（§7.13 热加载）。
#[derive(Clone)]
pub struct ActorConfig {
    pub settings: EngineSettings,
    pub models: ModelTable,
    pub templates: TemplateRegistry,
    pub reservation: Arc<dyn ReservationPolicy>,
    pub sandbox_capacity: u32,
    pub consts: EngineConsts,
}

#[derive(Clone)]
pub struct ActorDeps {
    pub store: Store,
    pub inbox: Arc<dyn InboxPort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub effects: Arc<dyn EffectRunner>,
    pub snapshots: Arc<dyn PrSnapshots>,
    /// 提议原文预取读的结果对象存储（§7.14.2）。
    pub cas: Arc<dyn ResultCas>,
}

/// 预取时单个 CAS 对象（结果清单或提议原文）的读取上限。
pub const PROPOSAL_FETCH_MAX_BYTES: usize = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("inbox raise: {0}")]
    Raise(#[from] RaiseError),
    #[error("inbox resolve: {0}")]
    Resolve(#[from] ResolveError),
    #[error("outbox enqueue: {0}")]
    Enqueue(#[from] EnqueueError),
    #[error("delivery not found")]
    NotFound,
    /// 收敛超过 `MAX_SETTLE` 轮：引擎缺陷，停止为该交付单元决策（§7.14.1）。
    #[error("did not converge within MAX_SETTLE rounds")]
    Degraded,
}

/// 一轮求值的结果。
struct Round {
    now: gantry_core::time::Timestamp,
    transactional: bool,
    effects: Vec<Effect>,
    replies: Vec<Result<(), CommandRejection>>,
    state: DeliveryState,
}

pub struct DeliveryActor {
    id: DeliveryId,
    deps: ActorDeps,
    config: ActorConfig,
    inflight: BTreeSet<EffectKey>,
    denied: BTreeSet<ActionKey>,
    lock_owner: BTreeMap<WorktreeId, NodeId>,
    proposals: ProposalTexts,
    degraded: bool,
    requests: u64,
}

impl DeliveryActor {
    pub fn new(id: DeliveryId, deps: ActorDeps, config: ActorConfig) -> Self {
        Self {
            id,
            deps,
            config,
            inflight: BTreeSet::new(),
            denied: BTreeSet::new(),
            lock_owner: BTreeMap::new(),
            proposals: ProposalTexts::default(),
            degraded: false,
            requests: 0,
        }
    }

    pub fn id(&self) -> DeliveryId {
        self.id
    }

    pub fn inflight(&self) -> &BTreeSet<EffectKey> {
        &self.inflight
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// 运行时判定该交付单元遇到引擎缺陷（如入队被端口拒绝）：停止为它决策（§7.14.1）。
    pub fn degrade(&mut self) {
        self.degraded = true;
    }

    /// 两次决策之间整体切换配置（§7.13）；设置变化后清空本进程的 `Deny` 记录（§7.14.1）。
    pub fn reconfigure(&mut self, config: ActorConfig) {
        self.config = config;
        self.denied.clear();
    }

    /// 执行体预取的提议原文（#125）。
    pub fn proposals_mut(&mut self) -> &mut ProposalTexts {
        &mut self.proposals
    }

    /// 处理一个输入及其引发的收敛与同步回报，返回命令的回复。
    pub async fn handle(
        &mut self,
        input: Input,
        identity: InputIdentity,
    ) -> Result<Vec<Result<(), CommandRejection>>, ActorError> {
        if self.degraded {
            return Err(ActorError::Degraded);
        }
        let mut replies = Vec::new();
        let mut queue = VecDeque::from([(input, identity)]);
        while let Some((input, identity)) = queue.pop_front() {
            self.note_input(&input, &identity);
            let mut current = (input, identity);
            let mut rounds = 0;
            loop {
                let round = self.round(&current.0, &current.1).await?;
                replies.extend(round.replies);
                for (report, key) in self.launch(&round.state, round.effects, round.now).await {
                    queue.push_back((Input::Report(report), InputIdentity::Report(key)));
                }
                if !round.transactional {
                    break;
                }
                rounds += 1;
                if rounds >= self.config.consts.max_settle {
                    // 引擎缺陷：status 报 degraded；不写事件、不上报（P-79）。
                    self.degraded = true;
                    return Err(ActorError::Degraded);
                }
                current = (
                    Input::Tick(TickReason::Settle),
                    InputIdentity::Tick(TickReason::Settle),
                );
            }
        }
        Ok(replies)
    }

    /// 输入到达时更新执行体内存：回报与定时器把对应键移出在途集合（§7.14.1 第 3 步）。
    fn note_input(&mut self, input: &Input, identity: &InputIdentity) {
        match identity {
            InputIdentity::Report(key) => {
                self.inflight.remove(key);
            }
            InputIdentity::Tick(TickReason::Timer { kind, .. }) => {
                self.inflight.remove(&EffectKey::Timer(kind.clone()));
            }
            _ => {}
        }
        match input {
            Input::Report(EffectReport::LockAcquired { node_id, token }) => {
                self.lock_owner.insert(token.worktree_id, *node_id);
            }
            Input::Report(EffectReport::ActionJudged {
                key,
                decision: ActionDecision::Deny { reason, .. },
            }) if *reason != DenyReason::HeadMismatch => {
                self.denied.insert(key.clone());
            }
            Input::Event(observed) => {
                if let ObservedData::SettingsUpdated { .. } = observed.data {
                    self.denied.clear();
                }
            }
            _ => {}
        }
    }

    fn ctx(&mut self, now_ms: u64) -> ActorCtx {
        self.requests += 1;
        ActorCtx {
            actor: Actor::new(ActorKind::Engine, "engine"),
            device: None,
            role: None,
            client_profile: None,
            request_id: Ulid::from_parts(now_ms, u128::from(self.requests))
                .expect("millisecond clock fits in 48 bits"),
            idempotency_key: None,
            dedup_key: None,
            causation_id: None,
        }
    }

    /// 一轮：拿写租约 → 折叠 → 决策 → 事务内执行 → 提交。任何一步失败整笔回滚、不发副作用、不回复。
    async fn round(
        &mut self,
        input: &Input,
        identity: &InputIdentity,
    ) -> Result<Round, ActorError> {
        let store = self.deps.store.clone();
        let mut tried = BTreeSet::new();
        let (mut tx, mut state) = loop {
            let tx = store.begin_write().await?;
            let Some(state) = load_delivery_state(&store, &self.id).await? else {
                tx.rollback().await?;
                return Err(ActorError::NotFound);
            };
            let missing: Vec<ProposalRef> = state
                .workflows
                .values()
                .flat_map(|w| w.nodes.values())
                .flat_map(|n| n.attempts.iter())
                .filter_map(ProposalRef::of)
                .filter(|key| !self.proposals.contains(key) && !tried.contains(key))
                .collect();
            if missing.is_empty() {
                break (tx, state);
            }
            // 第 0 步：不在租约内读 CAS。瞬时错误不登记，本轮 create_pr 不进期望集合，
            // 下一个输入再取（§7.14.2）。
            tx.rollback().await?;
            for key in missing {
                let fetched =
                    fetch_proposal_text(self.deps.cas.as_ref(), &key, PROPOSAL_FETCH_MAX_BYTES)
                        .await;
                if let Ok(text) = fetched {
                    self.proposals.insert(key.clone(), text);
                }
                tried.insert(key);
            }
        };
        state.lock_owner = self.lock_owner.clone();
        if let Some(pr) = &state.pr {
            state.pr_snapshot = self
                .deps
                .snapshots
                .pull_request(&state.delivery.project_id, pr.number)
                .await;
        }
        // 事务开始时的时钟值：同一轮内固定，且不早于最后一条事件（§7.14.3 `now`）。
        let now = tx.now();
        let config = self.config.clone();
        let env = DecideEnv {
            now,
            seed: DecideSeed::derive(&self.id, identity, state.last_seq),
            settings: &config.settings,
            models: &config.models,
            templates: &config.templates,
            reservation: config.reservation.as_ref(),
            sandbox_capacity: config.sandbox_capacity,
            inflight: &self.inflight,
            denied: &self.denied,
            proposals: &self.proposals,
            consts: config.consts,
        };
        let decisions = decide(&state, input, &env);
        let ctx = self.ctx(u64::try_from(now.as_millis()).unwrap_or(0));
        let mut round = Round {
            now,
            transactional: false,
            effects: Vec::new(),
            replies: Vec::new(),
            state,
        };
        for decision in decisions {
            match decision {
                Decision::Append(event) => {
                    round.transactional = true;
                    tx.append(event).await?;
                }
                Decision::Raise(request) => {
                    round.transactional = true;
                    self.deps.inbox.raise(Some(&mut tx), request, &ctx).await?;
                }
                Decision::Resolve { target, how } => {
                    round.transactional = true;
                    self.deps
                        .inbox
                        .resolve(Some(&mut tx), target, how, &ctx)
                        .await?;
                }
                Decision::Enqueue(request) => {
                    round.transactional = true;
                    self.deps.outbox.enqueue_in(&mut tx, request, &ctx).await?;
                }
                Decision::Reply(reply) => round.replies.push(reply),
                Decision::Effect(effect) => round.effects.push(effect),
            }
        }
        tx.commit().await?;
        Ok(round)
    }

    /// 提交后发起副作用：先记入在途集合再发起（§7.14.1 第 3 步）；同一键在途时不重复发起。
    async fn launch(
        &mut self,
        state: &DeliveryState,
        effects: Vec<Effect>,
        now: gantry_core::time::Timestamp,
    ) -> Vec<(EffectReport, EffectKey)> {
        let mut reports = Vec::new();
        for effect in effects {
            let key = effect.key(self.id);
            if !self.inflight.insert(key.clone()) {
                continue;
            }
            // 释放锁时清除 lock_owner：自动重试必须重新取锁（§7.14.5 ReleaseLock 行）。
            if let Effect::ReleaseLock { token } = &effect {
                self.lock_owner.remove(&token.worktree_id);
            }
            let ctx = self.ctx(u64::try_from(now.as_millis()).unwrap_or(0));
            if let Some(report) = self.deps.effects.run(state, effect, &ctx).await {
                reports.push((report, key));
            }
        }
        reports
    }
}
