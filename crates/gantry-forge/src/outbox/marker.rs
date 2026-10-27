//! 对象标记（FA §2.4）。标记只是索引，不是凭证：认定"自己的对象"必须叠加作者校验。

use gantry_core::entities::ForgeActionKind;
use gantry_core::ids::{ActionId, DeliveryId};

/// `<!-- gantry:v1 act=act_… dlv=dlv_… kind=… -->`
pub(crate) fn render(act: &ActionId, dlv: &DeliveryId, kind: ForgeActionKind) -> String {
    format!(
        "<!-- gantry:v1 act={act} dlv={dlv} kind={} -->",
        kind.as_str()
    )
}

/// 正文里是否带本动作的标记（只看 `act`；`dlv` 一致性由调用方比对 Outbox 行）。
pub(crate) fn carries_act(body: &str, act: &ActionId) -> bool {
    let needle = format!("act={act} ");
    body.lines()
        .any(|line| line.trim_start().starts_with("<!-- gantry:v1 ") && line.contains(&needle))
}

/// 去掉正文里的标记行，用于逐字段比较（FA §3.2 `update_pr`："正文比较时忽略标记"）。
pub(crate) fn without_markers(body: &str) -> String {
    body.lines()
        .filter(|line| !line.trim_start().starts_with("<!-- gantry:v1 "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

/// 正文里已有的 forge 标记行（`update_pr` 改正文时保留创建时的标记，使 probe 仍能认出对象）。
pub(crate) fn existing_marker(body: &str) -> Option<&str> {
    body.lines()
        .map(str::trim)
        .find(|line| line.starts_with("<!-- gantry:v1 "))
}
