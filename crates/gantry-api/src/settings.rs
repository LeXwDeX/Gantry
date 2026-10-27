//! settings 编排（api.md §4.9，本文 §7.8）：读 #30、写 #31（M1 只开放 `models`）、
//! `catalog` #35、`status` #36。
//!
//! 合并 / 校验 / 放宽判定 / secret 拆分都用 `core::settings`；本模块负责 ETag 条件
//! 请求、`dry_run`、角色与签名判定、`policy.consume_internal` 与 `settings.updated`
//! 同一写事务，以及凭据的两阶段转交（`EgressControl`：stage → 事件提交 → activate；
//! 失败 discard，config.md §2.4）。M1 的 `models` 凭据使用方都是 Egress；Forge 通道
//! （`github.app.*`）随 `github` resource 开放写入时接入。
//!
//! 响应、事件、日志里永远没有 secret `value`：读回的 SecretRef 只有 `secret_ref` 与
//! `present`（I6）。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use gantry_core::entities::{
    Actor, ActorKind, ApprovalAction, ApprovalTarget, DeviceRole, Seq, SettingsResource,
    SettingsScope,
};
use gantry_core::events::{
    EventType, NewEvent, SecretChange, SecretChangeOp, SettingsUpdatedData, Subject,
};
use gantry_core::ids::ApprovalId;
use gantry_core::ports::egress::{StageSecret, StagedSecret};
use gantry_core::ports::ModuleError;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::settings::{
    diff_relaxes, find_secret_refs, required_authority_for_change, split_secret_values, validate,
    SecretWriteIntent, SecretWriteKind, ValidationContext, ValidationIssue,
    SETTINGS_SCHEMA_VERSION,
};
use gantry_policy::service::{ConsumeInternal, InternalAction, RevCheck};
use gantry_policy::signing::request_digest;
use http::header::{HeaderValue, CACHE_CONTROL, ETAG, IF_MATCH, IF_NONE_MATCH};
use http::StatusCode;
use serde_json::{json, Map, Value};

use crate::approvals::GANTRY_APPROVAL;
use crate::auth::role_satisfies;
use crate::control::actor_ctx;
use crate::problem::{FieldError, Problem, ProblemCode};
use crate::server::{ApiCore, HandlerRequest, Reply};

/// 校验上下文、`catalog` 与 `status` 的来源（本文 §6.3；数据来自 models / engine /
/// secrets / 各模块的 `ModuleStatus`）。装配层实现；测试用替身。
#[async_trait]
pub trait SettingsContext: Send + Sync {
    /// models 能力表、engine 模板登记、密钥存在性、其他 resource 当前值。
    fn validation_ctx(&self) -> Arc<dyn ValidationContext + Send + Sync>;
    /// config.md §2.8。
    async fn catalog(&self) -> Result<Value, ModuleError>;
    /// `modules.<name>.{applied_rev, health, detail?, unresolved_secret_refs}`。
    async fn status_modules(&self) -> Result<Map<String, Value>, ModuleError>;
}

/// settings 的消费方模块（config.md §2.8 `status.modules`）。
const STATUS_MODULES: [&str; 7] = [
    "models", "engine", "policy", "guardian", "sandbox", "egress", "notify",
];

/// M1 开放写入的 resource（本文 §1.1、OQ-A14）。
const WRITABLE_M1: [SettingsResource; 1] = [SettingsResource::Models];

fn unavailable() -> Problem {
    Problem::retry_after(ProblemCode::Unavailable, "store unavailable", 1)
}

fn etag(rev: Seq) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{}\"", rev.as_u64())).expect("etag is ASCII")
}

/// `{resource}` 必须是八个之一；未知 → `404`（不是 `400`）。
fn resource(req: &HandlerRequest<'_>) -> Result<SettingsResource, Problem> {
    serde_json::from_value(Value::String(req.param("resource").to_owned()))
        .map_err(|_| Problem::not_found())
}

fn resource_str(resource: SettingsResource) -> Value {
    serde_json::to_value(resource).unwrap_or(Value::Null)
}

fn issues(list: &[ValidationIssue]) -> Vec<FieldError> {
    list.iter()
        .map(|i| FieldError::new(i.pointer.clone(), i.code, i.message.clone()))
        .collect()
}

fn issues_json(list: &[ValidationIssue]) -> Vec<Value> {
    list.iter()
        .map(|i| json!({ "pointer": i.pointer, "code": i.code, "message": i.message }))
        .collect()
}

/// 读回的文档：SecretRef 只留 `secret_ref` 与 `present`。
async fn with_presence(
    core: &ApiCore,
    resource: SettingsResource,
    mut doc: Value,
) -> Result<Value, Problem> {
    let refs = find_secret_refs(resource, &doc);
    if refs.is_empty() {
        return Ok(doc);
    }
    // 按使用方分别查询：Egress 经控制面，Forge 经 secrets 存储（config.md §2.4）
    let egress_names: Vec<SecretName> = refs
        .iter()
        .filter(|(_, _, c)| *c == Consumer::Egress)
        .map(|(_, n, _)| n.clone())
        .collect();
    let egress_present = if egress_names.is_empty() {
        BTreeMap::new()
    } else {
        core.services()
            .egress
            .secret_present(&egress_names)
            .await
            .map_err(|_| unavailable())?
    };
    for (pointer, name, consumer) in refs {
        let present = match consumer {
            Consumer::Egress => egress_present.get(&name).copied().unwrap_or(false),
            Consumer::Forge => core
                .services()
                .forge_secrets
                .present(&name)
                .map_err(|_| unavailable())?,
            _ => false,
        };
        if let Some(Value::Object(slot)) = doc.pointer_mut(&pointer) {
            slot.remove("value");
            slot.insert("present".into(), Value::Bool(present));
        }
    }
    Ok(doc)
}

/// #30 `GET /v1/settings/{resource}[?rev=N]`。
pub async fn get(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let resource = resource(req)?;
    let snapshot = core
        .services()
        .store
        .snapshot()
        .await
        .map_err(|_| unavailable())?;
    let seq = snapshot.head_seq();
    let entry = match req.query.int("rev") {
        Some(rev) => snapshot
            .settings_at_rev(
                &SettingsScope::Global,
                resource,
                Seq::from_wire(rev).map_err(|_| Problem::not_found())?,
            )
            .ok_or_else(Problem::not_found)?,
        None => snapshot.settings_current(&SettingsScope::Global, resource),
    };
    let tag = etag(entry.rev);
    let mut reply = if req.headers.get(IF_NONE_MATCH) == Some(&tag) {
        let mut r = Reply::json(StatusCode::NOT_MODIFIED, Value::Null, seq);
        r.body = None;
        r
    } else {
        let value = match entry.value {
            Some(doc) => with_presence(core, resource, doc).await?,
            None => Value::Null,
        };
        Reply::json(
            StatusCode::OK,
            json!({
                "resource": resource_str(resource),
                "scope": "global",
                "rev": entry.rev.as_u64(),
                "value": value,
                "warnings": [],
            }),
            seq,
        )
    };
    reply.headers.push((ETAG, tag));
    // settings 用 ETag 做条件请求，不是 no-store（本文 §7.1 步骤 14 的例外）。
    reply
        .headers
        .push((CACHE_CONTROL, HeaderValue::from_static("no-cache")));
    Ok(reply)
}

/// #35 `GET /v1/settings/catalog`。
pub async fn catalog(core: &ApiCore, _req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let seq = core
        .services()
        .store
        .events()
        .head()
        .await
        .map_err(|_| unavailable())?;
    let body = core
        .services()
        .settings_ctx
        .catalog()
        .await
        .map_err(|_| unavailable())?;
    Ok(Reply::json(StatusCode::OK, body, seq))
}

/// #36 `GET /v1/settings/status`：聚合不到的模块报 `unhealthy`，不报错（本文 §7.8）。
pub async fn status(core: &ApiCore, _req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let svc = core.services();
    let snapshot = svc.store.snapshot().await.map_err(|_| unavailable())?;
    let seq = snapshot.head_seq();
    let latest = snapshot.config_rev(None);
    // 聚合不到的模块报 unhealthy，不报错、也不隐藏成"未上报"（本文 §7.8）
    let modules = match svc.settings_ctx.status_modules().await {
        Ok(modules) => modules,
        Err(_) => STATUS_MODULES
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    json!({
                        "applied_rev": 0,
                        "health": "unhealthy",
                        "detail": "status aggregation failed",
                        "unresolved_secret_refs": [],
                    }),
                )
            })
            .collect(),
    };
    Ok(Reply::json(
        StatusCode::OK,
        json!({ "latest_rev": latest.as_u64(), "modules": modules }),
        seq,
    ))
}

fn if_match(req: &HandlerRequest<'_>) -> Result<Seq, Problem> {
    let raw = req
        .headers
        .get(IF_MATCH)
        .ok_or_else(|| Problem::new(ProblemCode::PreconditionRequired, "If-Match is required"))?;
    raw.to_str()
        .ok()
        .and_then(|v| {
            v.trim()
                .strip_prefix('"')?
                .strip_suffix('"')?
                .parse::<u64>()
                .ok()
        })
        .and_then(|n| Seq::from_wire(n).ok())
        .ok_or_else(|| Problem::new(ProblemCode::PreconditionFailed, "If-Match does not match"))
}

/// #31 `PUT /v1/settings/{resource}`（本文 §7.8 的固定顺序）。
pub async fn put(core: &ApiCore, req: &HandlerRequest<'_>) -> Result<Reply, Problem> {
    let svc = core.services();
    // 1. 范围与 resource：M1 只开放 models 写入，其余按未启用路由 404
    let resource = resource(req)?;
    if !WRITABLE_M1.contains(&resource) {
        return Err(Problem::not_found());
    }
    let scope = SettingsScope::Global;
    let current = svc
        .store
        .snapshot()
        .await
        .map_err(|_| unavailable())?
        .settings_current(&scope, resource);
    // 2. If-Match 优先于其后的一切
    let wanted = match if_match(req) {
        Ok(rev) => rev,
        Err(problem) if problem.code == ProblemCode::PreconditionFailed => {
            return Err(problem.with_header(ETAG, etag(current.rev)))
        }
        Err(problem) => return Err(problem),
    };
    if wanted != current.rev {
        return Err(
            Problem::new(ProblemCode::PreconditionFailed, "If-Match does not match")
                .with_header(ETAG, etag(current.rev)),
        );
    }
    // 3. 外壳 { value }：只查外壳；value 的 schema 与语义都交给 core 校验器，
    //    这样 secret 的 null / 缺失能得到 config.md §11 的语义码，指针相对 value。
    let raw = req.body.clone().unwrap_or(Value::Null);
    let Some(envelope) = raw.as_object() else {
        return Err(Problem::validation(vec![FieldError::new(
            "",
            "type",
            "expected an object",
        )]));
    };
    let unknown: Vec<FieldError> = envelope
        .keys()
        .filter(|k| k.as_str() != "value")
        .map(|k| FieldError::new(format!("/{k}"), "unknown_field", "unknown field"))
        .collect();
    if !unknown.is_empty() {
        return Err(Problem::validation(unknown));
    }
    let Some(request_value) = envelope.get("value").cloned() else {
        return Err(Problem::validation(vec![FieldError::new(
            "/value",
            "required",
            "field is required",
        )]));
    };
    let pointer_prefix = |mut e: FieldError| {
        // errors[].pointer 相对 value（api.md §3.1）
        e.pointer = e.pointer.trim_start_matches("/value").to_owned();
        e
    };
    // 4. core 校验（含 value 的原始文档：带值的 secret 不要求已存在）与 secret 拆分
    let ctx = svc.settings_ctx.validation_ctx();
    let report = validate(resource, &scope, &request_value, ctx.as_ref());
    let (sparse, writes, mut errors) = match split_secret_values(resource, request_value.clone()) {
        Ok((sparse, writes)) => (sparse, writes, Vec::new()),
        // 拆分失败（null / 非字符串值 / 非法 secret_ref）与其他校验错误一样进入结果，
        // dry_run 仍按约定返回 {valid:false, errors, …}（api.md §4.9）
        Err(split_report) => (request_value.clone(), Vec::new(), split_report.errors),
    };
    for issue in &report.errors {
        if !errors
            .iter()
            .any(|e| e.pointer == issue.pointer && e.code == issue.code)
        {
            errors.push(issue.clone());
        }
    }
    let old = current.value.clone().unwrap_or_else(|| json!({}));
    let relaxes = diff_relaxes(resource, &scope, &old, &sparse);
    let intents: Vec<SecretWriteIntent> = writes
        .iter()
        .map(|(name, consumer, _)| SecretWriteIntent {
            name: name.clone(),
            consumer: *consumer,
            write: SecretWriteKind::Set,
        })
        .collect();
    let authority = required_authority_for_change(resource, &relaxes, &old, &sparse, &intents);
    let target = ApprovalTarget::Settings {
        scope,
        project_id: None,
        resource,
        base_rev: current.rev,
    };
    let digest = request_digest(&raw).map_err(|_| Problem::internal())?;
    let signing = json!({
        "action": serde_json::to_value(ApprovalAction::SettingsUpdate).unwrap_or(Value::Null),
        "target": serde_json::to_value(&target).unwrap_or(Value::Null),
        "subject_digest": serde_json::to_value(digest).unwrap_or(Value::Null),
    });
    let relaxes_json: Vec<Value> = relaxes
        .iter()
        .map(|k| serde_json::to_value(k).unwrap_or(Value::Null))
        .collect();
    // 5. dry_run：不落库、不发事件、不消费审批，缺签名头也照常返回
    if req.query.bool("dry_run") == Some(true) {
        let mut body = json!({
            "valid": errors.is_empty(),
            "errors": issues_json(&errors),
            "warnings": issues_json(&report.warnings),
            "relaxes": relaxes_json,
            "required_role": crate::problem::role_str(authority.required_role),
            "requires_signature": authority.requires_signature,
        });
        if authority.requires_signature {
            body["signing"] = signing;
        }
        let seq = svc.store.events().head().await.map_err(|_| unavailable())?;
        let mut reply = Reply::json(StatusCode::OK, body, seq);
        reply.headers.push((ETAG, etag(current.rev)));
        return Ok(reply);
    }
    if !errors.is_empty() {
        return Err(Problem::validation(
            issues(&errors).into_iter().map(pointer_prefix).collect(),
        ));
    }
    // 6. 角色
    if !role_satisfies(req.auth.device.role, authority.required_role) {
        return Err(Problem::forbidden_role(authority.required_role));
    }
    // 7. 签名
    let approval_id: Option<ApprovalId> = match req.headers.get(GANTRY_APPROVAL) {
        Some(v) => Some(
            v.to_str()
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| {
                    Problem::new(
                        ProblemCode::InvalidRequest,
                        "Gantry-Approval is not an approval id",
                    )
                })?,
        ),
        None => None,
    };
    if authority.requires_signature && approval_id.is_none() {
        return Err(Problem::new(
            ProblemCode::SignatureRequired,
            "this write requires a signed approval",
        )
        .with_ext("signing", signing));
    }

    // 凭据暂存（对消费者不可见）
    let actor_ctx = actor_ctx(req);
    let present = if writes.is_empty() {
        BTreeMap::new()
    } else {
        let names: Vec<SecretName> = writes.iter().map(|(n, _, _)| n.clone()).collect();
        svc.egress
            .secret_present(&names)
            .await
            .map_err(|_| unavailable())?
    };
    let mut staged: Vec<StagedSecret> = Vec::new();
    for (name, consumer, value) in writes {
        if consumer != Consumer::Egress || staged.iter().any(|s| s.secret_ref == name) {
            continue;
        }
        let mut stage_ctx = actor_ctx.clone();
        stage_ctx.request_id = svc.ids.next_ulid().map_err(|_| Problem::internal())?;
        match svc
            .egress
            .stage_secret(
                StageSecret {
                    secret_ref: name,
                    value,
                },
                &stage_ctx,
            )
            .await
        {
            Ok(s) => staged.push(s),
            Err(_) => {
                discard(core, &staged, &actor_ctx).await;
                return Err(unavailable());
            }
        }
    }
    let secret_changes: Vec<SecretChange> = staged
        .iter()
        .map(|s| SecretChange {
            secret_ref: s.secret_ref.clone(),
            op: if present.get(&s.secret_ref).copied().unwrap_or(false) {
                SecretChangeOp::Rotate
            } else {
                SecretChangeOp::Set
            },
            version: s.version.get(),
        })
        .collect();

    // 8. 同一写事务：消费审批 → settings.updated
    let data = SettingsUpdatedData {
        scope,
        resource,
        schema_version: SETTINGS_SCHEMA_VERSION,
        base_rev: current.rev,
        value: Some(sparse.clone()),
        relaxes,
        secret_changes,
    };
    let committed = async {
        let mut tx = svc.store.begin_write().await.map_err(|_| unavailable())?;
        // 事务外读到的版本可能已被并发写入超过：取得写事务后重新核对，
        // 412 先于审批消费（api.md §6.6 第 4 步 #5）
        let live = tx.read().settings_current(&scope, resource).rev;
        if live != wanted {
            let _ = tx.rollback().await;
            return Err(
                Problem::new(ProblemCode::PreconditionFailed, "If-Match does not match")
                    .with_header(ETAG, etag(live)),
            );
        }
        if let Some(approval_id) = approval_id.filter(|_| authority.requires_signature) {
            let consumed = svc
                .approvals
                .consume_internal(
                    &mut tx,
                    ConsumeInternal {
                        approval_id,
                        expected_action: InternalAction::SettingsUpdate,
                        expected_target: target.clone(),
                        body_digest: digest,
                        rev_check: Some(RevCheck {
                            if_match_rev: wanted,
                            current_rev: current.rev,
                        }),
                        request_id: req.request_id.to_owned(),
                        actor: actor_ctx.actor.clone(),
                    },
                )
                .await;
            if let Err(error) = consumed {
                let _ = tx.rollback().await;
                return Err(crate::approvals::consume_problem(error));
            }
        }
        let event = NewEvent {
            ty: EventType::SettingsUpdated,
            subject: Subject::settings(),
            project_id: None,
            actor: Actor::new(ActorKind::Device, req.auth.device.id.to_string()),
            causation_id: None,
            correlation_id: None,
            data: serde_json::to_value(&data).map_err(|_| Problem::internal())?,
            dedup_key: None,
        };
        if tx.append(event).await.is_err() {
            let _ = tx.rollback().await;
            return Err(unavailable());
        }
        let committed = tx.commit().await.map_err(|_| unavailable())?;
        Seq::from_wire(committed.last_seq).map_err(|_| Problem::internal())
    }
    .await;
    let rev = match committed {
        Ok(rev) => rev,
        Err(problem) => {
            discard(core, &staged, &actor_ctx).await;
            return Err(problem);
        }
    };
    // 9. 提交成功后生效暂存项；事件已提交而生效失败 → 500，由启动对账补做（本文 §9）
    for s in &staged {
        if svc.egress.activate_secret(s, &actor_ctx).await.is_err() {
            return Err(Problem::internal());
        }
    }
    let value = with_presence(core, resource, sparse).await?;
    let mut reply = Reply::json(
        StatusCode::OK,
        json!({
            "resource": resource_str(resource),
            "scope": "global",
            "rev": rev.as_u64(),
            "value": value,
            "warnings": issues_json(&report.warnings),
        }),
        rev,
    );
    reply.headers.push((ETAG, etag(rev)));
    Ok(reply)
}

async fn discard(core: &ApiCore, staged: &[StagedSecret], ctx: &gantry_core::ports::ActorCtx) {
    for s in staged {
        let _ = core.services().egress.discard_secret(s, ctx).await;
    }
}

/// 写 settings 需要的最低角色（路由下限，本文 §6.2 的 `SettingsDynamic`）。
pub const ROUTE_FLOOR: DeviceRole = DeviceRole::Operator;
