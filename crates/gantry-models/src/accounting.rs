//! 用量记账与对账（models.md §7.3 第 7–9 步，interfaces/egress-call-terminal.md，
//! event-data §2.2.6）。
//!
//! 直接调用结束后先关闭调用（`close_call`），再读终结证明（`call_terminal`）；
//! 只有 `settleable_terminal()` 返回证明（已关闭、完整、与明细一致）才写
//! `budget.charged`。`incomplete = true` 不结算、不零判定；取不到计量就退回
//! 提供方自报并给出警告。`budget.charged` 以 `source.call_id` 去重：同一调用
//! 无论正常记账还是对账补记都只有一条。

use std::collections::{BTreeMap, BTreeSet, HashSet};

use gantry_core::egress::{CallTerminal, LedgerRef, SvcTokenId, UsageCaller, UsageRecord};
use gantry_core::entities::{Actor, ActorKind, Seq, Tier, TokenUsage};
use gantry_core::events::{
    BudgetChargeSource, BudgetChargedData, BudgetChargedDimension, EventType, NewEvent, Subject,
    WeightAudit, WeightRev,
};
use gantry_core::ids::{ConversationId, ProjectId};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::egress::{EgressControl, EgressError};
use gantry_core::ports::models::CallId;
use gantry_core::time::Date;
use gantry_store::{Appended, Store};

use crate::error::ModelsError;
use crate::router::{Attribution, UsageWarning};

/// 一次结算的结果。
#[derive(Clone, Debug, Default)]
pub struct Settlement {
    /// Egress 权威计量（四类 token、加权 token）；取不到为 `None`。
    pub metered: Option<(TokenUsage, u64)>,
    pub charged_seq: Option<Seq>,
    pub warnings: Vec<UsageWarning>,
}

impl Settlement {
    pub fn nothing() -> Self {
        Self::default()
    }

    fn unavailable(reason: String) -> Self {
        Self {
            warnings: vec![UsageWarning::UsageUnavailable { reason }],
            ..Self::default()
        }
    }
}

/// 待记账的一次直接调用。
pub struct Charge {
    pub project_id: ProjectId,
    pub call_id: CallId,
    /// `None`：未归属调用，不写 `budget.charged`。
    pub ledger: Option<LedgerRef>,
    pub tier: Tier,
}

pub fn ledger_ref(attribution: &Attribution, day: Date) -> Option<LedgerRef> {
    match attribution {
        Attribution::Attempt { delivery_id, .. } | Attribution::Aux { delivery_id, .. } => {
            Some(LedgerRef::Delivery { id: *delivery_id })
        }
        Attribution::Conversation {
            conversation_id, ..
        } => Some(LedgerRef::Conversation {
            id: *conversation_id,
            day,
        }),
        Attribution::Unattributed { .. } => None,
    }
}

pub fn dedup_key(call_id: &CallId) -> String {
    format!("budget.charged:direct_call:{call_id}")
}

fn subject_of(ledger: &LedgerRef) -> String {
    match ledger {
        LedgerRef::Delivery { id } => id.to_string(),
        LedgerRef::Conversation { id, .. } => id.to_string(),
    }
}

fn store_error(error: gantry_store::StoreError) -> ModelsError {
    ModelsError::Internal {
        reason: format!("store: {error}"),
    }
}

fn subject(text: &str) -> Result<Subject, ModelsError> {
    Subject::parse(text).map_err(|error| ModelsError::Internal {
        reason: format!("subject: {error:?}"),
    })
}

/// 该台账上已有的 token 记账（按 tier 过滤；`None` 表示全部 tier）。
async fn charged_on(
    store: &Store,
    ledger: &LedgerRef,
    tier: Option<Tier>,
) -> Result<u64, ModelsError> {
    let events = store
        .events()
        .by_subject(&subject(&subject_of(ledger))?)
        .await
        .map_err(store_error)?;
    let mut total = 0u64;
    for event in events {
        if event.ty != EventType::BudgetCharged {
            continue;
        }
        let Ok(data) = serde_json::from_value::<BudgetChargedData>(event.data) else {
            continue;
        };
        if data.dimension == BudgetChargedDimension::TokensByTier
            && &data.ledger == ledger
            && tier.is_none_or(|tier| data.tier == Some(tier))
        {
            total = total.saturating_add(data.delta);
        }
    }
    Ok(total)
}

/// 会话台账当日已用的加权 token（全部 tier，P-58）。
pub async fn conversation_used(
    store: &Store,
    conversation_id: &ConversationId,
    day: Date,
) -> Result<u64, ModelsError> {
    charged_on(
        store,
        &LedgerRef::Conversation {
            id: *conversation_id,
            day,
        },
        None,
    )
    .await
}

fn svc_token_of(record: &UsageRecord) -> Option<&SvcTokenId> {
    match &record.caller {
        UsageCaller::Svc { svc_token_id, .. } => Some(svc_token_id),
        UsageCaller::Job { .. } => None,
    }
}

/// 关闭一次调用：优先用实际接收过该调用请求的令牌（`close_call` 对别的令牌
/// 返回 `Conflict`）。
async fn close(
    egress: &dyn EgressControl,
    fallback_id: &SvcTokenId,
    call_id: &CallId,
    ctx: &ActorCtx,
) -> Result<(), EgressError> {
    let before = egress.call_terminal(call_id).await?;
    if before.is_closed() {
        return Ok(());
    }
    let id = before
        .items
        .iter()
        .find_map(svc_token_of)
        .cloned()
        .unwrap_or_else(|| fallback_id.clone());
    egress.close_call(&id, call_id, ctx).await.map(|_| ())
}

/// 结束一次直接调用的记账。
pub async fn settle_call(
    store: &Store,
    egress: &dyn EgressControl,
    svc_id: &SvcTokenId,
    charge: &Charge,
    ctx: &ActorCtx,
) -> Result<Settlement, ModelsError> {
    if let Err(error) = close(egress, svc_id, &charge.call_id, ctx).await {
        return Ok(Settlement::unavailable(format!(
            "close_call: {}",
            error_code(&error)
        )));
    }
    let statement = match egress.call_terminal(&charge.call_id).await {
        Ok(statement) => statement,
        Err(error) => {
            return Ok(Settlement::unavailable(format!(
                "call_terminal: {}",
                error_code(&error)
            )))
        }
    };
    let Some(terminal) = statement.settleable_terminal().cloned() else {
        let reason = if statement.terminal.as_ref().is_some_and(|t| t.incomplete) {
            "terminal is incomplete".to_owned()
        } else {
            "terminal does not verify against the records".to_owned()
        };
        return Ok(Settlement {
            warnings: vec![UsageWarning::NotSettled { reason }],
            ..Settlement::default()
        });
    };
    let mut settlement = Settlement {
        metered: Some((terminal.usage, terminal.weighted_tokens)),
        ..Settlement::default()
    };
    // 零记录：没有被计量的请求，B-01 不允许零记录的 direct_call 记账事件。
    if terminal.record_count == 0 {
        return Ok(settlement);
    }
    let Some(ledger) = &charge.ledger else {
        return Ok(settlement);
    };
    let (seq, _) = append_charge(
        store,
        charge.project_id,
        ledger,
        charge.tier,
        &charge.call_id,
        &statement.items,
        &terminal,
    )
    .await?;
    settlement.charged_seq = Some(seq);
    Ok(settlement)
}

fn error_code(error: &EgressError) -> &'static str {
    match error {
        EgressError::Unavailable { .. } => "unavailable",
        EgressError::Rejected { .. } => "rejected",
        EgressError::InvalidConfig { .. } => "invalid_config",
        EgressError::NotFound => "not_found",
        EgressError::Conflict { .. } => "conflict",
        EgressError::TokenInvalid => "token_invalid",
        EgressError::Store(_) => "store",
    }
}

/// 追加一条 `budget.charged`；按 `call_id` 去重。返回（序号，是否新写入）。
async fn append_charge(
    store: &Store,
    project_id: ProjectId,
    ledger: &LedgerRef,
    tier: Tier,
    call_id: &CallId,
    items: &[UsageRecord],
    terminal: &CallTerminal,
) -> Result<(Seq, bool), ModelsError> {
    let mut by_rev: BTreeMap<u64, u64> = BTreeMap::new();
    for item in items {
        let entry = by_rev.entry(item.weight_rev).or_default();
        *entry = entry.saturating_add(item.weighted_tokens);
    }
    let first_seq = items.iter().map(|item| item.seq).min();
    let used_before = charged_on(store, ledger, Some(tier)).await?;
    let data = BudgetChargedData {
        ledger: ledger.clone(),
        dimension: BudgetChargedDimension::TokensByTier,
        delta: terminal.weighted_tokens,
        used_after: used_before.saturating_add(terminal.weighted_tokens),
        source: BudgetChargeSource::DirectCall { call_id: *call_id },
        tier: Some(tier),
        usage: Some(terminal.usage),
        weighted_tokens: Some(terminal.weighted_tokens),
        weight_audit: Some(WeightAudit::SvcRecords {
            record_count: terminal.record_count,
            first_seq,
            last_seq: first_seq.map(|_| terminal.last_seq),
            weight_revs: by_rev
                .into_iter()
                .map(|(config_rev, weighted_tokens)| WeightRev {
                    config_rev,
                    weighted_tokens,
                })
                .collect(),
            records_digest: terminal.records_digest,
        }),
    };
    let subject_text = subject_of(ledger);
    let event = NewEvent {
        ty: EventType::BudgetCharged,
        subject: subject(&subject_text)?,
        project_id: Some(project_id),
        actor: Actor::new(ActorKind::System, "models"),
        causation_id: None,
        correlation_id: Some(subject_text),
        data: serde_json::to_value(&data).map_err(|error| ModelsError::Internal {
            reason: format!("budget.charged data: {error}"),
        })?,
        dedup_key: Some(dedup_key(call_id)),
    };
    let mut tx = store.begin_write().await.map_err(store_error)?;
    let appended = tx.append(event).await.map_err(store_error)?;
    tx.commit().await.map_err(store_error)?;
    Ok(match appended {
        Appended::New(event) => (event.seq, true),
        Appended::Duplicate { existing_seq } => (existing_seq, false),
    })
}

/// 台账实体所属的项目：取该 subject 的首个事件的 `project_id`。
async fn project_of(store: &Store, ledger: &LedgerRef) -> Result<Option<ProjectId>, ModelsError> {
    let events = store
        .events()
        .by_subject(&subject(&subject_of(ledger))?)
        .await
        .map_err(store_error)?;
    Ok(events.into_iter().find_map(|event| event.project_id))
}

/// 对一个（被吊销的）令牌对账，返回补记条数。
pub async fn reconcile_token(
    store: &Store,
    egress: &dyn EgressControl,
    svc_id: &SvcTokenId,
    skip: &HashSet<CallId>,
    ctx: &ActorCtx,
) -> Result<usize, ModelsError> {
    let mut records: Vec<UsageRecord> = Vec::new();
    let mut cursor = None;
    loop {
        let page = egress
            .svc_token_usage(svc_id, cursor.clone())
            .await
            .map_err(|error| ModelsError::Unavailable {
                reason: format!("svc_token_usage: {}", error_code(&error)),
            })?;
        if page.items.is_empty() {
            break;
        }
        records.extend(page.items);
        let next = Some(gantry_core::ports::egress::UsageCursor::new(
            page.next_cursor,
        ));
        if next == cursor {
            break;
        }
        cursor = next;
    }

    let calls: BTreeSet<CallId> = records
        .iter()
        .filter_map(|record| match &record.caller {
            UsageCaller::Svc { call_id, .. } => Some(*call_id),
            UsageCaller::Job { .. } => None,
        })
        .collect();
    let mut added = 0;
    for call_id in calls {
        if skip.contains(&call_id) {
            continue;
        }
        if store
            .events()
            .by_dedup_key(&dedup_key(&call_id))
            .await
            .map_err(store_error)?
            .is_some()
        {
            continue;
        }
        let Some(first) = records.iter().find(|record| {
            matches!(&record.caller, UsageCaller::Svc { call_id: id, .. } if *id == call_id)
        }) else {
            continue;
        };
        // 未归属调用不建台账（event-data §2.2.6.1）。
        let Some(ledger) = first.ledger.clone() else {
            continue;
        };
        let tier = first.tier;
        let Some(project_id) = project_of(store, &ledger).await? else {
            continue;
        };
        if close(egress, svc_id, &call_id, ctx).await.is_err() {
            continue;
        }
        let Ok(statement) = egress.call_terminal(&call_id).await else {
            continue;
        };
        let Some(terminal) = statement.settleable_terminal().cloned() else {
            continue;
        };
        if terminal.record_count == 0 {
            continue;
        }
        let (_, new) = append_charge(
            store,
            project_id,
            &ledger,
            tier,
            &call_id,
            &statement.items,
            &terminal,
        )
        .await?;
        if new {
            added += 1;
        }
    }
    Ok(added)
}
