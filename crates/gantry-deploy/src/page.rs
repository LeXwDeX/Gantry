//! Manifest 提交页（deploy.md §7.5 第 3 步）：一个自动提交的 HTML 表单，把 Manifest
//! 以字段 `manifest` POST 到 GitHub。页面不含任何秘密；`state` 只在与回调配对时有意义。

use serde_json::Value;

/// HTML 属性值 / 文本转义。Manifest 里的字符串来自命令参数，按不可信处理。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub fn render(form_action: &str, manifest: &Value) -> String {
    let action = escape(form_action);
    let manifest = escape(&manifest.to_string());
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="referrer" content="no-referrer">
<title>创建 Gantry 的 GitHub App</title>
</head>
<body>
<p>正在跳转到 GitHub 创建 App。没有自动跳转时，点击下面的按钮。</p>
<form id="manifest-form" method="post" action="{action}">
<input type="hidden" name="manifest" value="{manifest}">
<button type="submit">在 GitHub 上创建 App</button>
</form>
<script>document.getElementById("manifest-form").submit();</script>
</body>
</html>
"#
    )
}
