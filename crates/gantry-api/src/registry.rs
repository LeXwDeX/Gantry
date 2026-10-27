//! 路由注册表（本文 §6.2）。
//!
//! 注册表是**数据**：一份静态表驱动鉴权链、限流分类、幂等策略、体积上限与手机
//! 子集，并与 `schemas/openapi.yaml` 双向对账（测试 `registry_matches_openapi`）。
//! 序号、路径、最低角色、手机列逐字取自 api.md §4.1；限流类取自 §3.7；`since`
//! 是本规格的上线里程碑建议。

use gantry_core::entities::DeviceRole;
use http::Method;

use crate::consts::{BODY_MAX, BODY_MAX_SETTINGS};

/// 路由的最低角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleReq {
    /// 配对端点：不经设备鉴权，凭配对令牌。
    PairToken,
    Fixed(DeviceRole),
    /// settings 写入：路由下限为 operator，实际角色按改动内容判定（§7.8）。
    SettingsDynamic,
}

impl RoleReq {
    /// 鉴权链步骤 4 使用的路由下限。
    pub fn floor(self) -> Option<DeviceRole> {
        match self {
            RoleReq::PairToken => None,
            RoleReq::Fixed(role) => Some(role),
            RoleReq::SettingsDynamic => Some(DeviceRole::Operator),
        }
    }
}

/// 手机档案可用性（api.md §2.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MobileAccess {
    No,
    Yes,
    /// #42：手机只能对部分上报项选择，由 `notify` 在处理时判定（P-79）。
    YesChooseLimited,
}

impl MobileAccess {
    pub fn allowed(self) -> bool {
        !matches!(self, MobileAccess::No)
    }
}

/// 限流类（api.md §3.7）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RateClass {
    Read,
    Write,
    Message,
    Sign,
    PairIp,
}

impl RateClass {
    pub fn as_str(self) -> &'static str {
        match self {
            RateClass::Read => "read",
            RateClass::Write => "write",
            RateClass::Message => "message",
            RateClass::Sign => "sign",
            RateClass::PairIp => "pair_ip",
        }
    }
}

/// `Idempotency-Key` 策略（api.md §3.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdemPolicy {
    Required,
    Optional,
    NotApplicable,
}

/// 路由上线里程碑。未到里程碑的路由与未注册路由一样返回 `404 not_found`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Milestone {
    M1,
    M2,
    M4,
}

/// 路径参数的形状；与 `openapi.yaml` 的参数 schema 一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// 带前缀的 ULID（data-model §2）。
    Id(&'static str),
    /// settings resource；未知值是 `404`（不是 `400`），由处理程序判定。
    SettingsResource,
}

impl ParamKind {
    fn for_name(name: &str) -> ParamKind {
        match name {
            "project_id" => ParamKind::Id("prj"),
            "delivery_id" => ParamKind::Id("dlv"),
            "conversation_id" => ParamKind::Id("cnv"),
            "worktree_id" => ParamKind::Id("wtr"),
            "workflow_id" => ParamKind::Id("wfl"),
            "node_id" => ParamKind::Id("nod"),
            "attempt_id" => ParamKind::Id("att"),
            "inbox_id" => ParamKind::Id("inb"),
            "approval_id" => ParamKind::Id("apr"),
            "device_id" => ParamKind::Id("dev"),
            "resource" => ParamKind::SettingsResource,
            other => panic!("registry: unknown path parameter {other}"),
        }
    }

    pub fn accepts(self, value: &str) -> bool {
        match self {
            ParamKind::Id(prefix) => is_prefixed_ulid(value, prefix),
            ParamKind::SettingsResource => !value.is_empty(),
        }
    }
}

/// `^<prefix>_[0-7][0-9A-HJKMNP-TV-Z]{25}$`
pub fn is_prefixed_ulid(value: &str, prefix: &str) -> bool {
    let Some(rest) = value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return false;
    };
    is_ulid(rest)
}

/// 无前缀 ULID（`request_id` 的线上形态）。
pub fn is_ulid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 26
        && matches!(bytes[0], b'0'..=b'7')
        && bytes[1..].iter().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteSpec {
    /// 契约 §4.1 序号。
    pub id: u8,
    pub method: &'static str,
    pub pattern: &'static str,
    pub operation_id: &'static str,
    pub min_role: RoleReq,
    pub mobile: MobileAccess,
    pub rate: RateClass,
    pub idem: IdemPolicy,
    pub body_limit: usize,
    pub since: Milestone,
}

impl RouteSpec {
    pub fn method(&self) -> Method {
        Method::from_bytes(self.method.as_bytes()).expect("registry methods are valid")
    }

    /// 只在配对监听器上路由。
    pub fn is_pairing(&self) -> bool {
        matches!(self.min_role, RoleReq::PairToken)
    }

    /// 是否携带 JSON 请求体（决定 `Content-Type` 与体积检查是否适用）。
    pub fn has_body(&self) -> bool {
        matches!(self.method, "POST" | "PUT" | "PATCH")
    }

    /// 请求体 schema 是 `EmptyRequest`：空正文按 `{}` 处理，也不要求 `Content-Type`。
    pub fn empty_body(&self) -> bool {
        matches!(
            self.operation_id,
            "revokeDevice" | "pauseDelivery" | "resumeDelivery" | "retryNode"
        )
    }
}

use DeviceRole::{Approver, Operator, Viewer};
use IdemPolicy::{NotApplicable as IdemNa, Optional as IdemOpt, Required as IdemReq};
use MobileAccess::{No as MobNo, Yes as MobYes, YesChooseLimited};
use RateClass::{Message as RMsg, PairIp as RPair, Read as RRead, Sign as RSign, Write as RWrite};

#[allow(clippy::too_many_arguments)]
const fn route(
    id: u8,
    method: &'static str,
    pattern: &'static str,
    operation_id: &'static str,
    min_role: RoleReq,
    mobile: MobileAccess,
    rate: RateClass,
    idem: IdemPolicy,
    since: Milestone,
) -> RouteSpec {
    RouteSpec {
        id,
        method,
        pattern,
        operation_id,
        min_role,
        mobile,
        rate,
        idem,
        body_limit: BODY_MAX,
        since,
    }
}

const fn settings_put(
    id: u8,
    method: &'static str,
    pattern: &'static str,
    op: &'static str,
    since: Milestone,
) -> RouteSpec {
    RouteSpec {
        body_limit: BODY_MAX_SETTINGS,
        ..route(
            id,
            method,
            pattern,
            op,
            RoleReq::SettingsDynamic,
            MobNo,
            RWrite,
            IdemNa,
            since,
        )
    }
}

const V: RoleReq = RoleReq::Fixed(Viewer);
const O: RoleReq = RoleReq::Fixed(Operator);
const A: RoleReq = RoleReq::Fixed(Approver);
const M1: Milestone = Milestone::M1;
const M2: Milestone = Milestone::M2;
const M4: Milestone = Milestone::M4;

/// 契约 §4.1 的 48 条路由（mTLS 47 条 + 配对 1 条），按序号排列。
pub static ROUTES: [RouteSpec; 48] = [
    route(
        1,
        "POST",
        "/v1/pair",
        "pairDevice",
        RoleReq::PairToken,
        MobNo,
        RPair,
        IdemNa,
        M1,
    ),
    route(2, "GET", "/v1/me", "getMe", V, MobYes, RRead, IdemNa, M1),
    route(
        3,
        "GET",
        "/v1/projects",
        "listProjects",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        4,
        "GET",
        "/v1/projects/{project_id}",
        "getProject",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        5,
        "GET",
        "/v1/deliveries",
        "listDeliveries",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        6,
        "GET",
        "/v1/deliveries/{delivery_id}",
        "getDelivery",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        7,
        "PATCH",
        "/v1/deliveries/{delivery_id}",
        "patchDelivery",
        O,
        MobNo,
        RWrite,
        IdemNa,
        M2,
    ),
    route(
        8,
        "POST",
        "/v1/deliveries/{delivery_id}/abandon",
        "abandonDelivery",
        O,
        MobNo,
        RWrite,
        IdemOpt,
        M1,
    ),
    route(
        9,
        "GET",
        "/v1/deliveries/{delivery_id}/budget",
        "getDeliveryBudget",
        V,
        MobNo,
        RRead,
        IdemNa,
        M2,
    ),
    route(
        10,
        "GET",
        "/v1/projects/{project_id}/conversations",
        "listConversations",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        11,
        "GET",
        "/v1/conversations/{conversation_id}/messages",
        "listMessages",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        12,
        "POST",
        "/v1/conversations/{conversation_id}/messages",
        "postMessage",
        O,
        MobYes,
        RMsg,
        IdemReq,
        M1,
    ),
    route(
        13,
        "GET",
        "/v1/worktrees",
        "listWorktrees",
        V,
        MobNo,
        RRead,
        IdemNa,
        M2,
    ),
    route(
        14,
        "GET",
        "/v1/worktrees/{worktree_id}",
        "getWorktree",
        V,
        MobNo,
        RRead,
        IdemNa,
        M2,
    ),
    route(
        15,
        "PUT",
        "/v1/worktrees/{worktree_id}/lock",
        "acquireWorktreeLock",
        O,
        MobNo,
        RWrite,
        IdemNa,
        M2,
    ),
    route(
        16,
        "DELETE",
        "/v1/worktrees/{worktree_id}/lock",
        "releaseWorktreeLock",
        O,
        MobNo,
        RWrite,
        IdemNa,
        M2,
    ),
    route(
        17,
        "GET",
        "/v1/deliveries/{delivery_id}/workflows",
        "listDeliveryWorkflows",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        18,
        "GET",
        "/v1/workflows/{workflow_id}",
        "getWorkflow",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        19,
        "GET",
        "/v1/nodes/{node_id}/attempts",
        "listNodeAttempts",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        20,
        "GET",
        "/v1/attempts/{attempt_id}",
        "getAttempt",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        21,
        "GET",
        "/v1/inbox",
        "listInbox",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        22,
        "GET",
        "/v1/inbox/{inbox_id}",
        "getInboxItem",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        23,
        "POST",
        "/v1/inbox/{inbox_id}/dismiss",
        "dismissInboxItem",
        O,
        MobNo,
        RWrite,
        IdemOpt,
        M2,
    ),
    route(
        24,
        "GET",
        "/v1/approvals",
        "listApprovals",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        25,
        "GET",
        "/v1/approvals/{approval_id}",
        "getApproval",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        26,
        "POST",
        "/v1/approvals",
        "createApproval",
        O,
        MobNo,
        RSign,
        IdemReq,
        M1,
    ),
    route(
        27,
        "GET",
        "/v1/approvals/{approval_id}/challenge",
        "getApprovalChallenge",
        A,
        MobNo,
        RSign,
        IdemNa,
        M1,
    ),
    route(
        28,
        "POST",
        "/v1/approvals/{approval_id}/grant",
        "grantApproval",
        A,
        MobNo,
        RWrite,
        IdemReq,
        M1,
    ),
    route(
        29,
        "POST",
        "/v1/approvals/{approval_id}/deny",
        "denyApproval",
        A,
        MobNo,
        RWrite,
        IdemReq,
        M1,
    ),
    route(
        30,
        "GET",
        "/v1/settings/{resource}",
        "getSettings",
        O,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    settings_put(31, "PUT", "/v1/settings/{resource}", "putSettings", M1),
    route(
        32,
        "GET",
        "/v1/projects/{project_id}/settings/{resource}",
        "getProjectSettings",
        O,
        MobNo,
        RRead,
        IdemNa,
        M2,
    ),
    settings_put(
        33,
        "PUT",
        "/v1/projects/{project_id}/settings/{resource}",
        "putProjectSettings",
        M2,
    ),
    settings_put(
        34,
        "DELETE",
        "/v1/projects/{project_id}/settings/{resource}",
        "deleteProjectSettings",
        M2,
    ),
    route(
        35,
        "GET",
        "/v1/settings/catalog",
        "getSettingsCatalog",
        O,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        36,
        "GET",
        "/v1/settings/status",
        "getSettingsStatus",
        O,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        37,
        "GET",
        "/v1/devices",
        "listDevices",
        V,
        MobNo,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        38,
        "PATCH",
        "/v1/devices/{device_id}",
        "renameDevice",
        O,
        MobNo,
        RWrite,
        IdemNa,
        M1,
    ),
    route(
        39,
        "PUT",
        "/v1/devices/{device_id}/role",
        "setDeviceRole",
        A,
        MobNo,
        RWrite,
        IdemNa,
        M1,
    ),
    route(
        40,
        "POST",
        "/v1/devices/{device_id}/revoke",
        "revokeDevice",
        A,
        MobNo,
        RWrite,
        IdemOpt,
        M1,
    ),
    route(
        41,
        "GET",
        "/v1/events",
        "streamEvents",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        42,
        "POST",
        "/v1/inbox/{inbox_id}/choose",
        "chooseInboxOption",
        O,
        YesChooseLimited,
        RWrite,
        IdemReq,
        M2,
    ),
    route(
        43,
        "POST",
        "/v1/deliveries/{delivery_id}/pause",
        "pauseDelivery",
        O,
        MobNo,
        RWrite,
        IdemReq,
        M1,
    ),
    route(
        44,
        "POST",
        "/v1/deliveries/{delivery_id}/resume",
        "resumeDelivery",
        O,
        MobNo,
        RWrite,
        IdemReq,
        M1,
    ),
    route(
        45,
        "POST",
        "/v1/nodes/{node_id}/retry",
        "retryNode",
        O,
        MobNo,
        RWrite,
        IdemReq,
        M1,
    ),
    route(
        46,
        "GET",
        "/v1/inbox/summary",
        "getInboxSummary",
        V,
        MobYes,
        RRead,
        IdemNa,
        M1,
    ),
    route(
        47,
        "POST",
        "/v1/devices/self/renew",
        "renewDeviceCertificate",
        V,
        MobYes,
        RWrite,
        IdemOpt,
        M1,
    ),
    route(
        48,
        "PUT",
        "/v1/devices/self/push-token",
        "putPushToken",
        O,
        MobYes,
        RWrite,
        IdemNa,
        M4,
    ),
];

/// 路由匹配结果。
#[derive(Debug)]
pub enum RouteMatch {
    /// 没有任何模式匹配该路径。
    NoPath,
    /// 路径匹配但方法不对；`allow` 已排序。
    MethodNotAllowed { allow: Vec<&'static str> },
    Found {
        route: &'static RouteSpec,
        params: Vec<(&'static str, String)>,
    },
}

/// 一个监听器上的路由表。
pub struct Registry {
    /// 按模式分组，已按"静态段数降序、模式长度降序"排好（字面路径优先）。
    groups: Vec<Group>,
}

struct Group {
    pattern: &'static str,
    segments: Vec<Segment>,
    routes: Vec<&'static RouteSpec>,
}

enum Segment {
    Literal(&'static str),
    Param(&'static str),
}

impl Registry {
    /// mTLS 监听器：除 `POST /v1/pair` 外、已到 `enabled` 里程碑的路由。
    pub fn api(enabled: Milestone) -> Registry {
        Registry::build(
            ROUTES
                .iter()
                .filter(|r| !r.is_pairing() && r.since <= enabled),
        )
    }

    /// 配对监听器：只有 `POST /v1/pair`。
    pub fn pairing() -> Registry {
        Registry::build(ROUTES.iter().filter(|r| r.is_pairing()))
    }

    fn build<'a>(routes: impl Iterator<Item = &'static RouteSpec> + 'a) -> Registry {
        let mut groups: Vec<Group> = Vec::new();
        for route in routes {
            if let Some(group) = groups.iter_mut().find(|g| g.pattern == route.pattern) {
                group.routes.push(route);
                continue;
            }
            let segments = route
                .pattern
                .split('/')
                .filter(|s| !s.is_empty())
                .map(
                    |s| match s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                        Some(name) => Segment::Param(name),
                        None => Segment::Literal(s),
                    },
                )
                .collect();
            groups.push(Group {
                pattern: route.pattern,
                segments,
                routes: vec![route],
            });
        }
        groups.sort_by(|a, b| {
            b.static_rank()
                .cmp(&a.static_rank())
                .then(b.pattern.len().cmp(&a.pattern.len()))
        });
        Registry { groups }
    }

    /// 步骤 3：路径 + 方法。路径上最多去掉一个结尾 `/`。
    pub fn find(&self, method: &Method, path: &str) -> RouteMatch {
        let trimmed = path
            .strip_suffix('/')
            .filter(|p| !p.is_empty())
            .unwrap_or(path);
        let Some(rest) = trimmed.strip_prefix('/') else {
            return RouteMatch::NoPath;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        for group in &self.groups {
            let Some(params) = group.matches(&parts) else {
                continue;
            };
            return match group.routes.iter().find(|r| r.method() == *method) {
                Some(route) => RouteMatch::Found { route, params },
                None => {
                    let mut allow: Vec<&'static str> =
                        group.routes.iter().map(|r| r.method).collect();
                    allow.sort_unstable();
                    RouteMatch::MethodNotAllowed { allow }
                }
            };
        }
        RouteMatch::NoPath
    }
}

impl Group {
    fn static_rank(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| matches!(s, Segment::Literal(_)))
            .count()
    }

    fn matches(&self, parts: &[&str]) -> Option<Vec<(&'static str, String)>> {
        if parts.len() != self.segments.len() {
            return None;
        }
        let mut params = Vec::new();
        for (segment, part) in self.segments.iter().zip(parts) {
            match segment {
                Segment::Literal(lit) => {
                    if lit != part {
                        return None;
                    }
                }
                Segment::Param(name) => {
                    if part.is_empty() {
                        return None;
                    }
                    params.push((*name, (*part).to_owned()));
                }
            }
        }
        Some(params)
    }
}

/// 步骤 9 的路径参数检查：格式不符 → `400 invalid_request`。
pub fn check_path_params(params: &[(&'static str, String)]) -> Result<(), &'static str> {
    for (name, value) in params {
        if !ParamKind::for_name(name).accepts(value) {
            return Err(name);
        }
    }
    Ok(())
}
