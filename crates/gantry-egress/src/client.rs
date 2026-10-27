//! 控制面客户端：`gantryd` 侧经 `egress.sock` 实现 `core::ports::egress::EgressControl`
//! （`docs/modules/egress.md` §6.2；`docs/interfaces/egress-control.md` §2、§4、§6）。
//!
//! 每次调用一个短连接（HTTP/1.1 + JSON），带 `x-gantry-request-id`；超时按"结果未知"处理，
//! 依靠各操作的幂等语义由调用方重试。`svc_token` 每进程一枚：首次 `service_token` 时签发并
//! 缓存，到期前 1 h 自动轮换；`rotate_service_token` 强制轮换。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use gantry_core::egress::{
    ActivateSecretResponse, CallTerminal, CloseCallResponse, ConfigApplyResponse, EgressHealth,
    EndpointsResponse, FinalUsage, IssueJobTokenRequest, IssueJobTokenResponse,
    IssueSvcTokenRequest, IssueSvcTokenResponse, IssuedTo, LedgerRef, PresentResponse,
    ProblemDetails, RevokeSvcTokenRequest, StageSecretResponse, StagedSecretList, StatusResponse,
    SvcTokenId, UsageCaller, UsagePage,
};
use gantry_core::entities::{Seq, TokenUsage};
use gantry_core::ids::{AttemptId, Clock};
use gantry_core::ports::context::{ActorCtx, Health};
use gantry_core::ports::egress::{
    AppliedConfig, CallClosed, CallStatement, CallUsage, EgressConfigView, EgressControl,
    EgressEndpoints, EgressError, EgressStatus, IssueJobToken, JobTokenGrant, JobUsage,
    StageSecret, StagedInfo, StagedSecret, SvcTokenGrant, UsageCursor,
};
use gantry_core::ports::models::{CallId, UsageSource};
use gantry_core::ports::sandbox::GroupName;
use gantry_core::ports::secrets::SecretVersion;
use gantry_core::secret::{Consumer, SecretName};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::UnixStream;

/// svc_token 到期前多久主动轮换（egress-control §3.2）。
const ROTATE_BEFORE_S: i64 = 3600;

pub struct EgressClient {
    socket: PathBuf,
    clock: Arc<dyn Clock>,
    issued_to: IssuedTo,
    token_timeout: Duration,
    usage_timeout: Duration,
    svc: Mutex<Option<SvcTokenGrant>>,
}

impl EgressClient {
    pub fn new(socket: impl Into<PathBuf>, clock: Arc<dyn Clock>, issued_to: IssuedTo) -> Self {
        Self {
            socket: socket.into(),
            clock,
            issued_to,
            // egress-control §2.1 [提议]：令牌与 secret 操作 5 s，用量拉取 10 s。
            token_timeout: Duration::from_secs(5),
            usage_timeout: Duration::from_secs(10),
            svc: Mutex::new(None),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        ctx: Option<&ActorCtx>,
        timeout: Duration,
    ) -> Result<T, EgressError> {
        let send = async {
            let stream = UnixStream::connect(&self.socket)
                .await
                .map_err(unavailable)?;
            let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .map_err(unavailable)?;
            tokio::spawn(conn);
            let mut builder = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "egress")
                .header("content-type", "application/json");
            if let Some(ctx) = ctx {
                builder = builder.header("x-gantry-request-id", ctx.request_id.to_string());
            }
            let bytes = body.map(|b| b.to_string()).unwrap_or_default();
            let request = builder
                .body(Full::new(Bytes::from(bytes)))
                .map_err(unavailable)?;
            let response = sender.send_request(request).await.map_err(unavailable)?;
            let status = response.status().as_u16();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(unavailable)?
                .to_bytes();
            Ok::<_, EgressError>((status, bytes))
        };
        let (status, bytes) =
            tokio::time::timeout(timeout, send)
                .await
                .map_err(|_| EgressError::Unavailable {
                    message: "egress control call timed out; result unknown".into(),
                })??;
        if (200..300).contains(&status) {
            return serde_json::from_slice(&bytes).map_err(|e| EgressError::Unavailable {
                message: format!("malformed egress response: {e}"),
            });
        }
        Err(problem_to_error(status, &bytes))
    }

    async fn issue_svc(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        let body = serde_json::to_value(IssueSvcTokenRequest {
            issued_to: self.issued_to.clone(),
            ttl_s: None,
        })
        .map_err(internal)?;
        let grant: IssueSvcTokenResponse = self
            .call(
                Method::POST,
                "/control/v1/svc-tokens",
                Some(body),
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        *self.svc.lock().unwrap_or_else(|p| p.into_inner()) = Some(grant.clone());
        Ok(grant)
    }

    async fn page(&self, path: String) -> Result<UsagePage, EgressError> {
        self.call(Method::GET, &path, None, None, self.usage_timeout)
            .await
    }
}

fn unavailable(error: impl std::fmt::Display) -> EgressError {
    EgressError::Unavailable {
        message: error.to_string(),
    }
}

fn internal(error: impl std::fmt::Display) -> EgressError {
    EgressError::Rejected {
        code: "internal".into(),
        message: error.to_string(),
    }
}

/// problem+json → `EgressError`（egress-call-terminal §4 的映射，及 §6 错误表）。
fn problem_to_error(status: u16, bytes: &[u8]) -> EgressError {
    let Ok(problem) = serde_json::from_slice::<ProblemDetails>(bytes) else {
        return EgressError::Unavailable {
            message: format!("egress returned HTTP {status}"),
        };
    };
    let message = problem
        .detail
        .clone()
        .unwrap_or_else(|| problem.title.clone());
    match (status, problem.code.as_str()) {
        (404, _) => EgressError::NotFound,
        (409, code) => EgressError::Conflict {
            message: code.to_owned(),
        },
        (422, "config_invalid") => {
            let first = problem.errors.first();
            EgressError::InvalidConfig {
                pointer: first.map(|e| e.pointer.clone()).unwrap_or_default(),
                message: first.map(|e| e.message.clone()).unwrap_or(message),
            }
        }
        (503 | 500, "unavailable" | "internal") => EgressError::Unavailable { message },
        (_, code) => EgressError::Rejected {
            code: code.to_owned(),
            message,
        },
    }
}

fn wire_job(req: &IssueJobToken, ctx_day: gantry_core::time::Date) -> Result<Value, EgressError> {
    req.validate()?;
    let ledger = match (req.delivery_id, req.conversation_id) {
        (Some(id), None) => LedgerRef::Delivery { id },
        (None, Some(id)) => LedgerRef::Conversation { id, day: ctx_day },
        _ => unreachable!("validate rejects this shape"),
    };
    let wire = IssueJobTokenRequest {
        job_id: req.job_id,
        sandbox_id: req.sandbox_id,
        project_id: req.project_id,
        ledger,
        tier: req.tier,
        provider: req.provider.as_str().to_owned(),
        profile: req.profile.as_str().to_owned(),
        groups: req.groups.iter().map(|g| g.as_str().to_owned()).collect(),
        tokens_total: req.tokens_total,
        token_weights: req.weights,
        wall_time_s: req.wall_time_s,
        config_rev: req.config_rev,
    };
    serde_json::to_value(wire).map_err(internal)
}

fn job_usage_of(usage: FinalUsage) -> JobUsage {
    JobUsage {
        usage: usage.usage,
        weighted_tokens: usage.weighted_tokens,
        capped: usage.capped,
        incomplete: usage.incomplete,
        upstream_errors: usage.upstream_errors,
        denied: usage.denied,
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, EgressError> {
    serde_json::to_value(value).map_err(internal)
}

#[async_trait]
impl EgressControl for EgressClient {
    async fn endpoints(&self) -> Result<EgressEndpoints, EgressError> {
        let endpoints: EndpointsResponse = self
            .call(
                Method::GET,
                "/control/v1/endpoints",
                None,
                None,
                self.token_timeout,
            )
            .await?;
        let host = endpoints
            .connect_proxy_url
            .strip_prefix("http://")
            .unwrap_or(&endpoints.connect_proxy_url)
            .to_owned();
        Ok(EgressEndpoints {
            connect_proxy_host: host,
            add_host: None,
        })
    }

    async fn issue_job_token(
        &self,
        req: IssueJobToken,
        ctx: &ActorCtx,
    ) -> Result<JobTokenGrant, EgressError> {
        let body = wire_job(&req, self.clock.now().to_date())?;
        let grant: IssueJobTokenResponse = self
            .call(
                Method::POST,
                "/control/v1/jobs",
                Some(body),
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        Ok(JobTokenGrant {
            job_token: grant.job_token,
            model_base_urls: grant.model_base_urls,
            connect_proxy_url: grant.connect_proxy_url,
            registry_routes: BTreeMap::new(),
            effective_groups: grant
                .effective_groups
                .into_iter()
                .map(GroupName::new)
                .collect(),
            issued_at: grant.issued_at,
            expires_at: grant.expires_at,
            reissued: grant.reissued,
        })
    }

    async fn job_usage(&self, job_id: &AttemptId) -> Result<JobUsage, EgressError> {
        let usage: FinalUsage = self
            .call(
                Method::GET,
                &format!("/control/v1/jobs/{job_id}/usage"),
                None,
                None,
                self.usage_timeout,
            )
            .await?;
        Ok(job_usage_of(usage))
    }

    async fn revoke_job_token(
        &self,
        job_id: &AttemptId,
        ctx: &ActorCtx,
    ) -> Result<FinalUsage, EgressError> {
        self.call(
            Method::POST,
            &format!("/control/v1/jobs/{job_id}/revoke"),
            Some(json!({})),
            Some(ctx),
            self.token_timeout,
        )
        .await
    }

    async fn forget_job(&self, job_id: &AttemptId, ctx: &ActorCtx) -> Result<(), EgressError> {
        let _: Value = self
            .call(
                Method::DELETE,
                &format!("/control/v1/jobs/{job_id}"),
                None,
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        Ok(())
    }

    async fn service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        let cached = self.svc.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(grant) = cached {
            let now = self.clock.now().as_millis();
            if grant.expires_at.as_millis() - now > ROTATE_BEFORE_S * 1000 {
                return Ok(grant);
            }
        }
        self.issue_svc(ctx).await
    }

    async fn rotate_service_token(&self, ctx: &ActorCtx) -> Result<SvcTokenGrant, EgressError> {
        self.issue_svc(ctx).await
    }

    async fn apply_config(
        &self,
        view: EgressConfigView,
        ctx: &ActorCtx,
    ) -> Result<AppliedConfig, EgressError> {
        let body = to_json(&view)?;
        let applied: ConfigApplyResponse = self
            .call(
                Method::PUT,
                "/control/v1/config",
                Some(body),
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        Ok(applied)
    }

    async fn secret_present(
        &self,
        refs: &[SecretName],
    ) -> Result<BTreeMap<SecretName, bool>, EgressError> {
        let present: PresentResponse = self
            .call(
                Method::POST,
                "/control/v1/secrets/present",
                Some(json!({ "refs": refs })),
                None,
                self.token_timeout,
            )
            .await?;
        Ok(present.present)
    }

    async fn stage_secret(
        &self,
        req: StageSecret,
        ctx: &ActorCtx,
    ) -> Result<StagedSecret, EgressError> {
        let value = req.value.expose_secret().to_owned();
        // 值只在这一个请求体里出现；不进日志、不进错误。
        let body = json!({
            "request_id": ctx.request_id,
            "secret_ref": req.secret_ref,
            "value": value,
        });
        let staged: StageSecretResponse = self
            .call(
                Method::POST,
                "/control/v1/secrets/stage",
                Some(body),
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        Ok(StagedSecret {
            consumer: Consumer::Egress,
            staged_id: staged.staged_id,
            secret_ref: staged.secret_ref,
            version: SecretVersion::new(staged.version).ok_or_else(|| internal("version 0"))?,
        })
    }

    async fn activate_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<SecretVersion, EgressError> {
        let activated: ActivateSecretResponse = self
            .call(
                Method::POST,
                &format!("/control/v1/secrets/staged/{}/activate", staged.staged_id),
                Some(json!({})),
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        SecretVersion::new(activated.version).ok_or_else(|| internal("version 0"))
    }

    async fn discard_secret(
        &self,
        staged: &StagedSecret,
        ctx: &ActorCtx,
    ) -> Result<(), EgressError> {
        let _: Value = self
            .call(
                Method::POST,
                &format!("/control/v1/secrets/staged/{}/discard", staged.staged_id),
                Some(json!({})),
                Some(ctx),
                self.token_timeout,
            )
            .await?;
        Ok(())
    }

    async fn staged_secrets(&self) -> Result<Vec<StagedInfo>, EgressError> {
        let list: StagedSecretList = self
            .call(
                Method::GET,
                "/control/v1/secrets/staged",
                None,
                None,
                self.token_timeout,
            )
            .await?;
        Ok(list.items)
    }

    async fn call_usage(&self, call_id: &CallId) -> Result<CallUsage, EgressError> {
        let page = self
            .page(format!("/control/v1/usage?call_id={call_id}"))
            .await?;
        if page.items.is_empty() {
            return Err(EgressError::NotFound);
        }
        let mut usage = TokenUsage::default();
        let mut weighted_tokens = 0u64;
        for record in &page.items {
            usage.input_tokens += record.usage.input_tokens;
            usage.output_tokens += record.usage.output_tokens;
            usage.cache_read_tokens += record.usage.cache_read_tokens;
            usage.cache_write_tokens += record.usage.cache_write_tokens;
            usage.requests += record.usage.requests;
            weighted_tokens += record.weighted_tokens;
        }
        Ok(CallUsage {
            call_id: *call_id,
            usage,
            weighted_tokens,
            usage_source: UsageSource::Metered,
            incomplete: page.items.iter().any(|record| record.incomplete),
        })
    }

    async fn usage_since(&self, cursor: UsageCursor, limit: u32) -> Result<UsagePage, EgressError> {
        self.page(format!(
            "/control/v1/usage?after={}&limit={}",
            cursor.as_str(),
            limit.max(1)
        ))
        .await
    }

    async fn svc_token_usage(
        &self,
        id: &SvcTokenId,
        cursor: Option<UsageCursor>,
    ) -> Result<UsagePage, EgressError> {
        let after = cursor
            .map(|cursor| format!("&after={}", cursor.as_str()))
            .unwrap_or_default();
        self.page(format!("/control/v1/usage?svc_token_id={id}{after}"))
            .await
    }

    async fn close_call(
        &self,
        svc_token_id: &SvcTokenId,
        call_id: &CallId,
        ctx: &ActorCtx,
    ) -> Result<CallClosed, EgressError> {
        let body = to_json(&RevokeSvcTokenRequest::call(*call_id))?;
        let closed: CloseCallResponse = self
            .call(
                Method::POST,
                &format!("/control/v1/svc-tokens/{svc_token_id}/revoke"),
                Some(body),
                Some(ctx),
                self.usage_timeout,
            )
            .await?;
        Ok(closed)
    }

    async fn call_terminal(&self, call_id: &CallId) -> Result<CallStatement, EgressError> {
        let page = self
            .page(format!("/control/v1/usage?call_id={call_id}"))
            .await?;
        let items = page
            .items
            .into_iter()
            .filter(|record| {
                matches!(&record.caller, UsageCaller::Svc { call_id: id, .. } if id == call_id)
            })
            .collect();
        let terminal: Option<CallTerminal> = page.terminal;
        Ok(CallStatement {
            call_id: *call_id,
            items,
            terminal,
        })
    }

    async fn status(&self) -> Result<EgressStatus, EgressError> {
        let status: StatusResponse = self
            .call(
                Method::GET,
                "/control/v1/status",
                None,
                None,
                self.token_timeout,
            )
            .await?;
        Ok(EgressStatus {
            applied_rev: status
                .applied_rev
                .and_then(|rev| Seq::from_wire(rev).ok())
                .unwrap_or(Seq::ZERO),
            health: match status.health {
                EgressHealth::Ok => Health::Ok,
                EgressHealth::Degraded => Health::Degraded,
                EgressHealth::Unhealthy => Health::Unhealthy,
            },
            detail: status.detail,
            unresolved_secret_refs: status.unresolved_secret_refs,
            active_job_tokens: u32::try_from(status.active_jobs).unwrap_or(u32::MAX),
        })
    }
}
