//! 真实 `claude` CLI 的对照测试（worker.md §12.G 末条的本机部分；不进 PR 流水线）。
//!
//! 运行：`cargo test -p gantry-worker --test claude_cli_real -- --ignored`，需要 PATH 里有 `claude`。
//! 不调用真实模型：模型 base URL 指向本地一个只回 401 的端点，CLI 在会话启动后即失败退出。
//! `HOME` 是临时目录，不读本机用户配置或凭据。
//!
//! 同一个恶意工作区跑两次：反向对照（加载项目配置、不限 MCP）必须触发标记文件，
//! 证明测试本身有效；适配层的调用必须一个标记都不触发。

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gantry_core::worker::JobSpec;
use gantry_worker::claude_cli::{build_invocation, write_home, Invocation};
use serde_json::json;

const JOB_SPEC: &str = include_str!("../../gantry-core/fixtures/protocol/worker/job_spec.json");

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn root() -> Root {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gantry-worker-real-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    Root(path)
}

/// 只回 401 的模型端点：CLI 不重试认证错误，很快退出。
fn unauthorized_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.read(&mut buf);
            let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}")
}

struct Markers {
    hook: PathBuf,
    mcp: PathBuf,
}

fn hostile_workspace(work: &Path, markers: &Markers) {
    let claude = work.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    let hook = format!("touch '{}'", markers.hook.display());
    let settings = json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": hook}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": hook}]}]
        },
        "enableAllProjectMcpServers": true
    });
    fs::write(
        claude.join("settings.json"),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let mcp = json!({"mcpServers": {"canary": {
        "command": "/bin/sh",
        "args": ["-c", format!("touch '{}'; sleep 30", markers.mcp.display())]
    }}});
    fs::write(work.join(".mcp.json"), serde_json::to_vec(&mcp).unwrap()).unwrap();
}

fn run(inv: &Invocation, home: &Path, base_url: &str) {
    let mut env: BTreeMap<String, String> = inv.env.clone();
    env.insert("HOME".into(), home.display().to_string());
    env.insert("PATH".into(), std::env::var("PATH").unwrap());
    env.insert("ANTHROPIC_API_KEY".into(), "gantry-egress-managed".into());
    env.insert("ANTHROPIC_BASE_URL".into(), base_url.into());
    env.insert(
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
        "1".into(),
    );
    env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
    env.insert("DISABLE_TELEMETRY".into(), "1".into());
    let mut child = Command::new(inv.program)
        .args(&inv.args)
        .current_dir(&inv.cwd)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("claude CLI on PATH");
    child.stdin.take().unwrap().write_all(b"say ok").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.wait();
}

/// 反向对照：把隔离标志换成加载项目配置、不限 MCP。
fn permissive(mut inv: Invocation) -> Invocation {
    let mut args = Vec::new();
    let mut iter = inv.args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--setting-sources" => {
                iter.next();
                args.push(arg);
                args.push("user,project,local".into());
            }
            "--strict-mcp-config" => {}
            "--mcp-config" => {
                iter.next();
            }
            _ => args.push(arg),
        }
    }
    inv.args = args;
    // 对照组不写入关闭 hooks 的用户级配置。
    inv.home_files.clear();
    inv
}

fn case(adapter_flags: bool) -> (bool, bool) {
    let root = root();
    let work = root.0.join("work");
    let home = root.0.join("home");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&home).unwrap();
    let markers = Markers {
        hook: root.0.join("hook-ran"),
        mcp: root.0.join("mcp-started"),
    };
    hostile_workspace(&work, &markers);
    let job: JobSpec = serde_json::from_str(JOB_SPEC).unwrap();
    let mut inv = build_invocation(&job, &work, "gantry-preamble/1 test").unwrap();
    if !adapter_flags {
        inv = permissive(inv);
    }
    write_home(&home, &inv).unwrap();
    run(&inv, &home, &unauthorized_endpoint());
    // MCP 服务器与 hook 可能在 CLI 退出前后才落盘，留一点余量。
    thread::sleep(Duration::from_millis(500));
    (markers.hook.exists(), markers.mcp.exists())
}

#[test]
#[ignore = "需要本机 claude CLI；B 级证据，不进 PR 流水线"]
fn repo_claude_config_and_mcp_json_are_not_loaded_by_real_cli() {
    let (hook, mcp) = case(false);
    assert!(
        hook && mcp,
        "反向对照没有触发标记（hook={hook}, mcp={mcp}），测试对本机 CLI 版本无效"
    );
    let (hook, mcp) = case(true);
    assert!(!hook, "仓库 .claude/settings.json 的 hook 被执行了");
    assert!(!mcp, "仓库 .mcp.json 的 MCP 服务器被启动了");
}
