//! 引擎写入口的最小实现：把 `delivery.created` 写入事件日志（engine.md §7.1）。
//!
//! 只做创建事务；决策器、调度与副作用执行仍未实现。

use gantry_core::events::SchemaError;
use gantry_core::ids::{DeliveryId, IdError};
use gantry_store::{Store, StoreError};

use crate::create::{created_event, NewDelivery};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("contract: {0}")]
    Contract(#[from] SchemaError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("id: {0}")]
    Id(#[from] IdError),
}

pub struct Engine {
    store: Store,
}

impl Engine {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 在单个写事务中追加 `delivery.created`；相同 `source_message_id` 不重复追加。
    pub async fn create_delivery(&self, delivery: NewDelivery) -> Result<DeliveryId, EngineError> {
        let id_text = delivery.id.to_string();
        let event = created_event(delivery)?;
        let mut tx = self.store.begin_write().await?;
        tx.append(event).await?;
        tx.commit().await?;
        Ok(id_text.parse()?)
    }
}
