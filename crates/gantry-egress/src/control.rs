//! 控制面服务（`docs/interfaces/egress-control.md` §2、§4、§6）：`egress.sock` 上的
//! HTTP/1.1 + JSON，路径前缀 `/control/v1/`。
//!
//! 每个连接先读对端凭据，`uid` 不等于允许值就直接关闭、不回任何 HTTP 响应，并写一条
//! `control.peer_not_allowed` 审计（§2.2）。请求体严格校验：JSON 语法错误 `400
//! invalid_request`，字段缺失 / 未知字段 `422 validation_failed`。错误体统一为
//! `application/problem+json`。响应、错误与日志里都没有 secret 值。

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use gantry_core::egress::{
    ActivateSecretResponse, ConfigApplyResponse, ConfigView, IssueJobTokenRequest,
    IssueSvcTokenRequest, PresentRequest, PresentResponse, ProblemDetails, ProblemError,
    RevokeSvcTokenRequest, StageSecretResponse, StagedId, StagedSecretList, SvcTokenId,
};
use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::{AttemptId, Ulid};
use gantry_core::ports::context::ActorCtx;
use gantry_core::ports::secrets::SecretValue;
use gantry_core::secret::SecretName;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;

use crate::audit::{AuditCaller, AuditEvent, AuditKind, AuditSink, Decision};
use crate::registry::{ControlError, Registry};

/// 配置视图的请求体上限 4 MiB，其余 64 KiB（§2.1 [提议]）。
pub const CONFIG_BODY_LIMIT: usize = 4 * 1024 * 1024;
pub const BODY_LIMIT: usize = 64 * 1024;

const REQUEST_ID_HEADER: &str = "x-gantry-request-id";

/// 控制面服务。
#[derive(Clone)]
pub struct ControlServer {
    registry: Registry,
    audit: Arc<dyn AuditSink>,
    allowed_uid: u32,
}

impl ControlServer {
    pub fn new(registry: Registry, audit: Arc<dyn AuditSink>, allowed_uid: u32) -> Self {
        Self {
            registry,
            audit,
            allowed_uid,
        }
    }

    /// accept 循环；每个连接一个任务。
    pub async fn serve(self, listener: UnixListener) -> std::io::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                let uid = match stream.peer_cred() {
                    Ok(cred) => cred.uid(),
                    Err(_) => {
                        server.deny_peer("unknown");
                        return;
                    }
                };
                if uid != server.allowed_uid {
                    server.deny_peer(&format!("uid:{uid}"));
                    return; // 丢弃连接，不回任何 HTTP 响应
                }
                let service = hyper::service::service_fn(move |req| {
                    let server = server.clone();
                    async move { Ok::<_, Infallible>(server.handle(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }

    fn deny_peer(&self, src: &str) {
        let mut event = AuditEvent::new(
            AuditKind::Control,
            AuditCaller::Unknown { src: src.into() },
            Decision::Deny,
        );
        event.rule = Some("control.peer_not_allowed");
        if self.audit.record(event).is_err() {
            self.registry.set_audit_failed(true);
        }
    }

    async fn handle(&self, req: Request<Incoming>) -> Response<Full<Bytes>> {
        let request_id = req
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<Ulid>().ok());
        let mut response = match self.route(req, request_id).await {
            Ok(reply) => reply,
            Err(error) => problem(&error, request_id),
        };
        if let Some(id) = request_id {
            if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
                response.headers_mut().insert(REQUEST_ID_HEADER, value);
            }
        }
        response
    }

    async fn route(
        &self,
        req: Request<Incoming>,
        request_id: Option<Ulid>,
    ) -> Result<Response<Full<Bytes>>, ControlError> {
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        let query = req.uri().query().map(str::to_owned);
        let Some(rest) = path.strip_prefix("/control/v1/") else {
            return Err(ControlError::NotFound);
        };
        let segments: Vec<&str> = rest.split('/').collect();
        let registry = &self.registry;
        let ctx = actor_ctx(request_id);
        match (&method, segments.as_slice()) {
            (&Method::POST, ["jobs"]) => {
                let body: IssueJobTokenRequest = read_json(req, BODY_LIMIT).await?;
                let grant = registry.issue_job(body)?;
                let status = if grant.reissued {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                };
                json(status, &grant)
            }
            (&Method::POST, ["jobs", id, "revoke"]) => {
                let job_id = parse_attempt(id)?;
                read_empty(req).await?;
                json(StatusCode::OK, &registry.revoke_job(&job_id).await?)
            }
            (&Method::GET, ["jobs", id, "usage"]) => {
                let job_id = parse_attempt(id)?;
                json(StatusCode::OK, &registry.job_usage(&job_id)?)
            }
            (&Method::DELETE, ["jobs", id]) => {
                let job_id = parse_attempt(id)?;
                registry.forget_job(&job_id)?;
                json(StatusCode::OK, &Empty {})
            }
            (&Method::POST, ["svc-tokens"]) => {
                let body: IssueSvcTokenRequest = read_json(req, BODY_LIMIT).await?;
                json(StatusCode::CREATED, &registry.issue_svc(body)?)
            }
            (&Method::POST, ["svc-tokens", id, "revoke"]) => {
                let id = SvcTokenId::parse(id).map_err(|_| ControlError::NotFound)?;
                match read_json::<RevokeSvcTokenRequest>(req, BODY_LIMIT).await? {
                    RevokeSvcTokenRequest::Token(_) => {
                        json(StatusCode::OK, &registry.revoke_svc(&id)?)
                    }
                    RevokeSvcTokenRequest::Call(scope) => json(
                        StatusCode::OK,
                        &registry.close_call(&id, scope.call_id).await?,
                    ),
                }
            }
            (&Method::POST, ["secrets", "stage"]) => {
                let body: StageBody = read_json(req, BODY_LIMIT).await?;
                let value = SecretValue::from_bytes(body.value.into_bytes());
                let staged =
                    registry.stage_secret(body.request_id, body.secret_ref, value, &ctx)?;
                json(
                    StatusCode::CREATED,
                    &StageSecretResponse {
                        staged_id: staged.staged_id,
                        secret_ref: staged.secret_ref,
                        version: staged.version,
                        staged_at: staged.staged_at,
                        expires_at: staged.expires_at,
                    },
                )
            }
            (&Method::POST, ["secrets", "staged", id, "activate"]) => {
                let staged_id = parse_staged(id)?;
                read_empty(req).await?;
                let (secret_ref, version, activated_at) =
                    registry.activate_secret(&staged_id, &ctx)?;
                json(
                    StatusCode::OK,
                    &ActivateSecretResponse {
                        secret_ref,
                        version,
                        activated_at,
                    },
                )
            }
            (&Method::POST, ["secrets", "staged", id, "discard"]) => {
                let staged_id = parse_staged(id)?;
                read_empty(req).await?;
                registry.discard_secret(&staged_id, &ctx)?;
                json(StatusCode::OK, &Empty {})
            }
            (&Method::GET, ["secrets", "staged"]) => json(
                StatusCode::OK,
                &StagedSecretList {
                    items: registry.staged_secrets(&ctx)?,
                },
            ),
            (&Method::POST, ["secrets", "present"]) => {
                let body: PresentRequest = read_json(req, BODY_LIMIT).await?;
                let present: BTreeMap<SecretName, bool> = registry.secrets_present(&body.refs)?;
                json(StatusCode::OK, &PresentResponse { present })
            }
            (&Method::DELETE, ["secrets", name]) => {
                let name = SecretName::parse(name).map_err(|error| {
                    ControlError::Validation(vec![ProblemError {
                        pointer: "/secret_ref".into(),
                        message: error.to_string(),
                    }])
                })?;
                registry.delete_secret(&name, &ctx)?;
                json(StatusCode::OK, &Empty {})
            }
            (&Method::GET, ["usage"]) => {
                let query = UsageParams::parse(query.as_deref())?;
                let page = match (&query.call_id, &query.svc_token_id) {
                    (Some(call_id), None) if query.after.is_none() && query.limit.is_none() => {
                        registry.call_statement(*call_id)?
                    }
                    (None, Some(id)) => {
                        registry.svc_token_usage(id, query.after.as_deref(), query.limit)?
                    }
                    (None, None) => registry.usage_since(query.after.as_deref(), query.limit)?,
                    _ => {
                        return Err(ControlError::InvalidRequest(
                            "call_id cannot be combined with other parameters".into(),
                        ))
                    }
                };
                json(StatusCode::OK, &page)
            }
            (&Method::PUT, ["config"]) => {
                let view: ConfigView = read_json(req, CONFIG_BODY_LIMIT).await?;
                let applied_rev = registry.apply_config(view)?;
                json(StatusCode::OK, &ConfigApplyResponse { applied_rev })
            }
            (&Method::GET, ["status"]) => json(StatusCode::OK, &registry.status()?),
            (&Method::GET, ["endpoints"]) => json(StatusCode::OK, &registry.endpoints()),
            _ => Err(ControlError::NotFound),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// 暂存请求体：值读进来立刻转成 `SecretValue`，本结构不实现 `Debug`。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageBody {
    request_id: Ulid,
    secret_ref: SecretName,
    value: String,
}

#[derive(Default)]
struct UsageParams {
    after: Option<String>,
    limit: Option<u32>,
    call_id: Option<Ulid>,
    svc_token_id: Option<SvcTokenId>,
}

impl UsageParams {
    fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut params = Self::default();
        let bad = |message: &str| ControlError::InvalidRequest(message.to_owned());
        for pair in query
            .unwrap_or_default()
            .split('&')
            .filter(|p| !p.is_empty())
        {
            let (key, value) = pair.split_once('=').ok_or_else(|| bad("malformed query"))?;
            match key {
                "after" if params.after.is_none() => params.after = Some(value.to_owned()),
                "limit" if params.limit.is_none() => {
                    params.limit = Some(value.parse().map_err(|_| bad("limit must be a u32"))?)
                }
                "call_id" if params.call_id.is_none() => {
                    params.call_id = Some(value.parse().map_err(|_| bad("call_id must be a ULID"))?)
                }
                "svc_token_id" if params.svc_token_id.is_none() => {
                    params.svc_token_id = Some(
                        SvcTokenId::parse(value).map_err(|_| bad("svc_token_id is malformed"))?,
                    )
                }
                _ => return Err(bad("unknown or repeated query parameter")),
            }
        }
        Ok(params)
    }
}

fn actor_ctx(request_id: Option<Ulid>) -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::System, "gantryd"),
        device: None,
        role: None,
        client_profile: None,
        request_id: request_id.unwrap_or_else(|| Ulid::from_parts(0, 0).expect("zero ulid")),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn parse_attempt(raw: &str) -> Result<AttemptId, ControlError> {
    raw.parse::<AttemptId>().map_err(|_| ControlError::NotFound)
}

fn parse_staged(raw: &str) -> Result<StagedId, ControlError> {
    StagedId::parse(raw).map_err(|_| ControlError::NotFound)
}

async fn read_body(req: Request<Incoming>, limit: usize) -> Result<Bytes, ControlError> {
    Limited::new(req.into_body(), limit)
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|_| ControlError::InvalidRequest(format!("body exceeds {limit} bytes")))
}

async fn read_json<T: DeserializeOwned>(
    req: Request<Incoming>,
    limit: usize,
) -> Result<T, ControlError> {
    let body = read_body(req, limit).await?;
    serde_json::from_slice(&body).map_err(|error| {
        if error.is_data() {
            ControlError::Validation(vec![ProblemError {
                pointer: "/".into(),
                // 只给位置与类别，不回显请求体内容（可能含 secret 值）。
                message: format!(
                    "invalid field at line {} column {}",
                    error.line(),
                    error.column()
                ),
            }])
        } else {
            ControlError::InvalidRequest("request body is not valid JSON".into())
        }
    })
}

/// 请求体必须是 `{}`（或为空）。
async fn read_empty(req: Request<Incoming>) -> Result<(), ControlError> {
    let body = read_body(req, BODY_LIMIT).await?;
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    serde_json::from_slice::<Empty>(&body)
        .map(|_| ())
        .map_err(|error| {
            if error.is_data() {
                ControlError::Validation(vec![ProblemError {
                    pointer: "/".into(),
                    message: "request body must be {}".into(),
                }])
            } else {
                ControlError::InvalidRequest("request body is not valid JSON".into())
            }
        })
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Result<Response<Full<Bytes>>, ControlError> {
    let bytes =
        serde_json::to_vec(body).map_err(|error| ControlError::Internal(error.to_string()))?;
    let mut response = Response::new(Full::new(Bytes::from(bytes)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

fn problem(error: &ControlError, request_id: Option<Ulid>) -> Response<Full<Bytes>> {
    let details = ProblemDetails {
        type_uri: format!("urn:gantry:problem:{}", error.code()),
        title: title(error).into(),
        status: error.status(),
        code: error.code().into(),
        detail: Some(error.to_string()),
        request_id,
        errors: error.errors().to_vec(),
    };
    let bytes = serde_json::to_vec(&details).unwrap_or_default();
    let mut response = Response::new(Full::new(Bytes::from(bytes)));
    *response.status_mut() =
        StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

fn title(error: &ControlError) -> &'static str {
    match error {
        ControlError::InvalidRequest(_) => "Invalid request",
        ControlError::NotFound => "Not found",
        ControlError::BindingMismatch => "Job binding mismatch",
        ControlError::JobRevoked => "Job already revoked",
        ControlError::StaleRev => "Stale config revision",
        ControlError::Conflict(_) => "Conflict",
        ControlError::StagedExpired => "Staged secret expired",
        ControlError::CursorExpired => "Usage cursor expired",
        ControlError::Validation(_) => "Validation failed",
        ControlError::CredentialMissing(_) => "Credential missing",
        ControlError::ConfigInvalid(_) => "Config invalid",
        ControlError::NotConfigured => "Not configured",
        ControlError::Unavailable(_) => "Unavailable",
        ControlError::Internal(_) => "Internal error",
    }
}
