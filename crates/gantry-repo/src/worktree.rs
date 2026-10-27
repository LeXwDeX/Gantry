//! 工作树与写权锁的内存投影（R2、R3）。
//!
//! 事实源是事件日志：本表只由 `worktree.*` 事件折叠得到，重启时从日志重建
//! （repo.md §7.6、§7.8）。折叠规则：
//!
//! - `worktree.locked` 时表里没有锁 → 新的持有期，`fence` = 该事件的 `seq`；
//!   已有锁 → 续租，只更新 `lease_until`，`fence` 不变。
//! - 过期的锁一定先由 `worktree.unlocked(reason=expired)` 结束，再开始新的持有期，
//!   所以上面的规则对重放是确定的。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gantry_core::canon::GitOid;
use gantry_core::entities::{LockHolder, Seq, Worktree, WorktreeLock};
use gantry_core::events::{
    Event, EventType, WorktreeCreatedData, WorktreeLockedData, WorktreeRemovedData,
    WorktreeUnlockedData,
};
use gantry_core::ids::{DeliveryId, ProjectId, WorktreeId};
use gantry_core::ports::{LockInfo, LockToken, RepoError};
use gantry_core::time::Timestamp;
use serde_json::Value;

/// 人工锁的最长租约（api.md §3.7）。
pub const HUMAN_LEASE_MAX: Duration = Duration::from_secs(14_400);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveLock {
    pub holder: LockHolder,
    pub delivery_id: DeliveryId,
    pub fence: Seq,
    pub acquired_at: Timestamp,
    pub lease_until: Timestamp,
}

impl ActiveLock {
    pub fn info(&self) -> LockInfo {
        LockInfo {
            holder: self.holder,
            delivery_id: self.delivery_id,
            fence: self.fence,
            lease_until: self.lease_until,
        }
    }

    pub fn token(&self, worktree_id: WorktreeId) -> LockToken {
        LockToken {
            worktree_id,
            holder: self.holder,
            delivery_id: self.delivery_id,
            fence: self.fence,
            lease_until: self.lease_until,
        }
    }

    /// 与 `core::machines::worktree_lock::is_expired` 一致：`now >= lease_until`。
    pub fn expired(&self, now: Timestamp) -> bool {
        now >= self.lease_until
    }

    pub fn matches(&self, token: &LockToken) -> bool {
        self.fence == token.fence
            && self.holder == token.holder
            && self.delivery_id == token.delivery_id
    }
}

#[derive(Clone, Debug)]
pub struct WorktreeState {
    pub worktree: Worktree,
    pub delivery_id: DeliveryId,
    pub base_sha: GitOid,
    pub lock: Option<ActiveLock>,
}

#[derive(Debug, Default)]
pub struct WorktreeTable {
    pub by_id: BTreeMap<WorktreeId, WorktreeState>,
    pub by_delivery: BTreeMap<DeliveryId, WorktreeId>,
    pub worktrees_dir: PathBuf,
}

impl WorktreeTable {
    pub fn new(worktrees_dir: PathBuf) -> Self {
        Self {
            worktrees_dir,
            ..Self::default()
        }
    }

    pub fn get(&self, id: &WorktreeId) -> Result<&WorktreeState, RepoError> {
        self.by_id.get(id).ok_or(RepoError::WorktreeNotFound)
    }

    /// 展示用路径（OQ-R1：可选的惰性物化检出，本模块不常驻检出）。
    pub fn display_path(dir: &Path, project: &ProjectId, delivery: &DeliveryId) -> String {
        dir.join(project.to_string())
            .join(delivery.to_string())
            .display()
            .to_string()
    }

    /// 折叠一条 `worktree.*` 事件；其他类型忽略。
    pub fn apply(&mut self, event: &Event<Value>) -> Result<(), RepoError> {
        match event.ty {
            EventType::WorktreeCreated => {
                let data: WorktreeCreatedData = decode(&event.data)?;
                let path =
                    Self::display_path(&self.worktrees_dir, &data.project_id, &data.delivery_id);
                let worktree = Worktree {
                    id: data.worktree_id,
                    project_id: data.project_id,
                    branch: data.branch,
                    base_ref: data.base_ref,
                    path,
                    lock: None,
                    created_at: event.ts,
                    updated_at: event.ts,
                };
                self.by_delivery.insert(data.delivery_id, data.worktree_id);
                self.by_id.insert(
                    data.worktree_id,
                    WorktreeState {
                        worktree,
                        delivery_id: data.delivery_id,
                        base_sha: data.base_sha,
                        lock: None,
                    },
                );
            }
            EventType::WorktreeLocked => {
                let data: WorktreeLockedData = decode(&event.data)?;
                if let Some(state) = self.by_id.get_mut(&data.worktree_id) {
                    let lock = match state.lock.take() {
                        Some(mut current) => {
                            current.lease_until = data.lease_until;
                            current
                        }
                        None => ActiveLock {
                            holder: data.holder,
                            delivery_id: data.delivery_id,
                            fence: event.seq,
                            acquired_at: event.ts,
                            lease_until: data.lease_until,
                        },
                    };
                    state.worktree.lock = Some(WorktreeLock {
                        holder: lock.holder,
                        delivery_id: lock.delivery_id,
                        acquired_at: lock.acquired_at,
                        lease_until: lock.lease_until,
                    });
                    state.worktree.updated_at = event.ts;
                    state.lock = Some(lock);
                }
            }
            EventType::WorktreeUnlocked => {
                let data: WorktreeUnlockedData = decode(&event.data)?;
                if let Some(state) = self.by_id.get_mut(&data.worktree_id) {
                    state.lock = None;
                    state.worktree.lock = None;
                    state.worktree.updated_at = event.ts;
                }
            }
            EventType::WorktreeRemoved => {
                let data: WorktreeRemovedData = decode(&event.data)?;
                if let Some(state) = self.by_id.remove(&data.worktree_id) {
                    if self.by_delivery.get(&state.delivery_id) == Some(&data.worktree_id) {
                        self.by_delivery.remove(&state.delivery_id);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// 写操作对锁令牌的校验（repo.md §7.5 步 2、§7.6）。
///
/// 顺序：没有锁 → `LockRequired`；人工锁有效且令牌不是它 → `WorktreeLocked`；
/// 围栏不符或已过期 → `StaleLock`；人工令牌不能提交 / 移除 → `WorktreeLocked`。
pub fn check_token(
    state: &WorktreeState,
    token: &LockToken,
    now: Timestamp,
) -> Result<(), RepoError> {
    let lock = state.lock.as_ref().ok_or(RepoError::LockRequired)?;
    if lock.holder == LockHolder::Human && !lock.expired(now) && !lock.matches(token) {
        return Err(RepoError::WorktreeLocked { lock: lock.info() });
    }
    if !lock.matches(token) || lock.expired(now) {
        return Err(RepoError::StaleLock);
    }
    if lock.holder == LockHolder::Human {
        return Err(RepoError::WorktreeLocked { lock: lock.info() });
    }
    Ok(())
}

pub fn lease_until(now: Timestamp, lease: Duration) -> Timestamp {
    let ms = i64::try_from(lease.as_millis()).unwrap_or(i64::MAX);
    Timestamp::from_millis(now.as_millis().saturating_add(ms))
}

fn decode<T: serde::de::DeserializeOwned>(data: &Value) -> Result<T, RepoError> {
    serde_json::from_value(data.clone()).map_err(|error| RepoError::Io {
        message: format!("worktree event data: {error}"),
    })
}
