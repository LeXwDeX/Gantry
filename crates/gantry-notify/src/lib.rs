#![forbid(unsafe_code)]

//! 收件箱与通知（`docs/modules/notify.md`）。
//!
//! M1 落地收件箱核心：[`Notify`] 实现 `core::ports::InboxPort` 的上报、
//! `dedup_key` 去重、`choose` / `dismiss` / `resolve`、查询与按项目计数
//! （`GET /v1/inbox/summary` 的数据来源，P-63）。收件箱状态只来自 `store`
//! 的内置 `inbox_items` 投影；本模块只追加 `inbox.*` 事件、不持有自己的状态
//! （§7.10）。通知管线（M2）与 APNs（M4）不在本 crate 的 M1 范围内。

mod inbox;
mod validate;

pub use inbox::Notify;
