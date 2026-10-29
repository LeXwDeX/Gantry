import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { TauriIpcAdapter } from "@/ipc/tauri";

/**
 * `TauriIpcAdapter` 的命令名与参数键对账（#370）。
 *
 * 外壳用 `#[tauri::command(rename_all = "snake_case")]` 声明参数，`exact_args` 或
 * `guarded_write_prepare` 的 `allowed` 表拒绝多余键。这里用 `mockIPC` 截获适配器的每次调用，
 * 与 Rust 源码里的定义对账，补上真实外壳联调前的形状漂移检查。
 *
 * 参数键集合的来源：
 * - 普通命令：`src-tauri/src/lib.rs` 的命令函数签名；
 * - `approval_sign` / `guarded_write_submit`：同一文件里的 `exact_args(args, &[...])`；
 * - `guarded_write_prepare`：`shell-core/src/guarded.rs` 按 `kind` 的 `allowed` 表。
 *
 * 不检查参数值的运行时合法性（外壳会按类型与路由校验），只检查命令名与键集合。
 */

const here = dirname(fileURLToPath(import.meta.url));
const desktopRoot = resolve(here, "../..");
const shellSrc = resolve(desktopRoot, "src-tauri/shell-core/src");
const tauriSrc = resolve(desktopRoot, "src-tauri/src");

const libRs = readFileSync(resolve(tauriSrc, "lib.rs"), "utf8");
const ipcRs = readFileSync(resolve(shellSrc, "ipc.rs"), "utf8");
const guardedRs = readFileSync(resolve(shellSrc, "guarded.rs"), "utf8");

/** `ipc.rs` 的 `COMMANDS`：唯一白名单。 */
function rustCommands(): string[] {
  const block = /pub const COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/.exec(ipcRs)?.[1];
  if (!block) throw new Error("COMMANDS not found");
  return [...block.matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);
}

/** 按顶层逗号切分参数列表，跳过泛型里的逗号。 */
function splitArgs(text: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let current = "";
  for (const ch of text) {
    if (ch === "<" || ch === "(" || ch === "[") depth += 1;
    if (ch === ">" || ch === ")" || ch === "]") depth -= 1;
    if (ch === "," && depth === 0) {
      out.push(current.trim());
      current = "";
    } else {
      current += ch;
    }
  }
  if (current.trim()) out.push(current.trim());
  return out;
}

interface CommandSig {
  required: string[];
  optional: string[];
  /** `exact_args` 或 guarded 的 allowed 表；为空表示由普通签名决定。 */
  exact?: string[];
}

/** 解析 `lib.rs` 的 `#[tauri::command]` 函数签名与 `exact_args` 列表。 */
function rustCommandSigs(): Map<string, CommandSig> {
  const result = new Map<string, CommandSig>();
  const re = /#\[tauri::command[^\]]*\]\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*\(([\s\S]*?)\)\s*->/g;
  const matches = [...libRs.matchAll(re)];
  const ignored = new Set(["shell", "app", "request"]);
  for (const [index, match] of matches.entries()) {
    const name = match[1];
    const params = splitArgs(match[2])
      .map((param) => /^(\w+)\s*:\s*(.+)$/.exec(param))
      .filter((m): m is RegExpExecArray => m !== null)
      .filter(([, paramName]) => !ignored.has(paramName))
      .map(([, paramName, type]) => ({
        name: paramName,
        optional: /^Option</.test(type.trim()),
      }));

    const bodyStart = (match.index ?? 0) + match[0].length;
    const nextStart = matches[index + 1]?.index ?? libRs.length;
    const body = libRs.slice(bodyStart, nextStart);
    const exactBlock = /exact_args\(args,\s*&\[([^\]]*)\]\)/.exec(body)?.[1];

    result.set(name, {
      required: params.filter((p) => !p.optional).map((p) => p.name),
      optional: params.filter((p) => p.optional).map((p) => p.name),
      exact: exactBlock
        ? [...exactBlock.matchAll(/"([a-z_]+)"/g)].map((m) => m[1])
        : undefined,
    });
  }
  return result;
}

/** `guarded.rs` 的 `"<kind>" => &["key", …]` 表。 */
function guardedAllowed(): Map<string, string[]> {
  const result = new Map<string, string[]>();
  const re = /"(settings_put|settings_delete|device_role|device_revoke)"\s*=>\s*&\[([\s\S]*?)\]/g;
  for (const match of guardedRs.matchAll(re)) {
    result.set(
      match[1],
      [...match[2].matchAll(/"([a-z_]+)"/g)].map((m) => m[1]),
    );
  }
  return result;
}

const COMMANDS = rustCommands();
const SIGS = rustCommandSigs();
const GUARDED = guardedAllowed();

interface Call {
  cmd: string;
  keys: string[];
}

let calls: Call[];

beforeEach(() => {
  calls = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, keys: args ? Object.keys(args) : [] });
    if (cmd === "pairing_begin") {
      return { confirmation_code: "123456", device: { id: "dev_1" } };
    }
    return null;
  });
});

afterEach(() => {
  clearMocks();
});

async function exerciseAll(): Promise<void> {
  const a = new TauriIpcAdapter();
  await a.identityStatus();
  const begin = await a.pairingBegin({
    pairingString: "gantry+pair://gantry.test:8444/?v=1&fp=sha256:00&t=gp1_x",
    deviceName: "laptop",
  });
  expect(begin).toEqual({ confirmationCode: "123456", device: { id: "dev_1" } });
  await a.pairingConfirm({ matches: true });
  await a.identityReset();
  await a.connectionState();
  await a.api("projects.list", {});
  await a.api("messages.post", {
    conversationId: "conv_1",
    body: { text: "hi", refs: [] },
    idempotencyKey: "idem_1",
  });
  await a.eventsOpen({}, () => {});
  await a.eventsOpen({ afterSeq: 7 }, () => {});
  await a.eventsAck(7);
  await a.eventsClose();
  await a.approvalPrepare({ approvalId: "apr_1" });
  await a.approvalSign({ preparedId: "prep_1" });
  await a.approvalDiscard({ preparedId: "prep_1" });
  await a.guardedWritePrepare({
    kind: "settings_put",
    scope: "global",
    resource: "models",
    value: {},
    ifMatch: '"1"',
  });
  await a.guardedWritePrepare({
    kind: "settings_put",
    scope: "project",
    projectId: "p_1",
    resource: "models",
    value: {},
    ifMatch: '"1"',
  });
  await a.guardedWritePrepare({
    kind: "settings_delete",
    projectId: "p_1",
    resource: "models",
    ifMatch: '"1"',
  });
  await a.guardedWritePrepare({
    kind: "device_role",
    deviceId: "dev_1",
    role: "operator",
  });
  await a.guardedWritePrepare({ kind: "device_revoke", deviceId: "dev_1" });
  await a.guardedWriteSubmit({ pendingId: "pend_1" });
  await a.guardedWriteDiscard({ pendingId: "pend_1" });
  await a.prefsGet();
  await a.prefsSet({ patch: { theme: "dark" } });
  await a.uiStateGet({ projectId: "p_1" });
  await a.uiStateSet({ projectId: "p_1", state: {} as never });
  await a.uiStateList();
  await a.openExternal({ url: "https://example.com/" });
  await a.openGithub({ kind: "issue", repo: "a/b", number: 1 });
  await a.openGithub({ kind: "commit", repo: "a/b", sha: "abc123" });
}

describe("TauriIpcAdapter 的命令名与参数键", () => {
  it("每次调用都在白名单内，命令名按预期顺序", async () => {
    await exerciseAll();
    expect(calls.map((c) => c.cmd)).toEqual([
      "identity_status",
      "pairing_begin",
      "pairing_confirm",
      "identity_reset",
      "connection_state",
      "api_request",
      "api_request",
      "events_open",
      "events_open",
      "events_ack",
      "events_close",
      "approval_prepare",
      "approval_sign",
      "approval_discard",
      "guarded_write_prepare",
      "guarded_write_prepare",
      "guarded_write_prepare",
      "guarded_write_prepare",
      "guarded_write_prepare",
      "guarded_write_submit",
      "guarded_write_discard",
      "prefs_get",
      "prefs_set",
      "ui_state_get",
      "ui_state_set",
      "ui_state_list",
      "open_external",
      "open_github",
      "open_github",
    ]);
    for (const call of calls) {
      expect(COMMANDS, call.cmd).toContain(call.cmd);
    }
    // 适配器覆盖白名单之外的 `diagnostics_snapshot`（由外壳内部调用）。
    expect([...new Set(calls.map((c) => c.cmd))]).toHaveLength(COMMANDS.length - 1);
  });

  it("普通命令的参数键等于或不超过 Rust 签名，必填键齐全", async () => {
    await exerciseAll();
    for (const call of calls) {
      if (call.cmd === "guarded_write_prepare" || call.cmd === "approval_sign" ||
          call.cmd === "guarded_write_submit") {
        continue;
      }
      const sig = SIGS.get(call.cmd);
      expect(sig, `lib.rs 缺少命令 ${call.cmd}`).toBeDefined();
      const allowed = new Set([...(sig?.required ?? []), ...(sig?.optional ?? [])]);
      for (const key of call.keys) {
        expect(allowed, `${call.cmd} 多余参数 ${key}`).toContain(key);
      }
      for (const key of sig?.required ?? []) {
        expect(call.keys, `${call.cmd} 缺少必填参数 ${key}`).toContain(key);
      }
    }
  });

  it("只收句柄的命令严格等于 exact_args 列表", async () => {
    await exerciseAll();
    for (const call of calls) {
      if (call.cmd !== "approval_sign" && call.cmd !== "guarded_write_submit") continue;
      const sig = SIGS.get(call.cmd);
      expect(sig?.exact, `${call.cmd} 缺少 exact_args`).toBeDefined();
      expect(call.keys.sort()).toEqual([...(sig?.exact ?? [])].sort());
    }
  });

  it("guarded_write_prepare 的键在 kind 的 allowed 表内", async () => {
    await exerciseAll();
    expect(calls.filter((c) => c.cmd === "guarded_write_prepare")).toHaveLength(5);
    // 每次调用只拿到键集合；逐 kind 重放以匹配 guarded.rs 的 allowed 表。
    calls = [];
    const a = new TauriIpcAdapter();
    const cases = [
      { kind: "settings_put", scope: "global", resource: "models", value: {}, ifMatch: '"1"' },
      { kind: "settings_put", scope: "project", projectId: "p", resource: "models", value: {}, ifMatch: '"1"' },
      { kind: "settings_delete", projectId: "p", resource: "models", ifMatch: '"1"' },
      { kind: "device_role", deviceId: "d", role: "operator" },
      { kind: "device_revoke", deviceId: "d" },
    ] as const;
    const required: Record<string, string[]> = {
      settings_put: ["kind", "scope", "resource", "value", "if_match"],
      settings_delete: ["kind", "project_id", "resource", "if_match"],
      device_role: ["kind", "device_id", "role"],
      device_revoke: ["kind", "device_id"],
    };
    for (const args of cases) {
      await a.guardedWritePrepare(args as never);
      const call = calls.at(-1);
      expect(call?.cmd).toBe("guarded_write_prepare");
      const allowed = GUARDED.get(args.kind);
      expect(allowed, `guarded.rs 缺少 kind ${args.kind}`).toBeDefined();
      for (const key of call?.keys ?? []) {
        expect(allowed, `${args.kind} 多余参数 ${key}`).toContain(key);
      }
      for (const key of required[args.kind]) {
        expect(call?.keys, `${args.kind} 缺少参数 ${key}`).toContain(key);
      }
    }
  });

  it("可选参数只在有值时出现", async () => {
    await exerciseAll();
    const events = calls.filter((c) => c.cmd === "events_open");
    expect(events[0].keys).toEqual(["on_frame"]);
    expect(events[1].keys).toEqual(["on_frame", "after_seq"]);
    const apis = calls.filter((c) => c.cmd === "api_request");
    expect(apis[0].keys).toEqual(["route", "path"]);
    expect(apis[1].keys).toEqual(["route", "path", "body", "idempotency_key"]);
    const opens = calls.filter((c) => c.cmd === "open_github");
    expect(opens[0].keys).toEqual(["kind", "repo", "number"]);
    expect(opens[1].keys).toEqual(["kind", "repo", "sha"]);
    const guarded = calls.filter((c) => c.cmd === "guarded_write_prepare");
    expect(guarded[0].keys).not.toContain("project_id");
    expect(guarded[1].keys).toContain("project_id");
  });
});
