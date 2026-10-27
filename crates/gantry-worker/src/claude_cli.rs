//! Claude Code CLI 的调用构造（worker.md §6.4；worker-protocol §10.2）。
//!
//! `build_invocation` 是纯函数：JobSpec → argv、追加环境变量、写入 `HOME` 的用户级配置。
//! 仓库内的 `.claude/`、hooks、`.mcp.json`、插件属仓库内容（I5），不得成为权限来源（A2）：
//! 只加载 `--setting-sources user`（即本模块写进 `HOME` 的配置），MCP 只取空的
//! `--mcp-config`。权限规则是纵深防御，不承担安全保证，外层沙盒才是边界。
//!
//! 标志写法以本机核对过的 CLI（2.1.x）为准；镜像钉死的版本与冒烟测试见 OQ-W5。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gantry_core::entities::Exec;
use gantry_core::level::Effort;
use gantry_core::worker::JobSpec;
use serde_json::json;

/// 沙盒内的 CLI 程序名。
pub const PROGRAM: &str = "claude";

/// 无人值守：未预先批准的工具一律拒绝，不提示（OQ-W4 的暂用值）。
/// **不使用** `bypassPermissions` 或跳过权限检查类标志。
pub const PERMISSION_MODE: &str = "dontAsk";

/// 只加载用户级配置；项目级（`.claude/settings.json`）与本地级（`settings.local.json`）不加载。
pub const SETTING_SOURCES: &str = "user";

/// 空 MCP 配置，配合 `--strict-mcp-config` 忽略仓库里的 `.mcp.json`。
pub const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;

/// `HOME` 下的用户级配置文件（相对 `HOME`）。
pub const USER_SETTINGS: &str = ".claude/settings.json";

/// 读 / 搜索类工具，所有 role 都放行。
const READ_TOOLS: [&str; 4] = ["Read", "Glob", "Grep", "TodoWrite"];
/// 编辑类工具。
const EDIT_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];
/// `sandbox_ro` 角色可用的只读命令（worker.md §6.4 权限档）。
const READ_ONLY_BASH: [&str; 5] = [
    "Bash(git diff:*)",
    "Bash(git log:*)",
    "Bash(git show:*)",
    "Bash(grep:*)",
    "Bash(rg:*)",
];
/// 不论 `paths.deny` 如何，`.git/**` 一律不可编辑。
const ALWAYS_DENY_PATHS: [&str; 1] = [".git/**"];

/// `reasoning_effort` → `--effort`。自适应推理模型忽略 `MAX_THINKING_TOKENS` 的正值，
/// 强度只能经 `--effort` 生效；`none` 没有对应档位，取最低档并把思考预算置 0。
pub fn cli_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::None | Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Max => "max",
    }
}

/// `reasoning_effort` → 思考预算（OQ-W3：适配层内的编译期常量，不进设置），
/// 对非自适应推理模型生效。取值为 **[建议值，待调优]**；`none` 关闭思考。
pub fn thinking_tokens(effort: Effort) -> u32 {
    match effort {
        Effort::None => 0,
        Effort::Low => 4_096,
        Effort::Medium => 16_384,
        Effort::High => 32_768,
        Effort::Max => 63_999,
    }
}

/// 一次 CLI 调用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: &'static str,
    pub args: Vec<String>,
    /// 在容器环境（`WorkerAdapter::env` + 令牌变量）之上追加的变量。
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// 写入 `HOME` 的用户级配置：相对 `HOME` 的路径 → 内容。
    pub home_files: BTreeMap<PathBuf, Vec<u8>>,
}

/// 预先批准与明确拒绝的工具规则。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionProfile {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvocationError {
    /// `paths.deny` 的模式不能安全地写成 `Edit(<pattern>)` 规则。
    #[error("constraints.paths.deny pattern cannot be expressed as a permission rule")]
    UnsupportedDenyPattern,
}

/// 按 role 与 `constraints.tools` / `paths.deny` 计算权限档（worker.md §6.4、§12.G）。
pub fn permission_profile(job: &JobSpec) -> Result<PermissionProfile, InvocationError> {
    let read_only = job.node.exec == Exec::SandboxRo;
    let tools = &job.constraints.tools;
    let mut profile = PermissionProfile::default();
    profile.allow.extend(READ_TOOLS.map(String::from));

    if tools.edit && !read_only {
        profile.allow.extend(EDIT_TOOLS.map(String::from));
        for pattern in ALWAYS_DENY_PATHS
            .iter()
            .copied()
            .chain(job.constraints.paths.deny.iter().map(String::as_str))
        {
            for path in deny_rule_paths(pattern)? {
                for tool in EDIT_TOOLS {
                    let rule = format!("{tool}({path})");
                    if !profile.deny.contains(&rule) {
                        profile.deny.push(rule);
                    }
                }
            }
        }
    } else {
        profile.deny.extend(EDIT_TOOLS.map(String::from));
    }

    match (tools.shell, read_only) {
        (false, _) => profile.deny.push("Bash".into()),
        (true, true) => profile.allow.extend(READ_ONLY_BASH.map(String::from)),
        (true, false) => profile.allow.push("Bash".into()),
    }
    Ok(profile)
}

/// 把 policy 的路径规则（gitignore 子集，policy.md §7.2）翻译成相对 cwd（仓库根
/// `/gantry/work`）的 CLI 规则路径，保持同样的命中范围：
///
/// - 含 `/`（开头的 `/` 可省略）：相对仓库根锚定 → `./<body>`；
/// - 不含 `/`：命中任一层级的组件 → `./**/<body>`；
/// - 命中目录即命中其下全部文件 → 另加 `<path>/**`；末尾 `/` 只匹配目录 → 只给 `<path>/**`。
///
/// `~`、`!`、`\`、括号、逗号与控制符会改变 CLI 规则或 policy 语义，一律拒绝而不是转义。
fn deny_rule_paths(pattern: &str) -> Result<Vec<String>, InvocationError> {
    if pattern.starts_with(['~', '!'])
        || pattern
            .chars()
            .any(|c| c.is_control() || matches!(c, '(' | ')' | ',' | '\\'))
    {
        return Err(InvocationError::UnsupportedDenyPattern);
    }
    let (body, dir_only) = match pattern.strip_suffix('/') {
        Some(body) => (body, true),
        None => (pattern, false),
    };
    let anchored = body.contains('/');
    let body = body
        .strip_prefix("./")
        .or_else(|| body.strip_prefix('/'))
        .unwrap_or(body);
    if body.is_empty() || body.split('/').any(str::is_empty) {
        return Err(InvocationError::UnsupportedDenyPattern);
    }
    let base = if anchored {
        format!("./{body}")
    } else {
        format!("./**/{body}")
    };
    let mut paths = Vec::with_capacity(2);
    if !dir_only {
        paths.push(base.clone());
    }
    if !body.ends_with("**") {
        paths.push(format!("{base}/**"));
    }
    Ok(paths)
}

/// 构造调用。`system_prompt` 是固定前言 + `task.instructions`（A3），经
/// `--append-system-prompt` 传入；用户 prompt 由调用方经 stdin 传入。
pub fn build_invocation(
    job: &JobSpec,
    work: &Path,
    system_prompt: &str,
) -> Result<Invocation, InvocationError> {
    let profile = permission_profile(job)?;
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--model".into(),
        job.execution.model.clone(),
        "--max-turns".into(),
        job.constraints.limits.max_turns.to_string(),
        "--effort".into(),
        cli_effort(job.execution.reasoning_effort).into(),
        "--permission-mode".into(),
        PERMISSION_MODE.into(),
        "--setting-sources".into(),
        SETTING_SOURCES.into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        EMPTY_MCP_CONFIG.into(),
        "--append-system-prompt".into(),
        system_prompt.into(),
    ];
    // 两个列表参数都是可变长的，放在最后，各自以下一个 `--` 标志结束。
    args.push("--allowedTools".into());
    args.extend(profile.allow.iter().cloned());
    if !profile.deny.is_empty() {
        args.push("--disallowedTools".into());
        args.extend(profile.deny.iter().cloned());
    }

    let mut env = BTreeMap::new();
    env.insert(
        "MAX_THINKING_TOKENS".to_owned(),
        thinking_tokens(job.execution.reasoning_effort).to_string(),
    );

    let settings = json!({
        "permissions": {
            "allow": profile.allow,
            "deny": profile.deny,
            "defaultMode": PERMISSION_MODE,
            "disableBypassPermissionsMode": "disable",
        },
        "disableAllHooks": true,
        "enableAllProjectMcpServers": false,
    });
    let mut settings_bytes = serde_json::to_vec_pretty(&settings).expect("settings serialize");
    settings_bytes.push(b'\n');
    let home_files = BTreeMap::from([(PathBuf::from(USER_SETTINGS), settings_bytes)]);

    Ok(Invocation {
        program: PROGRAM,
        args,
        env,
        cwd: work.to_path_buf(),
        home_files,
    })
}

/// 把用户级配置写进 `HOME`（tmpfs）。只写 `home_files` 里的相对路径。
pub fn write_home(home: &Path, invocation: &Invocation) -> io::Result<()> {
    for (rel, bytes) in &invocation.home_files {
        let path = home.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
    }
    Ok(())
}
