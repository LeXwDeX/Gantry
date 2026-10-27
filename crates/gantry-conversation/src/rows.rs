//! 事件行（§7.7）：把引擎 / forge / policy 事件投影成折叠显示的 `role = event`
//! 消息，带 `tone`（C26）。
//!
//! 文本只用确定性模板与 [`Ref`]：不嵌入标题、评论、拒绝理由等不可信文本，
//! 只插入经契约校验的枚举码与数字。未映射的事件返回 `None`（不显示）。

use gantry_core::canon::RepoSlug;
use gantry_core::entities::{
    ForgeActionKind, ForgeEventKind, MessageTone, OutboxAction, Ref, Stage,
};
use gantry_core::events::{
    DeliveryLevelChangedData, DeliveryStageChangedData, Event, EventType, LevelChangeKind,
    WorkflowRevisedData,
};
use gantry_core::ids::DeliveryId;
use serde_json::Value;

/// 本模块渲染的源事件类型（§7.7 表的 M1 部分；守护与上报项的行属于 M2）。
pub const ROW_EVENT_TYPES: &[EventType] = &[
    EventType::DeliveryStageChanged,
    EventType::DeliveryLevelChanged,
    EventType::DeliveryPaused,
    EventType::DeliveryResumed,
    EventType::DeliveryAbandoned,
    EventType::NodeRetryRequested,
    EventType::WorkflowRevised,
    EventType::OutboxSucceeded,
    EventType::OutboxFailed,
    EventType::Forge(ForgeEventKind::PrMerged),
    EventType::Forge(ForgeEventKind::PrClosed),
    EventType::Forge(ForgeEventKind::IssueClosed),
    EventType::ForgeDriftDetected,
    EventType::ApprovalInvalidated,
    EventType::ApprovalExpired,
    EventType::ApprovalDenied,
    EventType::PolicyDenied,
    EventType::BudgetExhausted,
];

/// 一条事件行的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSpec {
    pub text: String,
    pub tone: MessageTone,
    pub refs: Vec<Ref>,
    pub delivery_id: Option<DeliveryId>,
}

/// 渲染所需的服务端快照（均来自 store，可信结构）。
#[derive(Clone, Debug, Default)]
pub struct Snapshots {
    /// 项目绑定的仓库；用于把 PR / Issue 编号变成 `Ref`。
    pub repo: Option<RepoSlug>,
    /// `outbox.*` 事件对应的动作行。
    pub outbox: Option<OutboxAction>,
    /// `approval.*` 事件对应的 `approval.requested.data`。
    pub approval: Option<Value>,
}

pub fn render(event: &Event<Value>, snap: &Snapshots) -> Option<RowSpec> {
    let delivery = event.subject.as_str().parse::<DeliveryId>().ok();
    let delivery_refs = || {
        delivery
            .map(|id| vec![Ref::Delivery { id }])
            .unwrap_or_default()
    };
    let row = |text: String, tone: MessageTone, refs: Vec<Ref>, delivery_id| RowSpec {
        text,
        tone,
        refs,
        delivery_id,
    };
    match event.ty {
        EventType::DeliveryStageChanged => {
            let data: DeliveryStageChangedData = decode(event)?;
            let (name, tone) = match data.to {
                Stage::Plan => ("规划", MessageTone::Run),
                Stage::Implement => ("实现", MessageTone::Run),
                Stage::Propose => ("提交", MessageTone::Run),
                Stage::Review => ("评审", MessageTone::Run),
                Stage::Merge => ("合并", MessageTone::Run),
                Stage::Closed => ("关闭", MessageTone::Ok),
                Stage::Spec | Stage::Abandoned => return None,
            };
            Some(row(
                format!("交付单元进入{name}阶段"),
                tone,
                delivery_refs(),
                delivery,
            ))
        }
        EventType::DeliveryLevelChanged => {
            let data: DeliveryLevelChangedData = decode(event)?;
            // 只显示自动加码 / 初始风险评估（kind = effective 且提高）；用户调档不重复提示。
            if data.kind != LevelChangeKind::Effective || data.to <= data.from {
                return None;
            }
            let text = match data
                .reason
                .strip_prefix("escalation:")
                .filter(|id| is_code(id))
            {
                Some(rule) => format!("档位提高到 {}（规则 {rule}）", data.to),
                None => format!("档位提高到 {}", data.to),
            };
            Some(row(text, MessageTone::Info, delivery_refs(), delivery))
        }
        EventType::DeliveryPaused => Some(row(
            "交付单元已暂停".into(),
            MessageTone::Warn,
            delivery_refs(),
            delivery,
        )),
        EventType::DeliveryResumed => Some(row(
            "交付单元已继续".into(),
            MessageTone::Run,
            delivery_refs(),
            delivery,
        )),
        EventType::DeliveryAbandoned => Some(row(
            "交付单元已放弃".into(),
            MessageTone::Warn,
            delivery_refs(),
            delivery,
        )),
        EventType::NodeRetryRequested => Some(row(
            "已请求重试节点".into(),
            MessageTone::Run,
            Vec::new(),
            None,
        )),
        EventType::WorkflowRevised => {
            let data: WorkflowRevisedData = decode(event)?;
            if data.reason != "revise" {
                return None;
            }
            Some(row(
                format!("开始第 {} 轮修正", data.round),
                MessageTone::Run,
                Vec::new(),
                None,
            ))
        }
        EventType::OutboxSucceeded => {
            let action = snap.outbox.as_ref()?;
            let pr = event
                .data
                .pointer("/github_ref/pr/number")
                .and_then(Value::as_u64)
                .and_then(|number| pr_ref(snap, number));
            let issue = payload_number(action).and_then(|number| issue_ref(snap, number));
            let (text, refs) = match action.kind {
                ForgeActionKind::PushBranch => ("已推送分支", Vec::new()),
                ForgeActionKind::CreatePr => ("已创建 PR", pr.into_iter().collect()),
                ForgeActionKind::UpdateIssue => ("规格已写回 Issue", issue.into_iter().collect()),
                ForgeActionKind::CloseIssue => ("已关闭 Issue", issue.into_iter().collect()),
                ForgeActionKind::MergePr => ("已发起合并", pr.into_iter().collect()),
                _ => return None,
            };
            let mut refs: Vec<Ref> = refs;
            refs.insert(
                0,
                Ref::Delivery {
                    id: action.delivery_id,
                },
            );
            Some(row(
                text.into(),
                MessageTone::Ok,
                refs,
                Some(action.delivery_id),
            ))
        }
        EventType::OutboxFailed => {
            let code = code_at(&event.data, "/code")?;
            let delivery_id = snap.outbox.as_ref().map(|action| action.delivery_id);
            Some(row(
                format!("写入 GitHub 失败：{code}"),
                MessageTone::Warn,
                delivery_id
                    .map(|id| vec![Ref::Delivery { id }])
                    .unwrap_or_default(),
                delivery_id,
            ))
        }
        EventType::Forge(kind) => {
            let number = event
                .data
                .pointer("/target/number")
                .and_then(Value::as_u64)?;
            let delivery_id = event
                .data
                .get("delivery_id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<DeliveryId>().ok());
            let (text, tone, reference) = match kind {
                ForgeEventKind::PrMerged => ("PR 已合并", MessageTone::Ok, pr_ref(snap, number)),
                ForgeEventKind::PrClosed => (
                    "PR 已关闭（未合并）",
                    MessageTone::Warn,
                    pr_ref(snap, number),
                ),
                ForgeEventKind::IssueClosed => {
                    ("Issue 已关闭", MessageTone::Info, issue_ref(snap, number))
                }
                _ => return None,
            };
            Some(row(
                text.into(),
                tone,
                reference.into_iter().collect(),
                delivery_id,
            ))
        }
        EventType::ForgeDriftDetected => {
            let kind = code_at(&event.data, "/kind")?;
            let delivery_id = event
                .data
                .get("delivery_id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<DeliveryId>().ok());
            let pr = event
                .data
                .pointer("/target/number")
                .and_then(Value::as_u64)
                .filter(|_| event.data.pointer("/target/type") == Some(&Value::from("pr")))
                .and_then(|number| pr_ref(snap, number));
            Some(row(
                format!("GitHub 上的状态与预期不符（{kind}），已以 GitHub 为准"),
                MessageTone::Warn,
                pr.into_iter().collect(),
                delivery_id,
            ))
        }
        EventType::ApprovalInvalidated | EventType::ApprovalExpired | EventType::ApprovalDenied => {
            let text = match event.ty {
                EventType::ApprovalInvalidated => "PR 的审批已失效（有新推送）",
                EventType::ApprovalExpired => "PR 的审批已过期",
                _ => "PR 的审批已被拒绝",
            };
            let pr = snap
                .approval
                .as_ref()
                .filter(|data| data.pointer("/target/kind") == Some(&Value::from("pr")))
                .and_then(|data| data.pointer("/target/number"))
                .and_then(Value::as_u64)
                .and_then(|number| pr_ref(snap, number));
            Some(row(
                text.into(),
                MessageTone::Warn,
                pr.into_iter().collect(),
                None,
            ))
        }
        EventType::PolicyDenied => {
            let text = match code_at(&event.data, "/reason_code") {
                Some(code) => format!("策略拒绝：{code}"),
                None => "策略拒绝".to_owned(),
            };
            Some(row(text, MessageTone::Warn, delivery_refs(), delivery))
        }
        EventType::BudgetExhausted => {
            let dimension = match event.data.get("dimension") {
                Some(Value::String(code)) if is_code(code) => code.clone(),
                Some(Value::Object(map)) => map
                    .iter()
                    .next()
                    .and_then(|(key, value)| {
                        let tier = value.as_str().filter(|tier| is_code(tier))?;
                        is_code(key).then(|| format!("{key}.{tier}"))
                    })
                    .unwrap_or_else(|| "tokens".into()),
                _ => return None,
            };
            Some(row(
                format!("预算已耗尽：{dimension}"),
                MessageTone::Warn,
                delivery_refs(),
                delivery,
            ))
        }
        _ => None,
    }
}

fn decode<T: serde::de::DeserializeOwned>(event: &Event<Value>) -> Option<T> {
    serde_json::from_value(event.data.clone()).ok()
}

/// 只接受契约词表形状的码值（小写字母、数字、`_` `.` `-`），防止任意文本进入行。
fn is_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '.' | '-')
        })
}

fn code_at(data: &Value, pointer: &str) -> Option<String> {
    data.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|code| is_code(code))
        .map(str::to_owned)
}

fn pr_ref(snap: &Snapshots, number: u64) -> Option<Ref> {
    snap.repo.clone().map(|repo| Ref::Pr { repo, number })
}

fn issue_ref(snap: &Snapshots, number: u64) -> Option<Ref> {
    snap.repo.clone().map(|repo| Ref::Issue { repo, number })
}

/// `update_issue` / `close_issue` 的 Issue 编号。
fn payload_number(action: &OutboxAction) -> Option<u64> {
    let payload = serde_json::to_value(&action.payload).ok()?;
    payload
        .get("number")
        .or_else(|| payload.get("issue_number"))
        .and_then(Value::as_u64)
}
