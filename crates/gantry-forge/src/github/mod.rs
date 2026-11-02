//! GitHub REST 客户端（forge.md §7.1：`pub(crate)`，不对外暴露，机械防线 I2）。
//!
//! 只实现执行器用到的调用；**不提供**任何 review `APPROVE` 类调用（P-12，FA §3.4）。
//! 端点与字段按 FA §3.2 的草案，真实 GitHub 行为以后续 spike 为准（forge.md F-OQ-2）。

pub(crate) mod http;

use std::collections::{BTreeMap, BTreeSet};
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
        self.call_with(Some(token), method, path, body).await
    }

    async fn call_with(
        &self,
        token: Option<&SecretString>,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, ApiError> {
        let mut headers = vec![
            ("accept", "application/vnd.github+json".to_owned()),
            ("x-github-api-version", "2022-11-28".to_owned()),
            ("user-agent", "gantry-forge".to_owned()),
        ];
        if let Some(token) = token {
            headers.push(("authorization", format!("Bearer {}", token.expose_secret())));
        }
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

    async fn json_page(
        &self,
        token: &SecretString,
        path: &str,
    ) -> Result<(Value, BTreeMap<String, String>), ApiError> {
        let response = self.call(token, "GET", path, None).await?;
        let value = serde_json::from_slice(&response.body)
            .map_err(|error| ApiError::Decode(error.to_string()))?;
        Ok((value, response.headers))
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

    /// Issue 或 PR 的原始 JSON（取标题与正文，forge.md P-59）。
    pub async fn issue_or_pull(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
        pull: bool,
    ) -> Result<Value, ApiError> {
        let kind = if pull { "pulls" } else { "issues" };
        self.json(
            token,
            "GET",
            &format!("/repos/{repo}/{kind}/{number}"),
            None,
        )
        .await
    }

    /// 对话区评论的一页（Issue 与 PR 共用 `issues/{n}/comments`）。
    pub async fn list_comments(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
        since: Option<&str>,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<Value>, ApiError> {
        let mut path =
            format!("/repos/{repo}/issues/{number}/comments?per_page={per_page}&page={page}");
        if let Some(since) = since {
            path.push_str("&since=");
            path.push_str(since);
        }
        match self.json(token, "GET", &path, None).await? {
            Value::Array(items) => Ok(items),
            _ => Err(ApiError::Decode("comments is not an array".into())),
        }
    }

    /// 不带鉴权的调用：App Manifest 转换（`code` 本身即凭据）与公开的用户查询（forge.md §7.14）。
    pub async fn anonymous_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let response = self.call_with(None, method, path, body).await?;
        serde_json::from_slice(&response.body).map_err(|error| ApiError::Decode(error.to_string()))
    }

    /// PR 的 reviews（一页，最多 100 条；§7.5 第 7 点的 `review_state` 口径）。
    pub async fn list_reviews(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        number: u64,
    ) -> Result<Vec<Value>, ApiError> {
        match self
            .json(
                token,
                "GET",
                &format!("/repos/{repo}/pulls/{number}/reviews?per_page=100"),
                None,
            )
            .await?
        {
            Value::Array(items) => Ok(items),
            _ => Err(ApiError::Decode("reviews is not an array".into())),
        }
    }

    /// 某 commit ref 的 check runs。使用官方 `filter=latest`：服务端返回的集合即最新的
    /// check runs；客户端按 id 独立保留，不再按 app + name 合并（官方没有保证 run id 单调，
    /// 同 app 不同 workflow 也可能同名）。
    ///
    /// 分页严格按 GitHub `Link: rel="next"` 走：只接受同源、同 repo、同 head、同端点的
    /// next URL，检测循环并限制页数。首页的 `total_count` 是固定基准：后续页必须相同，
    /// 跨页不得重复 run id，唯一条目数必须恰好等于 total，否则整体失败。
    /// 官方限制是单 ref 最近 1000 个 check suite；命中该上限时无法证明集合完整，保守失败。
    pub async fn list_check_runs(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        sha: &str,
    ) -> Result<Vec<Value>, ApiError> {
        const PER_PAGE: usize = 100;
        const MAX_PAGES: usize = 100;
        const MAX_SUITES: usize = 1000;
        let endpoint = format!("/repos/{repo}/commits/{sha}/check-runs");
        let mut path = format!("{endpoint}?filter=latest&per_page={PER_PAGE}&page=1");
        let mut visited = BTreeSet::new();
        let mut seen_ids = BTreeSet::new();
        let mut runs = Vec::new();
        let mut total: Option<u64> = None;
        let mut page = 1;
        loop {
            if !visited.insert(path.clone()) {
                return Err(ApiError::Decode("check-runs pagination loop".into()));
            }
            let (value, headers) = self.json_page(token, &path).await?;
            let page_total = value
                .get("total_count")
                .and_then(Value::as_u64)
                .ok_or_else(|| ApiError::Decode("check-runs lacks total_count".into()))?;
            match total {
                None => total = Some(page_total),
                Some(expected) if expected != page_total => {
                    return Err(ApiError::Decode(format!(
                        "check-runs total_count changed: {expected} -> {page_total}"
                    )))
                }
                Some(_) => {}
            }
            let page_runs = value
                .get("check_runs")
                .and_then(Value::as_array)
                .ok_or_else(|| ApiError::Decode("check-runs lacks check_runs array".into()))?;
            if u64::try_from(page_runs.len()).unwrap_or(u64::MAX) > page_total {
                return Err(ApiError::Decode(
                    "check-runs page exceeds total_count".into(),
                ));
            }
            if u64::try_from(runs.len().saturating_add(page_runs.len())).unwrap_or(u64::MAX)
                > page_total
            {
                return Err(ApiError::Decode(
                    "check-runs accumulated entries exceed total_count".into(),
                ));
            }
            for run in page_runs {
                let id = run
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::Decode("check run lacks id".into()))?;
                if !seen_ids.insert(id) {
                    return Err(ApiError::Decode(format!(
                        "check-runs pagination repeated run id {id}"
                    )));
                }
                runs.push(run.clone());
            }
            let Some(next) = next_link(&headers, &self.base, &endpoint)? else {
                break;
            };
            if page >= MAX_PAGES {
                return Err(ApiError::Decode(
                    "check-runs pagination exceeds bound".into(),
                ));
            }
            path = next
                .strip_prefix(self.base.trim_end_matches('/'))
                .unwrap_or(&next)
                .to_owned();
            page += 1;
        }
        let total = total.unwrap_or(0);
        if u64::try_from(runs.len()).unwrap_or(u64::MAX) != total {
            return Err(ApiError::Decode(format!(
                "check-runs pagination incomplete: {}/{total}",
                runs.len()
            )));
        }
        let suites: BTreeSet<u64> = runs
            .iter()
            .map(|run| {
                run.pointer("/check_suite/id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::Decode("check run lacks check_suite.id".into()))
            })
            .collect::<Result<_, _>>()?;
        if suites.len() >= MAX_SUITES {
            return Err(ApiError::Decode(
                "check-runs reached the 1000-suite fetch bound; result may be truncated".into(),
            ));
        }
        Ok(runs)
    }

    /// 某 commit ref 的传统 commit statuses（`GET /commits/{ref}/statuses`）。
    ///
    /// 官方按创建时间倒序返回（最新在前）；调用方按 `context` 只保留最新一条。
    /// 该端点没有 `total_count`：分页严格按 `Link` 走，跨页重复 id 视为分页错误。
    pub async fn list_commit_statuses(
        &self,
        token: &SecretString,
        repo: &RepoSlug,
        sha: &str,
    ) -> Result<Vec<Value>, ApiError> {
        const PER_PAGE: usize = 100;
        const MAX_PAGES: usize = 100;
        let endpoint = format!("/repos/{repo}/commits/{sha}/statuses");
        let mut path = format!("{endpoint}?per_page={PER_PAGE}&page=1");
        let mut visited = BTreeSet::new();
        let mut seen_ids = BTreeSet::new();
        let mut statuses = Vec::new();
        let mut page = 1;
        loop {
            if !visited.insert(path.clone()) {
                return Err(ApiError::Decode("commit statuses pagination loop".into()));
            }
            let (value, headers) = self.json_page(token, &path).await?;
            let items = value
                .as_array()
                .ok_or_else(|| ApiError::Decode("commit statuses is not an array".into()))?;
            for item in items {
                let id = item
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::Decode("commit status lacks id".into()))?;
                if !seen_ids.insert(id) {
                    return Err(ApiError::Decode(format!(
                        "commit statuses pagination repeated id {id}"
                    )));
                }
                statuses.push(item.clone());
            }
            let Some(next) = next_link(&headers, &self.base, &endpoint)? else {
                break;
            };
            if page >= MAX_PAGES {
                return Err(ApiError::Decode(
                    "commit statuses pagination exceeds bound".into(),
                ));
            }
            path = next
                .strip_prefix(self.base.trim_end_matches('/'))
                .unwrap_or(&next)
                .to_owned();
            page += 1;
        }
        Ok(statuses)
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

/// 解析 `Link` 头里的 `rel="next"`，并校验 next 只指向本次请求的同一端点。
fn next_link(
    headers: &BTreeMap<String, String>,
    base: &str,
    endpoint: &str,
) -> Result<Option<String>, ApiError> {
    let Some(header) = headers.get("link") else {
        return Ok(None);
    };
    let mut next = None;
    for (url, params) in parse_links(header) {
        let is_next = params.split(';').any(|param| {
            let param = param.trim();
            param == "rel=next" || param == "rel=\"next\""
        });
        if is_next {
            if next.is_some() {
                return Err(ApiError::Decode("multiple next links".into()));
            }
            next = Some(url);
        }
    }
    if let Some(url) = &next {
        validate_next_url(base, endpoint, url)?;
    }
    Ok(next)
}

fn parse_links(header: &str) -> Vec<(String, String)> {
    let mut links = Vec::new();
    let mut rest = header;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start + 1..].find('>') else {
            break;
        };
        let url = rest[start + 1..start + 1 + end].to_owned();
        let after = &rest[start + 1 + end + 1..];
        let params_end = after.find(',').unwrap_or(after.len());
        links.push((url, after[..params_end].to_owned()));
        rest = &after[params_end..];
        if let Some(stripped) = rest.strip_prefix(',') {
            rest = stripped;
        }
    }
    links
}

fn validate_next_url(base: &str, endpoint: &str, url: &str) -> Result<(), ApiError> {
    if url.contains('#') || url.chars().any(char::is_whitespace) {
        return Err(ApiError::Decode("pagination link has invalid URL".into()));
    }
    let base = base.trim_end_matches('/');
    let rest = url
        .strip_prefix(base)
        .ok_or_else(|| ApiError::Decode("pagination link leaves the API origin".into()))?;
    if !rest.starts_with('/') {
        return Err(ApiError::Decode(
            "pagination link leaves the API origin".into(),
        ));
    }
    let path = rest.split('?').next().unwrap_or(rest);
    if path != endpoint {
        return Err(ApiError::Decode(format!(
            "pagination link points to the wrong endpoint: {path}"
        )));
    }
    Ok(())
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

#[cfg(test)]
mod link_tests {
    use std::collections::BTreeMap;

    use super::next_link;

    const BASE: &str = "http://127.0.0.1:9";
    const ENDPOINT: &str = "/repos/acme/shop/commits/abc/check-runs";

    fn headers(link: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("link".to_owned(), link.to_owned())])
    }

    #[test]
    fn next_link_accepts_same_origin_and_endpoint() {
        let link = format!("<{BASE}{ENDPOINT}?page=2>; rel=\"next\"");
        assert_eq!(
            next_link(&headers(&link), BASE, ENDPOINT).unwrap(),
            Some(format!("{BASE}{ENDPOINT}?page=2"))
        );
    }

    #[test]
    fn next_link_rejects_cross_origin() {
        let link =
            "<http://evil.example/repos/acme/shop/commits/abc/check-runs?page=2>; rel=\"next\"";
        assert!(next_link(&headers(link), BASE, ENDPOINT).is_err());
    }

    #[test]
    fn next_link_rejects_cross_endpoint() {
        let link =
            format!("<{BASE}/repos/acme/shop/commits/abc/check-suites?page=2>; rel=\"next\"");
        assert!(next_link(&headers(&link), BASE, ENDPOINT).is_err());
    }

    #[test]
    fn next_link_rejects_fragment_and_relative_urls() {
        let fragment = format!("<{BASE}{ENDPOINT}?page=2#x>; rel=\"next\"");
        assert!(next_link(&headers(&fragment), BASE, ENDPOINT).is_err());
        let relative = "<relative?page=2>; rel=\"next\"";
        assert!(next_link(&headers(relative), BASE, ENDPOINT).is_err());
    }
}
