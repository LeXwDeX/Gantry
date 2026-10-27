//! GitHub REST 客户端（forge.md §7.1：`pub(crate)`，不对外暴露，机械防线 I2）。
//!
//! 只实现执行器用到的调用；**不提供**任何 review `APPROVE` 类调用（P-12，FA §3.4）。
//! 端点与字段按 FA §3.2 的草案，真实 GitHub 行为以后续 spike 为准（forge.md F-OQ-2）。

pub(crate) mod http;

use std::time::Duration;

use gantry_core::canon::RepoSlug;
use gantry_core::secret::SecretString;
use serde_json::Value;

use http::{HttpRequest, HttpResponse, TransportError};

#[derive(Clone, Debug)]
pub(crate) struct GithubClient {
    base: String,
    timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
    /// 网络错误、超时、连接重置：结果未知（FA §2.6 `transient`，先 probe 再重试）。
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("github status {status}")]
    Status {
        status: u16,
        retry_after: Option<Duration>,
        rate_limited: bool,
        message: String,
    },
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullView {
    pub number: u64,
    pub node_id: String,
    pub state: String,
    pub merged: bool,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub user_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IssueView {
    pub number: u64,
    pub state: String,
}

impl GithubClient {
    pub fn new(base: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base: base.into(),
            timeout,
        }
    }

    async fn call(
        &self,
        token: &SecretString,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ApiError> {
        let mut headers = vec![
            ("accept", "application/vnd.github+json".to_owned()),
            ("x-github-api-version", "2022-11-28".to_owned()),
            ("user-agent", "gantry-forge".to_owned()),
            ("authorization", format!("Bearer {}", token.expose_secret())),
        ];
        let body = body.map(|value| value.to_string().into_bytes());
        if body.is_some() {
            headers.push(("content-type", "application/json".to_owned()));
        }
        let response = http::send(
            &self.base,
            HttpRequest {
                method,
                path,
                headers,
                body,
            },
            self.timeout,
        )
        .await?;
        if (200..300).contains(&response.status) {
            Ok(response)
        } else {
            Err(status_error(&response))
        }
    }

    async fn json(
        &self,
        token: &SecretString,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let response = self.call(token, method, path, body).await?;
        serde_json::from_slice(&response.body).map_err(|error| ApiError::Decode(error.to_string()))
    }

    pub async fn get_pull(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<PullView, ApiError> {
        let value = self
            .json(token, "GET", &format!("/repos/{repo}/pulls/{number}"), None)
            .await?;
        pull_view(&value)
    }

    /// `GET /pulls?head={owner}:{branch}&base={base}&state=all`（FA §3.2 `create_pr` probe）。
    pub async fn list_pulls(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        head_branch: &str,
        base: &str,
    ) -> Result<Vec<PullView>, ApiError> {
        let owner = repo.as_str().split('/').next().unwrap_or_default();
        let path = format!(
            "/repos/{repo}/pulls?head={}&base={}&state=all&per_page=100",
            encode(&format!("{owner}:{head_branch}")),
            encode(base)
        );
        let value = self.json(token, "GET", &path, None).await?;
        value
            .as_array()
            .ok_or_else(|| ApiError::Decode("pulls list is not an array".into()))?
            .iter()
            .map(pull_view)
            .collect()
    }

    pub async fn create_pull(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        body: &Value,
    ) -> Result<PullView, ApiError> {
        let value = self
            .json(token, "POST", &format!("/repos/{repo}/pulls"), Some(body))
            .await?;
        pull_view(&value)
    }

    pub async fn update_pull(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
        body: &Value,
    ) -> Result<PullView, ApiError> {
        let value = self
            .json(
                token,
                "PATCH",
                &format!("/repos/{repo}/pulls/{number}"),
                Some(body),
            )
            .await?;
        pull_view(&value)
    }

    /// `PUT /pulls/{n}/merge`（带 `sha`）。绝不传 `bypass_rules`（FA §2.5）。
    pub async fn merge_pull(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
        body: &Value,
    ) -> Result<(), ApiError> {
        self.call(
            token,
            "PUT",
            &format!("/repos/{repo}/pulls/{number}/merge"),
            Some(body),
        )
        .await
        .map(|_| ())
    }

    pub async fn get_issue(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<IssueView, ApiError> {
        let value = self
            .json(
                token,
                "GET",
                &format!("/repos/{repo}/issues/{number}"),
                None,
            )
            .await?;
        issue_view(&value)
    }

    pub async fn update_issue(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
        body: &Value,
    ) -> Result<IssueView, ApiError> {
        let value = self
            .json(
                token,
                "PATCH",
                &format!("/repos/{repo}/issues/{number}"),
                Some(body),
            )
            .await?;
        issue_view(&value)
    }

    /// `POST /app/installations/{id}/access_tokens`（以 App JWT 认证，FA §9.4）。
    pub async fn create_installation_token(
        &self,
        jwt: &SecretString,
        installation_id: i64,
        body: &Value,
    ) -> Result<Value, ApiError> {
        self.json(
            jwt,
            "POST",
            &format!("/app/installations/{installation_id}/access_tokens"),
            Some(body),
        )
        .await
    }

    /// `GET /app/installations/{id}`（以 App JWT 认证；返回结构需核实，forge.md §7.12）。
    pub async fn get_installation(
        &self,
        jwt: &SecretString,
        installation_id: i64,
    ) -> Result<Value, ApiError> {
        self.json(
            jwt,
            "GET",
            &format!("/app/installations/{installation_id}"),
            None,
        )
        .await
    }

    /// 远端分支 head；分支不存在返回 `None`。
    pub async fn branch_head(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        branch: &str,
    ) -> Result<Option<String>, ApiError> {
        match self
            .json(
                token,
                "GET",
                &format!("/repos/{repo}/git/ref/heads/{branch}"),
                None,
            )
            .await
        {
            Ok(value) => Ok(value
                .pointer("/object/sha")
                .and_then(Value::as_str)
                .map(str::to_owned)),
            Err(ApiError::Status { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn status_error(response: &HttpResponse) -> ApiError {
    let retry_after = response
        .headers
        .get("retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    let message = serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let remaining_zero = response
        .headers
        .get("x-ratelimit-remaining")
        .is_some_and(|value| value == "0");
    // FA §10.3：带 retry-after、剩余为 0，或消息表明二级限流，才算限流；否则 403 是缺权限。
    let rate_limited = matches!(response.status, 403 | 429)
        && (retry_after.is_some()
            || remaining_zero
            || message.to_ascii_lowercase().contains("rate limit"));
    ApiError::Status {
        status: response.status,
        retry_after,
        rate_limited,
        message,
    }
}

fn str_field(value: &Value, pointer: &str) -> Result<String, ApiError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::Decode(format!("missing {pointer}")))
}

fn pull_view(value: &Value) -> Result<PullView, ApiError> {
    Ok(PullView {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| ApiError::Decode("missing /number".into()))?,
        node_id: str_field(value, "/node_id")?,
        state: str_field(value, "/state")?,
        merged: value
            .get("merged")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        title: str_field(value, "/title")?,
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        head_ref: str_field(value, "/head/ref")?,
        head_sha: str_field(value, "/head/sha")?,
        base_ref: str_field(value, "/base/ref")?,
        user_id: value.pointer("/user/id").and_then(Value::as_i64),
    })
}

fn issue_view(value: &Value) -> Result<IssueView, ApiError> {
    Ok(IssueView {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| ApiError::Decode("missing /number".into()))?,
        state: str_field(value, "/state")?,
    })
}

/// 查询串的最小百分号编码（RFC 3986 unreserved 之外的字节一律编码，`:` 与 `/` 例外）。
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
