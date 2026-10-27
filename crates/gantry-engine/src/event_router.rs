//! 事件路由与 `engine` 游标（engine.md §7.13、§7.14.1、§7.14.2，#71）。
//!
//! `EventRouter` 按 `seq` 顺序读已提交事件，用路由表（[`Router`]）决定投给哪个交付单元执行体。
//! 只有路由表标为"投递"的事件生成 `Input::Event`；`settings.updated` 广播给所有活跃执行体。
//!
//! 游标按低水位推进：仍有"已投递未处理完"事件的执行体中最小的未处理 `seq` − 1；没有积压时取
//! 已读到的最大 `seq`（含不投给任何执行体的事件）。空闲执行体不拖住游标。崩溃后从低水位重放，
//! 重复投递由 `decide` 的前置条件消化（§7.14.1"事件游标"）。

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use gantry_core::entities::Seq;
use gantry_core::events::{
    ApprovalRequestedData, Event, EventType, InboxResolvedData, OutboxCancelledData,
    OutboxFailedData, OutboxSucceededData, PatchRejectedData, PatchValidatedData,
    WorktreeCreatedData, WorktreeLockedData, WorktreeUnlockedData,
};
use gantry_core::forge::ForgeEventEnvelope;
use gantry_core::ids::{ActionId, ApprovalId, DeliveryId};
use gantry_core::ports::inbox::OptionId;
use gantry_store::{EventFilter, Store, StoreError};
use serde_json::Value;

use crate::decide::{Input, InputIdentity, Observed, ObservedData};
use crate::route::{Route, RouteError, Router};

/// `engine` 消费者在 store 游标表里的名字。
pub const CURSOR: &str = "engine";

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("route: {0}")]
    Route(#[from] RouteError),
}

/// 一次投递：交给某个交付单元执行体的输入。
pub struct Dispatch {
    pub delivery: DeliveryId,
    pub seq: Seq,
    pub input: Input,
    pub identity: InputIdentity,
}

pub struct EventRouter {
    store: Store,
    router: Router,
    /// 已读到（已路由）的最大 seq。
    read_to: Seq,
    /// 每个执行体已投递、尚未回报处理完的 seq（升序）。
    backlog: BTreeMap<DeliveryId, VecDeque<Seq>>,
}

impl EventRouter {
    /// 从持久游标恢复：先按 seq 重放 `(0, 游标]` 建立联查索引（不投递），之后从游标后读。
    pub async fn open(store: Store) -> Result<Self, RouterError> {
        let cursor = store.cursors().load(CURSOR).await?;
        let mut router = Router::default();
        let mut after = Seq::ZERO;
        while after < cursor {
            let page = store
                .events()
                .scan(after, &EventFilter::default(), 200)
                .await?;
            for event in page.events.iter().filter(|e| e.seq <= cursor) {
                router.route(event)?;
            }
            if page.scanned_to >= page.head || page.events.is_empty() {
                break;
            }
            after = page.scanned_to;
        }
        Ok(Self {
            store,
            router,
            read_to: cursor,
            backlog: BTreeMap::new(),
        })
    }

    pub fn read_to(&self) -> Seq {
        self.read_to
    }

    /// 读一页新事件并路由；`active` 是当前活跃的执行体（广播的对象）。
    pub async fn poll(
        &mut self,
        active: &BTreeSet<DeliveryId>,
    ) -> Result<Vec<Dispatch>, RouterError> {
        let page = self
            .store
            .events()
            .scan(self.read_to, &EventFilter::default(), 200)
            .await?;
        let mut out = Vec::new();
        for event in &page.events {
            match self.router.route(event)? {
                Route::Delivery { id, input: true } => {
                    if let Some(dispatch) = dispatch(id, event) {
                        self.backlog.entry(id).or_default().push_back(event.seq);
                        out.push(dispatch);
                    }
                }
                Route::Broadcast => {
                    for id in active {
                        if let Some(dispatch) = dispatch(*id, event) {
                            self.backlog.entry(*id).or_default().push_back(event.seq);
                            out.push(dispatch);
                        }
                    }
                }
                Route::Delivery { input: false, .. } | Route::None => {}
            }
            self.read_to = event.seq;
        }
        if page.events.is_empty() {
            self.read_to = self.read_to.max(page.scanned_to);
        }
        Ok(out)
    }

    /// 执行体处理完 `seq`（含）之前投给它的事件后回报。
    pub fn processed(&mut self, delivery: DeliveryId, seq: Seq) {
        if let Some(queue) = self.backlog.get_mut(&delivery) {
            while queue.front().is_some_and(|s| *s <= seq) {
                queue.pop_front();
            }
            if queue.is_empty() {
                self.backlog.remove(&delivery);
            }
        }
    }

    /// 低水位：有积压的执行体中最小的未处理 seq − 1；没有积压时取已读到的最大 seq。
    pub fn low_water(&self) -> Seq {
        self.backlog
            .values()
            .filter_map(|queue| queue.front())
            .min()
            .map(|first| Seq::from_wire(first.as_u64().saturating_sub(1)).unwrap_or(Seq::ZERO))
            .unwrap_or(self.read_to)
    }

    /// 把持久游标推进到低水位（只前进，不后退）。
    pub async fn advance_cursor(&self) -> Result<Seq, RouterError> {
        let target = self.low_water();
        let current = self.store.cursors().load(CURSOR).await?;
        if target > current {
            self.store.cursors().advance(CURSOR, target).await?;
            return Ok(target);
        }
        Ok(current)
    }
}

fn dispatch(delivery: DeliveryId, event: &Event<Value>) -> Option<Dispatch> {
    let data = observed_data(event)?;
    Some(Dispatch {
        delivery,
        seq: event.seq,
        identity: InputIdentity::Event(event.id),
        input: Input::Event(Observed {
            seq: event.seq,
            id: event.id,
            actor: event.actor.clone(),
            data,
        }),
    })
}

fn decode<T: serde::de::DeserializeOwned>(event: &Event<Value>) -> Option<T> {
    serde_json::from_value(event.data.clone()).ok()
}

fn subject<T: std::str::FromStr>(event: &Event<Value>) -> Option<T> {
    event.subject.as_str().parse().ok()
}

/// 把路由表标为"投递"的事件解码成 `ObservedData`（§7.14.2）。不新增事件字段；解码失败不投递。
fn observed_data(event: &Event<Value>) -> Option<ObservedData> {
    use EventType as T;
    Some(match event.ty {
        T::Forge(_) => ObservedData::Forge(decode::<ForgeEventEnvelope>(event)?.data),
        T::OutboxSucceeded => ObservedData::OutboxSucceeded {
            action_id: subject::<ActionId>(event)?,
            github_ref: decode::<OutboxSucceededData>(event)?.github_ref,
        },
        T::OutboxFailed => ObservedData::OutboxFailed {
            action_id: subject::<ActionId>(event)?,
            code: decode::<OutboxFailedData>(event)?.code,
        },
        T::OutboxCancelled => ObservedData::OutboxCancelled {
            action_id: subject::<ActionId>(event)?,
            reason: decode::<OutboxCancelledData>(event)?.reason,
        },
        T::ApprovalRequested => {
            let data: ApprovalRequestedData = decode(event)?;
            ObservedData::ApprovalRequested {
                approval_id: subject::<ApprovalId>(event)?,
                action: data.action,
                subject_digest: data.subject_digest,
            }
        }
        T::ApprovalGranted
        | T::ApprovalConsumed
        | T::ApprovalInvalidated
        | T::ApprovalExpired
        | T::ApprovalDenied => {
            use gantry_core::entities::ApprovalStatus as S;
            let status = match event.ty {
                T::ApprovalGranted => S::Granted,
                T::ApprovalConsumed => S::Consumed,
                T::ApprovalInvalidated => S::Invalidated,
                T::ApprovalExpired => S::Expired,
                _ => S::Denied,
            };
            ObservedData::Approval {
                approval_id: subject::<ApprovalId>(event)?,
                status,
            }
        }
        T::InboxResolved => {
            let data: InboxResolvedData = decode(event)?;
            ObservedData::InboxResolved {
                inbox_id: data.inbox_id,
                resolution: data.resolution,
                option_id: data.option_id.map(OptionId::new),
                // 原上报项快照由执行选项的切片（#73）联查补上；本切片的 decide 不读它。
                item: None,
            }
        }
        T::InboxDismissed => ObservedData::InboxDismissed {
            inbox_id: decode::<gantry_core::events::InboxDismissedData>(event)?.inbox_id,
        },
        T::PatchValidated => ObservedData::PatchValidated(decode::<PatchValidatedData>(event)?),
        T::PatchRejected => ObservedData::PatchRejected(decode::<PatchRejectedData>(event)?),
        T::WorktreeCreated => {
            let data: WorktreeCreatedData = decode(event)?;
            ObservedData::WorktreeCreated {
                worktree_id: data.worktree_id,
                branch: data.branch,
                base_sha: data.base_sha,
            }
        }
        T::WorktreeLocked => {
            let data: WorktreeLockedData = decode(event)?;
            ObservedData::WorktreeLocked {
                worktree_id: data.worktree_id,
                holder: data.holder,
                lease_until: data.lease_until,
                fence: event.seq,
            }
        }
        T::WorktreeUnlocked => ObservedData::WorktreeUnlocked {
            worktree_id: decode::<WorktreeUnlockedData>(event)?.worktree_id,
        },
        T::SettingsUpdated => ObservedData::SettingsUpdated { rev: event.seq },
        // worktree.removed 在 ObservedData 里没有变体（§7.14.2）：只折叠，不投递。
        _ => return None,
    })
}
