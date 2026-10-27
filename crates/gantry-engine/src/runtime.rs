//! 引擎运行时入口（engine.md §7.13、§7.15，#277）。
//!
//! `EngineRuntime::new` 组装 `EventRouter`、`DeliveryActor` 池与生产用 `PortEffects`；
//! `run(shutdown)` 是主循环：
//! 1. 启动：对仍需处理的交付单元（非 `closed`，或 `closed` 但工作树未移除）发 `Tick(Recover)`；
//! 2. 循环：处理执行体邮箱里的回报与定时器；从 `engine` 游标轮询事件，按路由表投给执行体；
//!    新出现的 `delivery.created` 启动新执行体并 `Tick(Settle)`；按低水位推进游标；
//! 3. 停机：`shutdown` 完成后不再读新事件，处理完已入队的回报即返回。在途尝试留在沙盒里，
//!    下次启动由 `Tick(Recover)` 以 `resume` 接管（§7.13 第 2 步）。
//!
//! M1 的执行体在主循环里串行处理（交付单元之间也串行）；分发顺序与 seq 一致，正确性不依赖并行。

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use gantry_core::events::EventType;
use gantry_core::ids::{Clock, DeliveryId, Entropy};
use gantry_core::ports::forge::{ForgeReader, OutboxPort};
use gantry_core::ports::inbox::InboxPort;
use gantry_core::ports::policy::PolicyPort;
use gantry_core::ports::repo::RepoService;
use gantry_core::ports::sandbox::SandboxService;
use gantry_core::ports::worker::WorkerAdapter;
use gantry_store::{EventFilter, Store, TypePattern};
use tokio::sync::{mpsc, Mutex};

use crate::actor::{ActorConfig, ActorDeps, ActorError, DeliveryActor, PrSnapshots};
use crate::attempt::{AttemptPorts, ResultCas, RunnerConfig};
use crate::decide::{Input, InputIdentity, TickReason};
use crate::effects::{Deliver, PortEffects};
use crate::engine::{load_delivery_state, scan_events, Engine, EngineError};
use crate::event_router::{EventRouter, RouterError};

/// 运行时需要的全部端口（gantryd 的装配里已有同名字段）。
pub struct EnginePorts {
    pub inbox: Arc<dyn InboxPort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub policy: Arc<dyn PolicyPort>,
    pub forge: Arc<dyn ForgeReader>,
    pub repo: Arc<dyn RepoService>,
    pub sandbox: Arc<dyn SandboxService>,
    pub worker: Arc<dyn WorkerAdapter>,
    /// 读写结果对象（`gantry_store::Cas` 已实现）。
    pub cas: Arc<dyn ResultCas>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    /// PR 快照来源；`None` 时读 store 的 GitHub 快照投影（#150）。
    pub snapshots: Option<Arc<dyn PrSnapshots>>,
}

/// 两次决策之间可整体切换的配置（§7.13 热加载）。
#[derive(Clone)]
pub struct RuntimeSettings {
    pub actor: ActorConfig,
    pub runner: RunnerConfig,
    /// 事件流没有新事件时的轮询间隔。
    pub poll_interval: Duration,
    /// `RemoveWorktree` 在锁已释放时先以 engine 身份取锁所用的租约。
    pub cleanup_lease: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    #[error("router: {0}")]
    Router(#[from] RouterError),
    #[error("actor {delivery}: {source}")]
    Actor {
        delivery: DeliveryId,
        #[source]
        source: ActorError,
    },
}

/// 执行体邮箱里的一条输入（副作用回报、定时器到期）。
struct Mail {
    delivery: DeliveryId,
    input: Input,
    identity: InputIdentity,
}

/// 读 store 的 PR 快照投影。
struct StoreSnapshots {
    store: Store,
    repo: gantry_core::canon::RepoSlug,
}

#[async_trait::async_trait]
impl PrSnapshots for StoreSnapshots {
    async fn pull_request(
        &self,
        _project: &gantry_core::ids::ProjectId,
        number: u64,
    ) -> Option<gantry_core::entities::PullRequestRef> {
        let mut snapshot = self.store.snapshot().await.ok()?;
        snapshot
            .pr_snapshot(&self.repo, number)
            .await
            .ok()
            .flatten()
    }
}

pub struct EngineRuntime {
    store: Store,
    engine: Engine,
    deps: ActorDeps,
    effects: Arc<PortEffects>,
    settings: Mutex<RuntimeSettings>,
    actors: Mutex<BTreeMap<DeliveryId, DeliveryActor>>,
    /// 停止决策的交付单元及原因（供 `status` 报 `degraded`）。
    degraded: Mutex<BTreeMap<DeliveryId, String>>,
    router: Mutex<EventRouter>,
    mail_tx: mpsc::UnboundedSender<Mail>,
    mail_rx: Mutex<mpsc::UnboundedReceiver<Mail>>,
}

impl EngineRuntime {
    pub async fn new(
        store: Store,
        ports: EnginePorts,
        settings: RuntimeSettings,
    ) -> Result<Arc<Self>, RuntimeError> {
        let (mail_tx, mail_rx) = mpsc::unbounded_channel::<Mail>();
        let tx = mail_tx.clone();
        let deliver: Deliver = Arc::new(move |delivery, input, identity| {
            // 主循环已退出时丢弃：下次启动由 Tick(Recover) 补齐。
            let _ = tx.send(Mail {
                delivery,
                input,
                identity,
            });
        });
        let effects = Arc::new(PortEffects::new(
            AttemptPorts {
                sandbox: ports.sandbox,
                repo: ports.repo,
                policy: ports.policy,
                forge: ports.forge,
                cas: ports.cas.clone(),
                adapter: ports.worker,
                clock: ports.clock,
                entropy: ports.entropy,
            },
            ports.outbox.clone(),
            settings.runner.clone(),
            settings.cleanup_lease,
            deliver,
        ));
        let snapshots = ports.snapshots.unwrap_or_else(|| {
            Arc::new(StoreSnapshots {
                store: store.clone(),
                repo: settings.actor.settings.project.repo.clone(),
            })
        });
        let deps = ActorDeps {
            store: store.clone(),
            inbox: ports.inbox.clone(),
            outbox: ports.outbox,
            effects: effects.clone(),
            snapshots,
            cas: ports.cas.clone(),
        };
        let router = EventRouter::open(store.clone()).await?;
        Ok(Arc::new(Self {
            engine: Engine::new(store.clone(), ports.inbox),
            store,
            deps,
            effects,
            settings: Mutex::new(settings),
            actors: Mutex::new(BTreeMap::new()),
            degraded: Mutex::new(BTreeMap::new()),
            router: Mutex::new(router),
            mail_tx,
            mail_rx: Mutex::new(mail_rx),
        }))
    }

    /// 现有的写入口（`create_delivery` 等）。
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// `settings.updated` 热加载：两次决策之间整体切换（§7.13）。
    pub async fn reconfigure(&self, settings: RuntimeSettings) {
        self.effects.reconfigure(settings.runner.clone());
        for actor in self.actors.lock().await.values_mut() {
            actor.reconfigure(settings.actor.clone());
        }
        *self.settings.lock().await = settings;
    }

    /// 主循环；`shutdown` 完成后处理完已入队的回报即返回。
    pub async fn run(
        self: Arc<Self>,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), RuntimeError> {
        self.recover().await?;
        tokio::pin!(shutdown);
        loop {
            self.drain_mail().await?;
            let progressed = self.pump().await?;
            if progressed {
                continue;
            }
            let interval = self.settings.lock().await.poll_interval;
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep(interval) => {}
                mail = self.next_mail() => {
                    if let Some(mail) = mail {
                        self.handle(mail.delivery, mail.input, mail.identity).await?;
                    }
                }
            }
        }
        self.drain_mail().await?;
        Ok(())
    }

    async fn next_mail(&self) -> Option<Mail> {
        self.mail_rx.lock().await.recv().await
    }

    async fn drain_mail(&self) -> Result<(), RuntimeError> {
        loop {
            let mail = self.mail_rx.lock().await.try_recv().ok();
            let Some(mail) = mail else {
                return Ok(());
            };
            self.handle(mail.delivery, mail.input, mail.identity)
                .await?;
        }
    }

    /// 启动恢复（§7.13）：对仍需处理的交付单元发 `Tick(Recover)`。
    async fn recover(&self) -> Result<(), RuntimeError> {
        let filter = EventFilter {
            project_ids: None,
            include: vec![TypePattern::Exact(EventType::DeliveryCreated)],
            exclude: Vec::new(),
        };
        let mut ids = BTreeSet::new();
        scan_events(&self.store, &filter, |event| {
            if let Ok(id) = event.subject.as_str().parse::<DeliveryId>() {
                ids.insert(id);
            }
            Ok(())
        })
        .await?;
        for id in ids {
            let Some(state) = load_delivery_state(&self.store, &id).await? else {
                continue;
            };
            let closed_clean = state.delivery.stage == gantry_core::entities::Stage::Closed
                && state.worktree.is_none();
            if closed_clean {
                continue;
            }
            self.handle(
                id,
                Input::Tick(TickReason::Recover),
                InputIdentity::Tick(TickReason::Recover),
            )
            .await?;
        }
        Ok(())
    }

    /// 读一页事件并分发；新建交付单元启动执行体。返回是否有进展。
    async fn pump(&self) -> Result<bool, RuntimeError> {
        let active: BTreeSet<DeliveryId> = self.actors.lock().await.keys().copied().collect();
        let (dispatches, created) = {
            let mut router = self.router.lock().await;
            let dispatches = router.poll(&active).await?;
            (dispatches, router.take_created())
        };
        let progressed = !dispatches.is_empty() || !created.is_empty();
        for id in created {
            self.handle(
                id,
                Input::Tick(TickReason::Settle),
                InputIdentity::Tick(TickReason::Settle),
            )
            .await?;
        }
        for dispatch in dispatches {
            self.handle(dispatch.delivery, dispatch.input, dispatch.identity)
                .await?;
            self.router
                .lock()
                .await
                .processed(dispatch.delivery, dispatch.seq);
        }
        self.router.lock().await.advance_cursor().await?;
        Ok(progressed)
    }

    /// 把一个输入交给对应执行体（不存在则创建）。degraded 的执行体不再决策，只记录。
    async fn handle(
        &self,
        delivery: DeliveryId,
        input: Input,
        identity: InputIdentity,
    ) -> Result<(), RuntimeError> {
        let config = self.settings.lock().await.actor.clone();
        let mut actors = self.actors.lock().await;
        let actor = actors
            .entry(delivery)
            .or_insert_with(|| DeliveryActor::new(delivery, self.deps.clone(), config));
        match actor.handle(input, identity).await {
            Ok(_) | Err(ActorError::NotFound) => Ok(()),
            // 存储故障不是某个交付单元的问题：主循环退出，由 gantryd 重启后 Tick(Recover) 接管。
            Err(source @ ActorError::Store(_)) => Err(RuntimeError::Actor { delivery, source }),
            // 引擎缺陷（收敛超限、端口拒绝事务内决策）：该交付单元停止决策（§7.14.1），
            // 其他交付单元照常推进。
            Err(source) => {
                actor.degrade();
                self.degraded
                    .lock()
                    .await
                    .entry(delivery)
                    .or_insert_with(|| source.to_string());
                Ok(())
            }
        }
    }

    /// 已停止决策的交付单元及原因（`status` 报 `degraded`）。
    pub async fn degraded(&self) -> BTreeMap<DeliveryId, String> {
        self.degraded.lock().await.clone()
    }

    /// 测试与 `DeliveryControl`（#73）用：直接投递一个输入。
    pub async fn deliver(
        &self,
        delivery: DeliveryId,
        input: Input,
        identity: InputIdentity,
    ) -> Result<(), RuntimeError> {
        self.handle(delivery, input, identity).await
    }

    /// 邮件发送端（供装配方把外部定时器等接到同一邮箱）。
    pub fn mailbox(&self) -> Deliver {
        let tx = self.mail_tx.clone();
        Arc::new(move |delivery, input, identity| {
            let _ = tx.send(Mail {
                delivery,
                input,
                identity,
            });
        })
    }
}
