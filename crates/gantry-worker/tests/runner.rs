//! 执行者 `gantry-worker run` 的端到端测试（worker.md §12 A / B / D / E / F / K 中 A 级可做的部分）。
//!
//! 直接运行编译出的二进制：`GANTRY_JOB` / `GANTRY_OUT` / `GANTRY_WORK` 指向临时目录，`PATH`
//! 里只有 `stub-claude`（一个 sh 脚本，记录 argv / 环境 / stdin，按场景回放 stream-json）。
//! 场景里的行取自真实 CLI 的录制（`fixtures/claude-stream-json/`）。
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{adapter, assert_valid, TempDir, JOB_SPEC};
use gantry_core::canon::{sha256, GitOid};
use gantry_core::ports::worker::WorkerAdapter;
use gantry_core::worker::{JobSpec, ProgressEvent, ResultErrorKind, ResultJson, ResultStatus};
use serde_json::{json, Value};

const SUCCESS: &str = include_str!("../fixtures/claude-stream-json/success.jsonl");
const MAX_TURNS: &str = include_str!("../fixtures/claude-stream-json/max_turns.jsonl");
const SCHEMA: &[u8] = include_bytes!("../../../schemas/worker/output.build.schema.json");
const WORKER: &str = env!("CARGO_BIN_EXE_gantry-worker");

const STUB: &str = r#"#!/bin/sh
d="$STUB_DIR"
n=$(( $(cat "$d/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$d/count"
for a in "$@"; do printf '%s\n' "$a"; done > "$d/argv.$n"
env > "$d/env.$n"
cat > "$d/stdin.$n"
[ -f "$d/before.$n.sh" ] && . "$d/before.$n.sh"
[ -f "$d/stream.$n.jsonl" ] && cat "$d/stream.$n.jsonl"
[ -f "$d/after.$n.sh" ] && . "$d/after.$n.sh"
exit 0
"#;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Sandbox {
    _root: TempDir,
    root: PathBuf,
    job_path: PathBuf,
    inputs: PathBuf,
    work: PathBuf,
    out: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    stub: PathBuf,
    job: JobSpec,
    /// 额外的容器环境（模拟 sandbox 按 token_env 填入的令牌变量）。
    extra_env: BTreeMap<String, String>,
}

struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new("runner");
        let root = dir.path().to_path_buf();
        let [inputs, work, out, home, tmp, stub] =
            ["inputs", "work", "out", "home", "tmp", "stub"].map(|d| root.join(d));
        for d in [&inputs, &work, &out, &home, &tmp, &stub] {
            fs::create_dir_all(d).unwrap();
        }
        fs::create_dir_all(inputs.join("materials")).unwrap();
        git(&work, &["init", "-q"]);
        fs::write(work.join("a.txt"), "one\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "base"]);
        // 与 repo 准备的工作区一致：HEAD 游离在 base_sha。
        let head = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-q", "--detach", &head]);
        let mut job: JobSpec = serde_json::from_str(JOB_SPEC).unwrap();
        job.workspace.base_sha = head.parse::<GitOid>().unwrap();
        job.output.structured_schema.sha256 = sha256(SCHEMA);
        let stub_bin = stub.join("claude");
        fs::write(&stub_bin, STUB).unwrap();
        fs::set_permissions(&stub_bin, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            job_path: root.join("job.json"),
            _root: dir,
            root,
            inputs,
            work,
            out,
            home,
            tmp,
            stub,
            job,
            extra_env: BTreeMap::new(),
        }
    }

    /// 按 sandbox 的 staging 约定写 job.json、inputs、MANIFEST，最后 READY。
    fn stage(&self, ready: bool) {
        let job_bytes = serde_json::to_vec(&self.job).unwrap();
        fs::write(&self.job_path, &job_bytes).unwrap();
        fs::write(self.inputs.join("output.schema.json"), SCHEMA).unwrap();
        let material = format!(
            "<<<GANTRY-UNTRUSTED-BEGIN nonce={n} id=mat-001 kind=issue_body source=acme/widgets#5 truncated=false>>>\nPagination skips the last item.\n<<<GANTRY-UNTRUSTED-END nonce={n} id=mat-001>>>\n",
            n = self.job.isolation.nonce
        );
        fs::write(self.inputs.join("materials/mat-001.txt"), &material).unwrap();
        let manifest = json!({
            "job_sha256": sha256(&job_bytes),
            "files": [
                {"path": "/gantry/inputs/materials/mat-001.txt", "sha256": sha256(material.as_bytes()), "bytes": material.len()},
                {"path": "/gantry/inputs/output.schema.json", "sha256": sha256(SCHEMA), "bytes": SCHEMA.len()},
            ]
        });
        fs::write(
            self.inputs.join("MANIFEST.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        if ready {
            fs::write(self.inputs.join("READY"), b"").unwrap();
        }
    }

    fn script(&self, name: &str, body: &str) {
        fs::write(self.stub.join(name), body).unwrap();
    }

    fn stream(&self, n: usize, lines: &[String]) {
        fs::write(
            self.stub.join(format!("stream.{n}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(WORKER);
        cmd.arg("run").env_clear();
        // 容器环境：WorkerAdapter::env() 的输出（测试替身改写三条路径）+ 令牌变量。
        for (k, v) in adapter().env(&self.job) {
            cmd.env(k, v);
        }
        cmd.env("GANTRY_JOB", &self.job_path)
            .env("GANTRY_OUT", &self.out)
            .env("GANTRY_WORK", &self.work)
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .env("PATH", format!("{}:/usr/bin:/bin", self.stub.display()))
            .env("STUB_DIR", &self.stub)
            .envs(&self.extra_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn run(&self) -> Outcome {
        let child = self.command().spawn().unwrap();
        finish(child, Duration::from_secs(60))
    }

    fn result(&self) -> Option<(Value, ResultJson)> {
        let bytes = fs::read(self.out.join("result.json")).ok()?;
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_valid("result.schema.json", &value);
        Some((value.clone(), serde_json::from_value(value).unwrap()))
    }

    fn out_files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.out)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn stub_file(&self, name: &str) -> String {
        fs::read_to_string(self.stub.join(name)).unwrap_or_default()
    }

    fn invocations(&self) -> usize {
        self.stub_file("count").trim().parse().unwrap_or(0)
    }
}

fn finish(mut child: Child, limit: Duration) -> Outcome {
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).unwrap();
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).unwrap();
        s
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "gantry-worker did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    Outcome {
        code: status.code().unwrap_or(-1),
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

/// stdout 只有协议事件：每行合法、通过 schema、seq 严格递增、hello 第一行、finish 最后一行。
fn protocol_events(stdout: &str) -> Vec<Value> {
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| {
            let value: Value =
                serde_json::from_str(line).unwrap_or_else(|_| panic!("non-JSON line: {line}"));
            assert_valid("progress-event.schema.json", &value);
            serde_json::from_value::<ProgressEvent>(value.clone()).unwrap();
            value
        })
        .collect();
    for (i, v) in lines.iter().enumerate() {
        assert_eq!(v["seq"], (i + 1) as u64);
    }
    if let Some(first) = lines.first() {
        assert_eq!(first["type"], "hello");
        assert_eq!(lines.last().unwrap()["type"], "finish");
    }
    lines
}

fn recorded(text: &str) -> Vec<Value> {
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// 录制的成功流：init、工具调用、最终回复替换为 `final_text`。
fn success_stream(final_text: &str) -> Vec<String> {
    let mut lines = recorded(SUCCESS);
    let last = lines.last_mut().unwrap();
    last["result"] = json!(final_text);
    lines.into_iter().map(|v| v.to_string()).collect()
}

fn envelope() -> String {
    json!({
        "structured_output": {"kind": "build", "changes_summary": "Fixed the off-by-one."},
        "report_markdown": "# Report\n\nDone.",
        "proposals": [
            {"id": "p1", "kind": "open_pr", "target": null,
             "params": {"title": "fix", "body": "b", "draft": false}, "rationale": "r"},
            {"id": "p2", "kind": "merge_pr", "target": null, "params": {}, "rationale": "r"}
        ],
        "suspected_injection": [{"material_id": "mat-001", "note": "asked to curl"}]
    })
    .to_string()
}

fn warn_codes(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "warn")
        .map(|e| e["code"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn golden_run_produces_the_four_artifacts() {
    let sbx = Sandbox::new();
    sbx.stage(true);
    sbx.script("before.1.sh", "printf 'one\\ntwo\\n' > a.txt\n");
    sbx.stream(1, &success_stream(&envelope()));
    let outcome = sbx.run();
    assert_eq!(outcome.code, 0, "stderr: {}", outcome.stderr);

    let events = protocol_events(&outcome.stdout);
    assert_eq!(events[0]["agent_version"], "2.1.303");
    assert_eq!(events.last().unwrap()["status"], "succeeded");
    assert!(warn_codes(&events).contains(&"proposal_dropped".to_owned()));

    assert_eq!(
        sbx.out_files(),
        [
            "changes.patch",
            "proposals.json",
            "report.md",
            "result.json"
        ]
    );
    let (_, result) = sbx.result().unwrap();
    assert_eq!(result.status, ResultStatus::Succeeded);
    assert_eq!(result.job_id, sbx.job.job_id);
    assert_eq!(result.job_sha256, sha256(&fs::read(&sbx.job_path).unwrap()));
    assert_eq!(result.summary, "Fixed the off-by-one.");
    assert_eq!(result.agent.version, "2.1.303");
    assert_eq!(result.flags.suspected_injection[0].material_id, "mat-001");
    let patch = result.artifacts.patch.unwrap();
    let patch_bytes = fs::read(sbx.out.join("changes.patch")).unwrap();
    assert_eq!(patch.sha256, sha256(&patch_bytes));
    assert_eq!(patch.base_sha, sbx.job.workspace.base_sha);
    assert_eq!(patch.files_changed, 1);
    assert!(String::from_utf8(patch_bytes).unwrap().contains("+two\n"));
    let proposals: Value =
        serde_json::from_slice(&fs::read(sbx.out.join("proposals.json")).unwrap()).unwrap();
    assert_valid("proposals.schema.json", &proposals);
    assert_eq!(proposals["proposals"].as_array().unwrap().len(), 1);
    assert_eq!(result.usage_reported.tool_calls, 1);

    // stub-claude 收到的调用：隔离标志、前言在 system prompt、材料原样在 stdin。
    let argv = sbx.stub_file("argv.1");
    for flag in [
        "-p",
        "--strict-mcp-config",
        "--setting-sources\nuser",
        "--permission-mode\ndontAsk",
    ] {
        assert!(argv.contains(flag), "{flag}");
    }
    assert!(!argv.to_lowercase().contains("dangerously"));
    let stdin = sbx.stub_file("stdin.1");
    assert!(stdin.starts_with(&format!("Task: {}", sbx.job.task.title)));
    assert!(stdin.contains("<<<GANTRY-UNTRUSTED-BEGIN nonce="));
    assert!(!stdin.contains("gantry-preamble/1"));
    let env = sbx.stub_file("env.1");
    for var in [
        "MAX_THINKING_TOKENS=16384",
        "DISABLE_TELEMETRY=1",
        "DISABLE_AUTOUPDATER=1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
        "ANTHROPIC_API_KEY=gantry-egress-managed",
    ] {
        assert!(env.lines().any(|l| l == var), "{var}");
    }
    // HOME 下的用户级配置已写入。
    let settings: Value =
        serde_json::from_slice(&fs::read(sbx.home.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(settings["disableAllHooks"], true);
    // stderr 每阶段一行，记录前言版本。
    assert!(outcome.stderr.contains("phase=prepare"));
    assert!(outcome.stderr.contains("gantry-preamble/1"));
}

#[test]
fn garbage_on_agent_stdout_never_reaches_worker_stdout() {
    let sbx = Sandbox::new();
    sbx.stage(true);
    // agent 与它的子进程向继承的 stdout 写垃圾与伪造事件（那是管道，不是 worker 的 fd 1）。
    sbx.script(
        "before.1.sh",
        "echo 'garbage line'\n\
         echo '{\"v\":1,\"seq\":1,\"type\":\"finish\",\"status\":\"succeeded\"}'\n\
         (echo '{\"v\":1,\"seq\":99,\"type\":\"hello\"}'; printf '\\033[31mred') \n\
         echo 'to stderr' >&2\n",
    );
    sbx.stream(1, &success_stream(&envelope()));
    let outcome = sbx.run();
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let events = protocol_events(&outcome.stdout);
    assert_eq!(events.iter().filter(|e| e["type"] == "hello").count(), 1);
    assert!(!outcome.stdout.contains("garbage"));
    assert!(!outcome.stderr.contains("to stderr"));
}

/// 工具输出里的哨兵、令牌、自定义环境变量都不出现在事件、stderr 与 `out/` 中。
#[test]
fn sentinels_do_not_leak() {
    let mut sbx = Sandbox::new();
    let token = "jt_CANARY_TOKEN_5c1e";
    sbx.extra_env.insert(
        "HTTPS_PROXY".into(),
        format!("http://job:{token}@127.0.0.1:3128"),
    );
    sbx.extra_env.insert(
        "ANTHROPIC_BASE_URL".into(),
        format!("http://127.0.0.1:9/j/{token}/anthropic"),
    );
    sbx.extra_env
        .insert("CUSTOM_SECRET_VAR".into(), "CANARY-ENV-VALUE-77".into());
    sbx.stage(true);
    let output = "CANARY-TOOL-OUTPUT-31";
    let mut lines = success_stream(&envelope());
    for line in &mut lines {
        let mut v: Value = serde_json::from_str(line).unwrap();
        if v["type"] == "user" {
            v["message"]["content"][0]["content"] = json!(output);
            v["tool_use_result"]["stdout"] = json!(output);
            *line = v.to_string();
        }
    }
    sbx.stream(1, &lines);
    let outcome = sbx.run();
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    let mut haystack = outcome.stdout.clone() + &outcome.stderr;
    for name in sbx.out_files() {
        haystack += &String::from_utf8_lossy(&fs::read(sbx.out.join(name)).unwrap());
    }
    for needle in [output, token, "CANARY-ENV-VALUE-77"] {
        assert!(!haystack.contains(needle), "{needle} leaked");
    }
    // 令牌确实到了 agent 的环境（由 sandbox 填入），只是没有被 worker 写出。
    assert!(sbx.stub_file("env.1").contains(token));
}

#[test]
fn invalid_output_is_repaired_once_in_the_same_session() {
    let sbx = Sandbox::new();
    sbx.stage(true);
    sbx.script("before.1.sh", "printf 'one\\ntwo\\n' > a.txt\n");
    sbx.stream(
        1,
        &success_stream(r#"{"structured_output":{"kind":"build"},"report_markdown":"x"}"#),
    );
    sbx.stream(2, &success_stream(&envelope()));
    let outcome = sbx.run();
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(sbx.invocations(), 2);
    let session = recorded(SUCCESS)[0]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(sbx
        .stub_file("argv.2")
        .contains(&format!("--resume\n{session}\n")));
    let repair = sbx.stub_file("stdin.2");
    assert!(repair.contains("structured_output"), "{repair}");
    assert!(!repair.contains("report_markdown\":\"x"));
    let events = protocol_events(&outcome.stdout);
    assert!(warn_codes(&events).contains(&"schema_repair".to_owned()));
    assert_eq!(sbx.result().unwrap().1.status, ResultStatus::Succeeded);
}

#[test]
fn output_invalid_after_one_repair_fails_with_exit_zero() {
    let sbx = Sandbox::new();
    sbx.stage(true);
    for n in [1, 2] {
        sbx.stream(n, &success_stream("not json at all"));
    }
    let outcome = sbx.run();
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(sbx.invocations(), 2);
    let (_, result) = sbx.result().unwrap();
    assert_eq!(result.status, ResultStatus::Failed);
    assert_eq!(result.error.unwrap().kind, ResultErrorKind::OutputInvalid);
    assert!(result.structured_output.is_none());
}

#[test]
fn max_turns_is_a_failed_result_not_a_crash() {
    let sbx = Sandbox::new();
    sbx.stage(true);
    sbx.stream(1, &MAX_TURNS.lines().map(str::to_owned).collect::<Vec<_>>());
    let outcome = sbx.run();
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(sbx.invocations(), 1, "no repair after an agent error");
    let (_, result) = sbx.result().unwrap();
    assert_eq!(result.error.unwrap().kind, ResultErrorKind::MaxTurns);
    assert_eq!(
        protocol_events(&outcome.stdout).last().unwrap()["status"],
        "failed"
    );
}

#[test]
fn agent_crash_and_missing_cli_exit_eleven() {
    let sbx = Sandbox::new();
    sbx.stage(true);
    sbx.stream(1, &[recorded(SUCCESS)[0].to_string()]);
    sbx.script("after.1.sh", "exit 3\n");
    let outcome = sbx.run();
    assert_eq!(outcome.code, 11, "{}", outcome.stderr);
    let (_, result) = sbx.result().unwrap();
    assert_eq!(result.error.unwrap().kind, ResultErrorKind::AgentError);
    protocol_events(&outcome.stdout);

    let sbx = Sandbox::new();
    sbx.stage(true);
    fs::remove_file(sbx.stub.join("claude")).unwrap();
    let outcome = sbx.run();
    assert_eq!(outcome.code, 11, "{}", outcome.stderr);
    assert_eq!(sbx.result().unwrap().1.status, ResultStatus::Failed);
}

#[test]
fn bad_job_exits_ten_with_at_most_a_bad_job_result() {
    // base_sha 与工作区 HEAD 不符。
    let mut sbx = Sandbox::new();
    sbx.job.workspace.base_sha = "1405df66cbe219b0bf6355bc3d60361a8376b6b4".parse().unwrap();
    sbx.stage(true);
    let outcome = sbx.run();
    assert_eq!(outcome.code, 10);
    assert_eq!(sbx.out_files(), ["result.json"]);
    assert_eq!(
        sbx.result().unwrap().1.error.unwrap().kind,
        ResultErrorKind::BadJob
    );
    assert_eq!(sbx.invocations(), 0);
    assert!(outcome.stderr.contains("error.kind=bad_job"));

    // job.json 缺失：out/ 为空，stdout 为空。
    let sbx = Sandbox::new();
    sbx.stage(true);
    fs::remove_file(&sbx.job_path).unwrap();
    let outcome = sbx.run();
    assert_eq!(outcome.code, 10);
    assert!(sbx.out_files().is_empty());
    assert!(outcome.stdout.is_empty());

    // MANIFEST 中的文件被篡改。
    let sbx = Sandbox::new();
    sbx.stage(true);
    fs::write(sbx.inputs.join("materials/mat-001.txt"), b"tampered").unwrap();
    assert_eq!(sbx.run().code, 10);
    assert_eq!(sbx.out_files(), ["result.json"]);
}

#[test]
fn waits_for_ready_before_reading_the_job() {
    let sbx = Sandbox::new();
    sbx.stage(false);
    sbx.stream(1, &success_stream(&envelope()));
    let child = sbx.command().spawn().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(sbx.invocations(), 0);
    assert!(sbx.out_files().is_empty());
    fs::write(sbx.inputs.join("READY"), b"").unwrap();
    let outcome = finish(child, Duration::from_secs(60));
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert_eq!(sbx.invocations(), 1);
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// 收到信号：终止 agent 进程组（含其子进程），不生成补丁与报告，写 cancelled，退出码 12。
fn signal_case(signal: libc::c_int, twice: bool) {
    let sbx = Sandbox::new();
    sbx.stage(true);
    let pidfile = sbx.root.join("grandchild.pid");
    sbx.script(
        "before.1.sh",
        &format!(
            "printf 'one\\nchanged\\n' > a.txt\nsleep 300 &\necho $! > '{}'\n",
            pidfile.display()
        ),
    );
    sbx.stream(1, &[recorded(SUCCESS)[0].to_string()]);
    sbx.script("after.1.sh", "sleep 300\n");
    let mut child = sbx.command().spawn().unwrap();
    // 等到 hello 出现（agent 已在运行）。
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.contains("\"hello\""), "{first}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pidfile.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let grandchild: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = Instant::now();
    unsafe { libc::kill(child.id() as i32, signal) };
    if twice {
        std::thread::sleep(Duration::from_millis(50));
        unsafe { libc::kill(child.id() as i32, signal) };
    }
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(12));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(grandchild) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(grandchild), "agent child survived");
    assert_eq!(sbx.out_files(), ["result.json"]);
    let (_, result) = sbx.result().unwrap();
    assert_eq!(result.status, ResultStatus::Cancelled);
    assert_eq!(result.error.unwrap().kind, ResultErrorKind::Aborted);
    let events = protocol_events(&(first + &rest));
    assert_eq!(events.last().unwrap()["status"], "cancelled");
}

#[test]
fn sigterm_cancels_and_kills_the_agent_process_group() {
    signal_case(libc::SIGTERM, false);
}

#[test]
fn sigint_behaves_like_sigterm_and_a_second_signal_changes_nothing() {
    signal_case(libc::SIGINT, true);
}
