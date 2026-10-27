//! 查询参数（管线步骤 9）：每条路由允许的参数与类型，取自 `openapi.yaml`
//! （测试 `query_params_match_openapi` 对账）。未知参数、重复的非重复参数、
//! 类型 / 范围 / 枚举错误一律 `400 invalid_request`。

use std::collections::BTreeMap;

use crate::registry::is_prefixed_ulid;

const STAGES: &[&str] = &[
    "spec",
    "plan",
    "implement",
    "propose",
    "review",
    "merge",
    "closed",
    "abandoned",
];
const STAGE_STATUSES: &[&str] = &["running", "waiting_human", "blocked", "paused", "done"];
const ORDER: &[&str] = &["asc", "desc"];
const INBOX_STATUS: &[&str] = &["open", "resolved", "dismissed"];
const INBOX_KIND: &[&str] = &[
    "approval_needed",
    "decision_needed",
    "spec_needed",
    "budget_exhausted",
    "escalation",
    "security_finding",
];
const APPROVAL_STATUS: &[&str] = &[
    "pending",
    "granted",
    "consumed",
    "invalidated",
    "expired",
    "denied",
];
const VIEW: &[&str] = &["effective"];

/// 2^53 − 1（api.md §1.5）。
pub const MAX_SAFE_INT: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryType {
    Int {
        min: u64,
        max: u64,
    },
    Bool,
    Str,
    Id(&'static str),
    Enum(&'static [&'static str]),
    /// 数组：逗号分隔或重复出现，每项属于枚举。
    EnumList(&'static [&'static str]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryParam {
    pub name: &'static str,
    pub ty: QueryType,
    /// `x-gantry-repeatable`：可重复出现（`GET /v1/events?project_id=…&project_id=…`）。
    pub repeatable: bool,
}

const fn q(name: &'static str, ty: QueryType) -> QueryParam {
    QueryParam {
        name,
        ty,
        repeatable: false,
    }
}

const LIMIT: QueryParam = q("limit", QueryType::Int { min: 1, max: 200 });
const CURSOR: QueryParam = q("cursor", QueryType::Str);
const ORDER_P: QueryParam = q("order", QueryType::Enum(ORDER));
const PAGE: [QueryParam; 3] = [LIMIT, CURSOR, ORDER_P];

/// 按 `operationId` 列出允许的查询参数；未列出的路由不接受任何查询参数。
pub fn params_for(operation_id: &str) -> &'static [QueryParam] {
    static LIST_DELIVERIES: [QueryParam; 7] = [
        LIMIT,
        CURSOR,
        ORDER_P,
        q("project_id", QueryType::Id("prj")),
        q("stage", QueryType::EnumList(STAGES)),
        q("stage_status", QueryType::Enum(STAGE_STATUSES)),
        q("active", QueryType::Bool),
    ];
    static LIST_WORKTREES: [QueryParam; 5] = [
        LIMIT,
        CURSOR,
        ORDER_P,
        q("project_id", QueryType::Id("prj")),
        q("delivery_id", QueryType::Id("dlv")),
    ];
    static LIST_WORKFLOWS: [QueryParam; 4] =
        [LIMIT, CURSOR, ORDER_P, q("stage", QueryType::Enum(STAGES))];
    static GET_WORKFLOW: [QueryParam; 1] = [q("include_superseded", QueryType::Bool)];
    static LIST_INBOX: [QueryParam; 6] = [
        LIMIT,
        CURSOR,
        ORDER_P,
        q("project_id", QueryType::Id("prj")),
        q("status", QueryType::Enum(INBOX_STATUS)),
        q("kind", QueryType::Enum(INBOX_KIND)),
    ];
    static LIST_APPROVALS: [QueryParam; 5] = [
        LIMIT,
        CURSOR,
        ORDER_P,
        q("status", QueryType::Enum(APPROVAL_STATUS)),
        q("delivery_id", QueryType::Id("dlv")),
    ];
    static GET_SETTINGS: [QueryParam; 1] = [q(
        "rev",
        QueryType::Int {
            min: 0,
            max: MAX_SAFE_INT,
        },
    )];
    static PUT_SETTINGS: [QueryParam; 1] = [q("dry_run", QueryType::Bool)];
    static GET_PROJECT_SETTINGS: [QueryParam; 2] = [
        q(
            "rev",
            QueryType::Int {
                min: 0,
                max: MAX_SAFE_INT,
            },
        ),
        q("view", QueryType::Enum(VIEW)),
    ];
    static STREAM_EVENTS: [QueryParam; 3] = [
        QueryParam {
            repeatable: true,
            ..q("project_id", QueryType::Id("prj"))
        },
        q("types", QueryType::Str),
        q(
            "after",
            QueryType::Int {
                min: 0,
                max: MAX_SAFE_INT,
            },
        ),
    ];
    static PAGE_ONLY: [QueryParam; 3] = PAGE;

    match operation_id {
        "listProjects" | "listConversations" | "listMessages" | "listNodeAttempts"
        | "listDevices" => &PAGE_ONLY,
        "listDeliveries" => &LIST_DELIVERIES,
        "listWorktrees" => &LIST_WORKTREES,
        "listDeliveryWorkflows" => &LIST_WORKFLOWS,
        "getWorkflow" => &GET_WORKFLOW,
        "listInbox" => &LIST_INBOX,
        "listApprovals" => &LIST_APPROVALS,
        "getSettings" => &GET_SETTINGS,
        "putSettings" | "putProjectSettings" => &PUT_SETTINGS,
        "getProjectSettings" => &GET_PROJECT_SETTINGS,
        "streamEvents" => &STREAM_EVENTS,
        _ => &[],
    }
}

/// 解析后的查询值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryValue {
    Int(u64),
    Bool(bool),
    Str(String),
    List(Vec<String>),
}

/// 已校验的查询参数。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    values: BTreeMap<&'static str, Vec<QueryValue>>,
}

impl Query {
    pub fn int(&self, name: &str) -> Option<u64> {
        match self.values.get(name)?.first()? {
            QueryValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.values.get(name)?.first()? {
            QueryValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn str(&self, name: &str) -> Option<&str> {
        match self.values.get(name)?.first()? {
            QueryValue::Str(v) => Some(v),
            _ => None,
        }
    }

    /// 可重复参数的全部取值。
    pub fn strs(&self, name: &str) -> Vec<&str> {
        self.values
            .get(name)
            .map(|vs| {
                vs.iter()
                    .filter_map(|v| match v {
                        QueryValue::Str(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn list(&self, name: &str) -> Option<&[String]> {
        match self.values.get(name)?.first()? {
            QueryValue::List(v) => Some(v),
            _ => None,
        }
    }
}

/// 校验失败的英文说明（只进 `detail`，不回显参数值）。
pub type QueryError = &'static str;

pub fn parse(raw: Option<&str>, allowed: &'static [QueryParam]) -> Result<Query, QueryError> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in raw.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode(k).ok_or("query string is not valid percent-encoding")?;
        let value = decode(v).ok_or("query string is not valid percent-encoding")?;
        grouped.entry(key).or_default().push(value);
    }
    let mut query = Query::default();
    for (key, raws) in grouped {
        let Some(param) = allowed.iter().find(|p| p.name == key) else {
            return Err("unknown query parameter");
        };
        let list = matches!(param.ty, QueryType::EnumList(_));
        if raws.len() > 1 && !param.repeatable && !list {
            return Err("query parameter must not repeat");
        }
        let values = if let QueryType::EnumList(allowed_values) = param.ty {
            let items: Vec<String> = raws
                .iter()
                .flat_map(|r| r.split(','))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if items.iter().any(|i| !allowed_values.contains(&i.as_str())) {
                return Err("query parameter has a value outside its enum");
            }
            vec![QueryValue::List(items)]
        } else {
            raws.iter()
                .map(|r| convert(r, param.ty))
                .collect::<Result<Vec<_>, _>>()?
        };
        query.values.insert(param.name, values);
    }
    Ok(query)
}

fn convert(raw: &str, ty: QueryType) -> Result<QueryValue, QueryError> {
    match ty {
        QueryType::Int { min, max } => {
            let valid = !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit())
                && (raw == "0" || !raw.starts_with('0'));
            let value: u64 = if valid { raw.parse().ok() } else { None }
                .ok_or("query parameter must be an integer")?;
            if value < min || value > max {
                return Err("query parameter is out of range");
            }
            Ok(QueryValue::Int(value))
        }
        QueryType::Bool => match raw {
            "true" => Ok(QueryValue::Bool(true)),
            "false" => Ok(QueryValue::Bool(false)),
            _ => Err("query parameter must be true or false"),
        },
        QueryType::Str => Ok(QueryValue::Str(raw.to_owned())),
        QueryType::Id(prefix) => {
            if is_prefixed_ulid(raw, prefix) {
                Ok(QueryValue::Str(raw.to_owned()))
            } else {
                Err("query parameter is not a valid id")
            }
        }
        QueryType::Enum(values) => {
            if values.contains(&raw) {
                Ok(QueryValue::Str(raw.to_owned()))
            } else {
                Err("query parameter has a value outside its enum")
            }
        }
        QueryType::EnumList(_) => unreachable!("handled by the caller"),
    }
}

/// `application/x-www-form-urlencoded` 解码（`+` 为空格）；非法转义或非 UTF-8 → `None`。
fn decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let hi = (hex[0] as char).to_digit(16)?;
                let lo = (hex[1] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_and_repeated_parameters() {
        assert_eq!(
            parse(Some("nope=1"), params_for("listProjects")),
            Err("unknown query parameter")
        );
        assert!(parse(Some("limit=1&limit=2"), params_for("listProjects")).is_err());
        let q = parse(
            Some("project_id=prj_01JB6Z3K7R9T2X5N8Q1V4W6YZM&project_id=prj_01JB6Z3K7R9T2X5N8Q1V4W6YZN"),
            params_for("streamEvents"),
        )
        .unwrap();
        assert_eq!(q.strs("project_id").len(), 2);
    }

    #[test]
    fn checks_types_and_ranges() {
        let page = params_for("listProjects");
        assert_eq!(
            parse(Some("limit=200"), page).unwrap().int("limit"),
            Some(200)
        );
        assert!(parse(Some("limit=201"), page).is_err());
        assert!(parse(Some("limit=0"), page).is_err());
        assert!(parse(Some("limit=01"), page).is_err());
        assert!(parse(Some("limit=-1"), page).is_err());
        assert!(parse(Some("order=up"), page).is_err());
        assert!(parse(Some("project_id=oops"), params_for("listDeliveries")).is_err());
        let q = parse(
            Some("stage=spec,plan&stage=merge"),
            params_for("listDeliveries"),
        )
        .unwrap();
        assert_eq!(q.list("stage").unwrap(), ["spec", "plan", "merge"]);
        assert!(parse(Some("stage=nope"), params_for("listDeliveries")).is_err());
        assert!(parse(Some("active=yes"), params_for("listDeliveries")).is_err());
        assert!(parse(Some("x=%zz"), page).is_err());
    }
}
