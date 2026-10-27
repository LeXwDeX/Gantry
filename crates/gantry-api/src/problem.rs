//! 统一错误：`application/problem+json`（RFC 9457，api.md §3.1，本文 §7.4）。
//!
//! `code` 只取契约的 30 个稳定值，只增不改。`detail` 是面向日志的英文说明，
//! **不得**拼入外部文本（I5）；需要给客户端看的变量放扩展成员。

use gantry_core::entities::DeviceRole;
use http::{HeaderName, HeaderValue, StatusCode};
use serde_json::{Map, Value};

/// api.md §3.1 的 30 个稳定错误码。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProblemCode {
    InvalidRequest,
    IdempotencyKeyRequired,
    InvalidCursor,
    DeviceRevoked,
    PairTokenInvalid,
    ForbiddenRole,
    ForbiddenProfile,
    SignatureInvalid,
    SignatureRequired,
    NotFound,
    MethodNotAllowed,
    Conflict,
    IdempotencyInProgress,
    WorktreeLocked,
    ApprovalNotPending,
    ChallengeInvalid,
    StaleHead,
    ApprovalMismatch,
    CursorAhead,
    CursorExpired,
    PreconditionFailed,
    PayloadTooLarge,
    UnsupportedMediaType,
    ValidationFailed,
    NotOverridable,
    IdempotencyKeyReuse,
    PreconditionRequired,
    RateLimited,
    Internal,
    Unavailable,
}

impl ProblemCode {
    /// 契约表的顺序（api.md §3.1）。
    pub const ALL: [ProblemCode; 30] = [
        Self::InvalidRequest,
        Self::IdempotencyKeyRequired,
        Self::InvalidCursor,
        Self::DeviceRevoked,
        Self::PairTokenInvalid,
        Self::ForbiddenRole,
        Self::ForbiddenProfile,
        Self::SignatureInvalid,
        Self::SignatureRequired,
        Self::NotFound,
        Self::MethodNotAllowed,
        Self::Conflict,
        Self::IdempotencyInProgress,
        Self::WorktreeLocked,
        Self::ApprovalNotPending,
        Self::ChallengeInvalid,
        Self::StaleHead,
        Self::ApprovalMismatch,
        Self::CursorAhead,
        Self::CursorExpired,
        Self::PreconditionFailed,
        Self::PayloadTooLarge,
        Self::UnsupportedMediaType,
        Self::ValidationFailed,
        Self::NotOverridable,
        Self::IdempotencyKeyReuse,
        Self::PreconditionRequired,
        Self::RateLimited,
        Self::Internal,
        Self::Unavailable,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::IdempotencyKeyRequired => "idempotency_key_required",
            Self::InvalidCursor => "invalid_cursor",
            Self::DeviceRevoked => "device_revoked",
            Self::PairTokenInvalid => "pair_token_invalid",
            Self::ForbiddenRole => "forbidden_role",
            Self::ForbiddenProfile => "forbidden_profile",
            Self::SignatureInvalid => "signature_invalid",
            Self::SignatureRequired => "signature_required",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::Conflict => "conflict",
            Self::IdempotencyInProgress => "idempotency_in_progress",
            Self::WorktreeLocked => "worktree_locked",
            Self::ApprovalNotPending => "approval_not_pending",
            Self::ChallengeInvalid => "challenge_invalid",
            Self::StaleHead => "stale_head",
            Self::ApprovalMismatch => "approval_mismatch",
            Self::CursorAhead => "cursor_ahead",
            Self::CursorExpired => "cursor_expired",
            Self::PreconditionFailed => "precondition_failed",
            Self::PayloadTooLarge => "payload_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::ValidationFailed => "validation_failed",
            Self::NotOverridable => "not_overridable",
            Self::IdempotencyKeyReuse => "idempotency_key_reuse",
            Self::PreconditionRequired => "precondition_required",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest | Self::IdempotencyKeyRequired | Self::InvalidCursor => {
                StatusCode::BAD_REQUEST
            }
            Self::DeviceRevoked | Self::PairTokenInvalid => StatusCode::UNAUTHORIZED,
            Self::ForbiddenRole
            | Self::ForbiddenProfile
            | Self::SignatureInvalid
            | Self::SignatureRequired => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Conflict
            | Self::IdempotencyInProgress
            | Self::WorktreeLocked
            | Self::ApprovalNotPending
            | Self::ChallengeInvalid
            | Self::StaleHead
            | Self::ApprovalMismatch
            | Self::CursorAhead => StatusCode::CONFLICT,
            Self::CursorExpired => StatusCode::GONE,
            Self::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::ValidationFailed | Self::NotOverridable | Self::IdempotencyKeyReuse => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// RFC 9457 的 `title`：按 `code` 固定，不含任何请求内容。
    pub fn title(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid request",
            Self::IdempotencyKeyRequired => "Idempotency-Key is required",
            Self::InvalidCursor => "Invalid cursor",
            Self::DeviceRevoked => "Device is revoked",
            Self::PairTokenInvalid => "Pairing token is invalid",
            Self::ForbiddenRole => "Role is not sufficient",
            Self::ForbiddenProfile => "Route is not available for this client profile",
            Self::SignatureInvalid => "Approval signature is invalid",
            Self::SignatureRequired => "Approval signature is required",
            Self::NotFound => "Not found",
            Self::MethodNotAllowed => "Method not allowed",
            Self::Conflict => "Conflict",
            Self::IdempotencyInProgress => "Request with this Idempotency-Key is in progress",
            Self::WorktreeLocked => "Worktree is locked",
            Self::ApprovalNotPending => "Approval is not pending",
            Self::ChallengeInvalid => "Challenge is invalid",
            Self::StaleHead => "Approval head is stale",
            Self::ApprovalMismatch => "Approval does not match this write",
            Self::CursorAhead => "Cursor is ahead of the event log",
            Self::CursorExpired => "Cursor has expired",
            Self::PreconditionFailed => "Precondition failed",
            Self::PayloadTooLarge => "Payload too large",
            Self::UnsupportedMediaType => "Unsupported media type",
            Self::ValidationFailed => "Validation failed",
            Self::NotOverridable => "Resource is not overridable",
            Self::IdempotencyKeyReuse => "Idempotency-Key was reused with a different request",
            Self::PreconditionRequired => "Precondition required",
            Self::RateLimited => "Rate limited",
            Self::Internal => "Internal error",
            Self::Unavailable => "Service unavailable",
        }
    }
}

/// `validation_failed.errors[]` 的一条（api.md §3.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub pointer: String,
    pub code: &'static str,
    pub message: String,
}

impl FieldError {
    pub fn new(pointer: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            pointer: pointer.into(),
            code,
            message: message.into(),
        }
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("pointer".into(), Value::String(self.pointer.clone()));
        map.insert("code".into(), Value::String(self.code.into()));
        map.insert("message".into(), Value::String(self.message.clone()));
        Value::Object(map)
    }
}

/// 一次错误响应的全部内容；`request_id` 与 `instance` 在渲染时由管线填入。
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub code: ProblemCode,
    pub detail: String,
    pub ext: Box<Map<String, Value>>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

impl Problem {
    pub fn new(code: ProblemCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            ext: Box::default(),
            headers: Vec::new(),
        }
    }

    pub fn with_ext(mut self, key: &str, value: Value) -> Self {
        self.ext.insert(key.to_owned(), value);
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }

    pub fn not_found() -> Self {
        Self::new(ProblemCode::NotFound, "resource or route not found")
    }

    pub fn internal() -> Self {
        Self::new(ProblemCode::Internal, "internal error")
    }

    pub fn forbidden_role(required: DeviceRole) -> Self {
        Self::new(ProblemCode::ForbiddenRole, "device role is not sufficient")
            .with_ext("required_role", Value::String(role_str(required).into()))
    }

    pub fn conflict(reason: &str) -> Self {
        Self::new(ProblemCode::Conflict, "state conflict")
            .with_ext("reason", Value::String(reason.into()))
    }

    pub fn validation(errors: Vec<FieldError>) -> Self {
        let mut errors = errors;
        errors.truncate(crate::consts::VALIDATION_MAX_ERRORS);
        Self::new(ProblemCode::ValidationFailed, "request validation failed").with_ext(
            "errors",
            Value::Array(errors.iter().map(FieldError::to_json).collect()),
        )
    }

    pub fn retry_after(code: ProblemCode, detail: &str, secs: u64) -> Self {
        Self::new(code, detail)
            .with_header(http::header::RETRY_AFTER, HeaderValue::from(secs.max(1)))
    }

    /// 渲染 problem 正文。`instance` 是请求路径（不含查询串）。
    pub fn body(&self, instance: &str, request_id: &str) -> Value {
        let mut map = Map::new();
        map.insert(
            "type".into(),
            Value::String(format!("urn:gantry:problem:{}", self.code.as_str())),
        );
        map.insert("title".into(), Value::String(self.code.title().into()));
        map.insert("status".into(), Value::from(self.code.status().as_u16()));
        map.insert("code".into(), Value::String(self.code.as_str().into()));
        map.insert("detail".into(), Value::String(self.detail.clone()));
        map.insert("instance".into(), Value::String(instance.into()));
        map.insert("request_id".into(), Value::String(request_id.into()));
        for (key, value) in self.ext.iter() {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
}

pub(crate) fn role_str(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::Viewer => "viewer",
        DeviceRole::Operator => "operator",
        DeviceRole::Approver => "approver",
    }
}
