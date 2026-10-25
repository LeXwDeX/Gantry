/**
 * 开发场景示例数据：只构造契约形状实体，不模拟引擎、策略或证书。
 * 与 prototype/Main.dc.html 的示例内容接近；需要你计数为 gantry 3 / specgit 1 / graphagent 0。
 */
import type {
  Approval,
  Conversation,
  Delivery,
  Device,
  InboxItem,
  InboxSummary,
  Level,
  Me,
  Message,
  Project,
  SettingsCatalog,
  SettingsDocument,
  SettingsStatus,
  Workflow,
  WorkflowNode,
  Worktree,
  UntrustedText,
} from "@/api/types";
import type {
  ConnectionState,
  IdentityStatus,
  Prefs,
  ProjectUiState,
} from "@/api/shell";

export type ScenarioName =
  | "default"
  | "offline"
  | "viewer"
  | "operator"
  | "unpaired"
  | "revoked"
  | "expired"
  | "unsafe"
  | "error"
  | "stale";

export const SCENARIO_NAMES: ScenarioName[] = [
  "default",
  "offline",
  "viewer",
  "operator",
  "unpaired",
  "revoked",
  "expired",
  "unsafe",
  "error",
  "stale",
];

export function isScenarioName(value: string | null): value is ScenarioName {
  return value !== null && (SCENARIO_NAMES as string[]).includes(value);
}

export const SCENARIO_PROJECT_COUNTS = {
  prj_gantry: 3,
  prj_specgit: 1,
  prj_graphagent: 0,
} as const;

export const SCENARIO_SERVER_TIME = "2026-10-24T12:59:00.000Z";
export const SCENARIO_PR9_SHA =
  "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
export const SCENARIO_PR9_STALE_SHA =
  "41ab93c0d2e4f6a81b3c5d7e9f0a2b4c6d8e0f13";

const untrusted = (text: string, source: string): UntrustedText => ({
  text,
  source,
  truncated: false,
});

export interface ScenarioData {
  scenario: ScenarioName;
  projects: Project[];
  conversations: Record<string, Conversation>;
  messages: Record<string, Message[]>;
  deliveries: Delivery[];
  worktrees: Worktree[];
  inbox: InboxItem[];
  approvals: Approval[];
  workflows: Record<string, Workflow>;
  settings: Record<string, SettingsDocument>;
  projectSettings: Record<string, Record<string, SettingsDocument>>;
  catalog: SettingsCatalog;
  status: SettingsStatus;
  devices: Device[];
  me: Me;
  identity: IdentityStatus;
  connection: ConnectionState;
}

export function buildScenarioData(scenario: ScenarioName): ScenarioData {
  const sessionNow = Date.now();
  const role =
    scenario === "viewer"
      ? "viewer"
      : scenario === "operator"
        ? "operator"
        : "approver";
  const unsafe = scenario === "unsafe";
  const stale = scenario === "stale";

  const projects: Project[] = [
    {
      id: "prj_gantry",
      repo: "LeXwDeX/gantry",
      installation_id: 48213907,
      bot_user_id: 9988776,
      default_branch: "main",
      allowed_merge_methods: ["squash", "merge"],
      merge_queue_enabled: false,
    },
    {
      id: "prj_specgit",
      repo: "LeXwDeX/specgit",
      installation_id: 48213908,
      bot_user_id: 9988776,
      default_branch: "main",
      allowed_merge_methods: ["squash"],
      merge_queue_enabled: false,
    },
    {
      id: "prj_graphagent",
      repo: "LeXwDeX/graphagent",
      installation_id: 48213909,
      bot_user_id: 9988776,
      default_branch: "main",
      allowed_merge_methods: ["squash"],
      merge_queue_enabled: false,
    },
  ];

  const conversations: Record<string, Conversation> = {
    prj_gantry: { id: "cnv_gantry", project_id: "prj_gantry" },
    prj_specgit: { id: "cnv_specgit", project_id: "prj_specgit" },
    prj_graphagent: { id: "cnv_graphagent", project_id: "prj_graphagent" },
  };

  const head = stale ? SCENARIO_PR9_STALE_SHA : SCENARIO_PR9_SHA;
  const prTitle = (text: string): UntrustedText =>
    untrusted(
      unsafe ? `${text} <img src=x onerror=alert(1)>` : text,
      "github:pr.title",
    );
  const issueTitle = (text: string): UntrustedText =>
    untrusted(
      unsafe ? `${text} <script>alert(1)</script>` : text,
      "github:issue.title",
    );

  const deliveries: Delivery[] = [
    {
      id: "dlv_gantry_5",
      project_id: "prj_gantry",
      title: "沙盒驱动：rootless + gVisor",
      level_cap: "High",
      level_effective: "High",
      stage: "implement",
      stage_status: "running",
      issues: [
        {
          repo: "LeXwDeX/gantry",
          number: 5,
          node_id: "I_kwDO5",
          title: issueTitle("沙盒驱动：rootless + gVisor"),
          state: "open",
          state_reason: null,
          synced_at: "2026-10-24T12:40:58.000Z",
        },
      ],
      pr: {
        repo: "LeXwDeX/gantry",
        number: 8,
        node_id: "PR_kwDO8",
        title: prTitle("feat: App 安装与 HMAC 校验"),
        state: "open",
        head_sha: "7c5f2b4d9a1e8f3c6b0d2e4f5a7c9b1d3e5f7a90",
        base_ref: "main",
        base_sha: "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
        draft: false,
        mergeable_state: "unstable",
        in_merge_queue: false,
        checks: { passed: 5, failed: 1, pending: 0, required_ok: false },
        review_state: "none",
        synced_at: "2026-10-24T12:40:58.000Z",
      },
      worktree_id: "wtr_gantry_5",
      parent_id: null,
      depends_on: [],
      origin: "conversation",
    },
    {
      id: "dlv_gantry_7",
      project_id: "prj_gantry",
      title: "DAG 状态机与事件投影",
      level_cap: "Max",
      level_effective: "Max",
      stage: "review",
      stage_status: "running",
      issues: [
        {
          repo: "LeXwDeX/gantry",
          number: 2,
          node_id: "I_kwDO2",
          title: issueTitle("Engine：DAG 运行时骨架"),
          state: "open",
          state_reason: null,
          synced_at: "2026-10-24T12:40:58.000Z",
        },
        {
          repo: "LeXwDeX/gantry",
          number: 11,
          node_id: "I_kwDO11",
          title: issueTitle("事件 schema 与断点续传游标"),
          state: "open",
          state_reason: null,
          synced_at: "2026-10-24T12:40:58.000Z",
        },
      ],
      pr: {
        repo: "LeXwDeX/gantry",
        number: 7,
        node_id: "PR_kwDO7",
        title: prTitle("feat: DAG 状态机与事件投影"),
        state: "open",
        head_sha: "2f8b0c6d4e1a9f7b5c3d1e0f2a4b6c8d0e1f3a5b",
        base_ref: "main",
        base_sha: "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
        draft: false,
        mergeable_state: "clean",
        in_merge_queue: false,
        checks: { passed: 8, failed: 0, pending: 0, required_ok: true },
        review_state: "approved",
        synced_at: "2026-10-24T12:50:58.000Z",
      },
      worktree_id: "wtr_gantry_7",
      parent_id: null,
      depends_on: [],
      origin: "conversation",
    },
    {
      id: "dlv_gantry_9",
      project_id: "prj_gantry",
      title: "私有 CA 与 mTLS 配对",
      level_cap: "Max",
      level_effective: "Max",
      stage: "merge",
      stage_status: "waiting_human",
      issues: [
        {
          repo: "LeXwDeX/gantry",
          number: 4,
          node_id: "I_kwDO4",
          title: issueTitle("私有 CA 与 mTLS 配对"),
          state: "open",
          state_reason: null,
          synced_at: "2026-10-24T12:52:58.000Z",
        },
      ],
      pr: {
        repo: "LeXwDeX/gantry",
        number: 9,
        node_id: "PR_kwDO9",
        title: prTitle("feat: 配对 token 与 CSR 签发"),
        state: "open",
        head_sha: head,
        base_ref: "main",
        base_sha: "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
        draft: false,
        mergeable_state: "clean",
        in_merge_queue: false,
        checks: { passed: 9, failed: 0, pending: 0, required_ok: true },
        review_state: "approved",
        synced_at: "2026-10-24T12:54:58.000Z",
      },
      worktree_id: "wtr_gantry_9",
      parent_id: null,
      depends_on: [],
      origin: "conversation",
    },
    {
      id: "dlv_gantry_6",
      project_id: "prj_gantry",
      title: "桌面端控制台原型",
      level_cap: "Low",
      level_effective: "Low",
      stage: "implement",
      stage_status: "waiting_human",
      issues: [
        {
          repo: "LeXwDeX/gantry",
          number: 6,
          node_id: "I_kwDO6",
          title: issueTitle("桌面端控制台原型"),
          state: "open",
          state_reason: null,
          synced_at: "2026-10-24T12:56:58.000Z",
        },
      ],
      pr: null,
      worktree_id: "wtr_gantry_6",
      parent_id: null,
      depends_on: [],
      origin: "conversation",
    },
    {
      id: "dlv_gantry_done",
      project_id: "prj_gantry",
      title: "初始设计文档集",
      level_cap: "Low",
      level_effective: "Low",
      stage: "closed",
      stage_status: "done",
      issues: [],
      pr: {
        repo: "LeXwDeX/gantry",
        number: 1,
        node_id: "PR_kwDO1",
        title: prTitle("docs: 初始设计文档集"),
        state: "merged",
        head_sha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
        base_ref: "main",
        base_sha: "1405df66cbe219b0bf6355bc3d60361a8376b6b4",
        draft: false,
        mergeable_state: "clean",
        in_merge_queue: false,
        checks: { passed: 3, failed: 0, pending: 0, required_ok: true },
        review_state: "approved",
        synced_at: "2026-10-24T11:10:00.000Z",
      },
      worktree_id: "wtr_gantry_1",
      parent_id: null,
      depends_on: [],
      origin: "conversation",
    },
    {
      id: "dlv_specgit_31",
      project_id: "prj_specgit",
      title: "watch 超时可配置",
      level_cap: "High",
      level_effective: "High",
      stage: "spec",
      stage_status: "waiting_human",
      issues: [
        {
          repo: "LeXwDeX/specgit",
          number: 31,
          node_id: "I_kwDS31",
          title: issueTitle("watch 超时可配置"),
          state: "open",
          state_reason: null,
          synced_at: "2026-10-24T11:19:00.000Z",
        },
      ],
      pr: null,
      worktree_id: null,
      parent_id: null,
      depends_on: [],
      origin: "conversation",
    },
  ];

  const worktrees: Worktree[] = [
    {
      id: "wtr_gantry_5",
      project_id: "prj_gantry",
      branch: "feat/sandbox-driver",
      base_ref: "main",
      path: "/var/lib/gantry/worktrees/gantry/sandbox-driver",
      lock: {
        holder: "engine",
        delivery_id: "dlv_gantry_5",
        acquired_at: "2026-10-24T12:30:00.000Z",
        lease_until: "2026-10-24T13:00:00.000Z",
      },
    },
    {
      id: "wtr_gantry_7",
      project_id: "prj_gantry",
      branch: "feat/dag-engine",
      base_ref: "main",
      path: "/var/lib/gantry/worktrees/gantry/dag-engine",
      lock: {
        holder: "guardian",
        delivery_id: "dlv_gantry_7",
        acquired_at: "2026-10-24T12:20:00.000Z",
        lease_until: "2026-10-24T12:50:00.000Z",
      },
    },
    {
      id: "wtr_gantry_9",
      project_id: "prj_gantry",
      branch: "feat/mtls-pair",
      base_ref: "main",
      path: "/var/lib/gantry/worktrees/gantry/mtls-pair",
      lock: null,
    },
    {
      id: "wtr_gantry_6",
      project_id: "prj_gantry",
      branch: "feat/console-proto",
      base_ref: "main",
      path: "/var/lib/gantry/worktrees/gantry/console-proto",
      lock: {
        holder: "engine",
        delivery_id: "dlv_gantry_6",
        acquired_at: "2026-10-24T12:35:00.000Z",
        lease_until: "2026-10-24T13:05:00.000Z",
      },
    },
    {
      id: "wtr_gantry_1",
      project_id: "prj_gantry",
      branch: "docs/initial-design",
      base_ref: "main",
      path: "/var/lib/gantry/worktrees/gantry/initial-design",
      lock: null,
    },
  ];

  const inbox: InboxItem[] = [
    {
      id: "inb_apr9",
      project_id: "prj_gantry",
      delivery_id: "dlv_gantry_9",
      kind: "approval_needed",
      severity: "blocker",
      summary: unsafe
        ? "PR #9 可以合并，需要你的审批。<iframe src=javascript:alert(1)>"
        : "PR #9 可以合并，需要你的审批。",
      payload: { approval_id: "apr_gantry_9" },
      status: "open",
      dedup_key: "approval:apr_gantry_9",
    },
    {
      id: "inb_esc6",
      project_id: "prj_gantry",
      delivery_id: "dlv_gantry_6",
      kind: "escalation",
      severity: "warn",
      summary: "#6 改动超出 Low 档范围，需要升到 Medium。",
      payload: {
        reason_code: "level_cap_exceeded",
        node_id: null,
        attempt_id: null,
        target_level: "Medium",
        evidence: [{ kind: "issue", repo: "LeXwDeX/gantry", number: 6 }],
        options: [
          { id: "upgrade", label: "升到 Medium 并继续", primary: true },
          { id: "narrow", label: "保持当前，缩小范围" },
          { id: "abandon", label: "放弃" },
        ],
      },
      status: "open",
      dedup_key: "escalation:dlv_gantry_6:level_cap_exceeded:Medium",
    },
    {
      id: "inb_choice7",
      project_id: "prj_gantry",
      delivery_id: "dlv_gantry_7",
      kind: "decision_needed",
      severity: "info",
      summary: "性能视角 finding 已被 arbiter 驳回，是否改为修复？",
      payload: {
        kind: "review_dispute",
        reason_code: "guardian.scope_expanding_fix",
        options: [
          { id: "fix", label: "改为修复", primary: true },
          { id: "keep", label: "维持驳回" },
        ],
      },
      status: "open",
      dedup_key: "review_dispute:dlv_gantry_7:performance",
    },
    {
      id: "inb_spec31",
      project_id: "prj_specgit",
      delivery_id: "dlv_specgit_31",
      kind: "spec_needed",
      severity: "warn",
      summary: "#31 缺少验收标准。",
      payload: {
        missing: [{ aspect: "验收标准", question: "watch 超时的可配置范围与默认值是什么？" }],
      },
      status: "open",
      dedup_key: "spec:dlv_specgit_31:acceptance",
    },
  ];

  const approvals: Approval[] = [
    {
      id: "apr_gantry_9",
      delivery_id: "dlv_gantry_9",
      action: "merge_pr",
      target: { kind: "pr", repo: "LeXwDeX/gantry", number: 9 },
      subject_digest: SCENARIO_PR9_SHA,
      device_id: null,
      signature: null,
      status: "pending",
      expires_at: new Date(sessionNow + 24 * 60 * 60 * 1000).toISOString(),
    },
  ];

  const workflows: Record<string, Workflow> = {
    wfl_gantry_5_build: {
      id: "wfl_gantry_5_build",
      delivery_id: "dlv_gantry_5",
      conversation_id: null,
      stage: "implement",
      graph_rev: 1,
      status: "running",
      template: "high.implement",
      nodes: [
        node("nod_gantry_5_build", "wfl_gantry_5_build", "build", "running", null),
        node("nod_gantry_5_verify", "wfl_gantry_5_build", "verify", "pending", null),
      ],
    },
    wfl_gantry_7_review: {
      id: "wfl_gantry_7_review",
      delivery_id: "dlv_gantry_7",
      conversation_id: null,
      stage: "review",
      graph_rev: 2,
      status: "running",
      template: "max.review",
      nodes: [
        node("nod_gantry_7_sec", "wfl_gantry_7_review", "review:security", "succeeded", "ACCEPT"),
        node("nod_gantry_7_perf", "wfl_gantry_7_review", "review:performance", "succeeded", "REJECT"),
        node("nod_gantry_7_arb", "wfl_gantry_7_review", "arbiter", "running", null),
      ],
    },
    wfl_gantry_6_build: {
      id: "wfl_gantry_6_build",
      delivery_id: "dlv_gantry_6",
      conversation_id: null,
      stage: "implement",
      graph_rev: 1,
      status: "paused",
      template: "low.implement",
      nodes: [node("nod_gantry_6_single", "wfl_gantry_6_build", "single", "running", null)],
    },
    wfl_specgit_31_spec: {
      id: "wfl_specgit_31_spec",
      delivery_id: "dlv_specgit_31",
      conversation_id: null,
      stage: "spec",
      graph_rev: 1,
      status: "running",
      template: "high.spec",
      nodes: [node("nod_specgit_31_admission", "wfl_specgit_31_spec", "admission", "failed", "REVISE")],
    },
  };

  const messages: Record<string, Message[]> = {
    prj_gantry: buildGantryMessages(unsafe),
    prj_specgit: [
      {
        id: "msg_specgit_1",
        conversation_id: "cnv_specgit",
        role: "agent",
        agent_kind: "utility",
        tone: null,
        text: "specgit #31 进入规格阶段：High 档准入未通过，缺少验收标准。",
        level_cap: null,
        refs: [{ kind: "issue", repo: "LeXwDeX/specgit", number: 31 }],
        cards: [],
        delivery_id: "dlv_specgit_31",
      },
    ],
    prj_graphagent: [],
  };

  const settings: Record<string, SettingsDocument> = {
    levels: {
      resource: "levels",
      scope: "global",
      rev: 18102,
      value: { default_level: "Medium" },
    },
    models: {
      resource: "models",
      scope: "global",
      rev: 18235,
      value: modelsValue(),
    },
    policies: {
      resource: "policies",
      scope: "global",
      rev: 18120,
      value: {
        actions: { merge_pr: "approve" },
        approval: { ttl_s: 86400 },
      },
    },
  };

  const projectSettings: Record<string, Record<string, SettingsDocument>> = {
    prj_gantry: {
      levels: {
        resource: "levels",
        scope: "project",
        project_id: "prj_gantry",
        rev: 0,
        value: null,
      },
      models: {
        resource: "models",
        scope: "project",
        project_id: "prj_gantry",
        rev: 0,
        value: null,
      },
    },
    prj_specgit: {
      levels: {
        resource: "levels",
        scope: "project",
        project_id: "prj_specgit",
        rev: 0,
        value: null,
      },
      models: {
        resource: "models",
        scope: "project",
        project_id: "prj_specgit",
        rev: 0,
        value: null,
      },
    },
    prj_graphagent: {
      levels: {
        resource: "levels",
        scope: "project",
        project_id: "prj_graphagent",
        rev: 0,
        value: null,
      },
      models: {
        resource: "models",
        scope: "project",
        project_id: "prj_graphagent",
        rev: 0,
        value: null,
      },
    },
  };

  const catalog = buildCatalog();
  const status: SettingsStatus = {
    latest_rev: 18235,
    modules: {
      models: { applied_rev: 18235, health: "ok" },
      engine: { applied_rev: 18235, health: "ok" },
      egress: { applied_rev: 18235, health: "ok", unresolved_secret_refs: [] },
    },
  };

  const device: Device = {
    id: "dev_scenario_main",
    name: "Susan 的 MacBook",
    platform: "macos",
    cert_serial: "5f3a9c01d27be4468a1c",
    pubkey_fpr: "sha256:9fe698148e3e8234105e25bf598c9689640aba943b8275348470152799f1dd42",
    approval_pubkey_fpr:
      "sha256:7a1c3e5f90b2d4f6a8c0e2a4b6d8f0a2c4e6a8b0d2f4a6c8e0b2d4f6a8c0e2a4",
    role,
    status: "active",
    last_seen: SCENARIO_SERVER_TIME,
  };

  const second: Device = {
    id: "dev_scenario_second",
    name: "书房 Mac",
    platform: "macos",
    cert_serial: "44a19b02e18cf5579b2d",
    pubkey_fpr: "sha256:11fe698148e3e8234105e25bf598c9689640aba943b8275348470152799f1dd42",
    approval_pubkey_fpr:
      "sha256:22c3e5f90b2d4f6a8c0e2a4b6d8f0a2c4e6a8b0d2f4a6c8e0b2d4f6a8c0e2a4",
    role: "operator",
    status: "active",
    last_seen: "2026-10-24T10:11:00.000Z",
  };

  const me: Me = {
    device,
    profile: "desktop",
    certificate: {
      not_after: "2026-11-23T12:00:00.000Z",
      renew_after: "2026-11-13T12:00:00.000Z",
    },
    server: {
      api_version: "v1",
      version: "0.1.0-scenario",
      time: new Date(Date.now()).toISOString(),
      log_epoch: 1,
    },
  };

  const identity: IdentityStatus = {
    state:
      scenario === "unpaired"
        ? "unpaired"
        : scenario === "revoked"
          ? "revoked"
          : scenario === "expired"
            ? "cert_expired"
            : "paired",
    server_host: "gantry.example.com:8443",
    ca_fingerprint:
      "sha256:3b7e0c52a91d6f48e2b05c7a9d1f3e64b8a20c5d7e91f3a6b4c8d0e2f1a39b57",
    device:
      scenario === "unpaired"
        ? null
        : { ...device, platform: "macos" },
    cert:
      scenario === "unpaired"
        ? null
        : {
            serial: device.cert_serial,
            not_after: "2026-11-23T12:00:00.000Z",
            renew_after: "2026-11-13T12:00:00.000Z",
          },
    key_backend: "test_software",
  };

  const connection: ConnectionState = {
    phase:
      scenario === "offline"
        ? "reconnecting"
        : scenario === "unpaired"
          ? "unpaired"
          : scenario === "revoked"
            ? "revoked"
            : scenario === "expired"
              ? "cert_expired"
              : "live",
    epoch: 1,
    last_bytes_age_ms: scenario === "offline" ? 65_000 : 1_200,
    cert_not_after: "2026-11-23T12:00:00.000Z",
    cert_expiring: false,
    log_epoch: 1,
    clock_skew_ms: 0,
  };

  return {
    scenario,
    projects,
    conversations,
    messages,
    deliveries,
    worktrees,
    inbox,
    approvals,
    workflows,
    settings,
    projectSettings,
    catalog,
    status,
    devices: [device, second],
    me,
    identity,
    connection,
  };
}

function node(
  id: string,
  workflowId: string,
  role: WorkflowNode["role"],
  status: WorkflowNode["status"],
  verdict: WorkflowNode["verdict"],
): WorkflowNode {
  return {
    id,
    workflow_id: workflowId,
    role,
    tier: role.startsWith("review:") ? "worker" : "planner",
    exec: "sandbox",
    depends_on: [],
    required: true,
    status,
    verdict,
    superseded_by: null,
  };
}

function buildGantryMessages(unsafe: boolean): Message[] {
  const suffix = unsafe
    ? "\n<script>alert(1)</script> [点我](https://evil.example/x) ![图](https://evil.example/p.png)"
    : "";
  return [
    {
      id: "msg_gantry_1",
      conversation_id: "cnv_gantry",
      role: "user",
      agent_kind: null,
      tone: null,
      text: "给 #5 补验收标准：rootless 容器能跑通仓库测试，出网只走白名单代理。补完开工。",
      level_cap: "High",
      refs: [{ kind: "issue", repo: "LeXwDeX/gantry", number: 5 }],
      cards: [],
      delivery_id: null,
    },
    {
      id: "msg_gantry_2",
      conversation_id: "cnv_gantry",
      role: "agent",
      agent_kind: "planner",
      tone: null,
      text: "已把验收标准写进 #5，准入 READY。按上限 High 开工（实际 High）：",
      level_cap: null,
      refs: [
        { kind: "issue", repo: "LeXwDeX/gantry", number: 5 },
        { kind: "worktree", id: "wtr_gantry_5" },
      ],
      cards: [
        {
          kind: "plan",
          delivery_id: "dlv_gantry_5",
          workflow_id: "wfl_gantry_5_build",
          summary: untrusted(
            "新建工作树 feat/sandbox-driver，分配沙盒 sbx-09（rootless + gVisor）。",
            "conversation:plan",
          ),
        },
      ],
      delivery_id: "dlv_gantry_5",
    },
    {
      id: "msg_gantry_3",
      conversation_id: "cnv_gantry",
      role: "event",
      agent_kind: null,
      tone: "info",
      text: "PR #8 · CI typecheck 失败 → 在本 PR 范围内，静默自愈",
      level_cap: null,
      refs: [{ kind: "pr", repo: "LeXwDeX/gantry", number: 8 }],
      cards: [],
      delivery_id: "dlv_gantry_5",
    },
    {
      id: "msg_gantry_4",
      conversation_id: "cnv_gantry",
      role: "event",
      agent_kind: null,
      tone: "warn",
      text: "PR #8 · lint nit × 2 → 攒到下次推送",
      level_cap: null,
      refs: [{ kind: "pr", repo: "LeXwDeX/gantry", number: 8 }],
      cards: [],
      delivery_id: "dlv_gantry_5",
    },
    {
      id: "msg_gantry_5",
      conversation_id: "cnv_gantry",
      role: "event",
      agent_kind: null,
      tone: "ok",
      text: "PR #7 · 安全 finding 验证属实，修复已推送",
      level_cap: null,
      refs: [{ kind: "pr", repo: "LeXwDeX/gantry", number: 7 }],
      cards: [],
      delivery_id: "dlv_gantry_7",
    },
    {
      id: "msg_gantry_6",
      conversation_id: "cnv_gantry",
      role: "agent",
      agent_kind: "worker",
      tone: null,
      text: `#5 build 第一步完成：rootless 容器驱动能拉起沙盒并挂载一次性工作区，正在接出网白名单代理。${suffix}`,
      level_cap: null,
      refs: [{ kind: "issue", repo: "LeXwDeX/gantry", number: 5 }],
      cards: [],
      delivery_id: "dlv_gantry_5",
    },
    {
      id: "msg_gantry_7",
      conversation_id: "cnv_gantry",
      role: "user",
      agent_kind: null,
      tone: null,
      text: "PR #7 的性能 finding 为什么被驳回？",
      level_cap: "Max",
      refs: [{ kind: "pr", repo: "LeXwDeX/gantry", number: 7 }],
      cards: [],
      delivery_id: null,
    },
    {
      id: "msg_gantry_8",
      conversation_id: "cnv_gantry",
      role: "agent",
      agent_kind: "planner",
      tone: null,
      text: "arbiter 判断该 finding 指向的代码只在启动时执行一次，不在热路径上；修复要多加一层缓存，收益抵不过复杂度。理由已回复在对应 review thread。",
      level_cap: null,
      refs: [{ kind: "pr", repo: "LeXwDeX/gantry", number: 7 }],
      cards: [
        {
          kind: "choice",
          inbox_id: "inb_choice7",
          options: [
            { id: "fix", label: "改为修复", primary: true },
            { id: "keep", label: "维持驳回" },
          ],
        },
      ],
      delivery_id: "dlv_gantry_7",
    },
    {
      id: "msg_gantry_9",
      conversation_id: "cnv_gantry",
      role: "agent",
      agent_kind: "utility",
      tone: null,
      text: "离开期间：后台自愈 1 件；需要你 2 件（PR #9 签名审批、#6 升档）。",
      level_cap: null,
      refs: [],
      cards: [
        {
          kind: "summary",
          items: [
            {
              text: untrusted("后台自愈 1 件：PR #8 typecheck 修复", "conversation:summary"),
              ref: { kind: "pr", repo: "LeXwDeX/gantry", number: 8 },
            },
            {
              text: untrusted("需要你 2 件：PR #9 签名审批、#6 升档", "conversation:summary"),
              ref: { kind: "pr", repo: "LeXwDeX/gantry", number: 9 },
            },
          ],
        },
      ],
      delivery_id: null,
    },
    {
      id: "msg_gantry_10",
      conversation_id: "cnv_gantry",
      role: "agent",
      agent_kind: "utility",
      tone: null,
      text: "PR #9 通过对抗 review，arbiter 给出 ACCEPT。merge 涉及 auth 路径，按策略需要你签名审批。",
      level_cap: null,
      refs: [
        { kind: "pr", repo: "LeXwDeX/gantry", number: 9 },
        { kind: "issue", repo: "LeXwDeX/gantry", number: 4 },
      ],
      cards: [{ kind: "approval", approval_id: "apr_gantry_9" }],
      delivery_id: "dlv_gantry_9",
    },
    {
      id: "msg_gantry_11",
      conversation_id: "cnv_gantry",
      role: "agent",
      agent_kind: "planner",
      tone: null,
      text: "#6 的改动超出 Low 档范围：需要同时改 desktop-ui 和 api 契约，单节点实现风险偏高。",
      level_cap: null,
      refs: [{ kind: "issue", repo: "LeXwDeX/gantry", number: 6 }],
      cards: [{ kind: "escalation", inbox_id: "inb_esc6" }],
      delivery_id: "dlv_gantry_6",
    },
  ];
}

function modelsValue(): unknown {
  return {
    providers: {
      anthropic: {
        kind: "anthropic",
        base_url: "https://api.anthropic.com",
        credential: {
          type: "api_key",
          secret_ref: "model.anthropic.main",
          present: true,
        },
      },
    },
    utility: {
      provider: "anthropic",
      model: "claude-haiku-4-5",
      reasoning_effort: "low",
    },
    planner: {
      provider: "anthropic",
      model: "claude-sonnet-4-5",
      reasoning_effort: "high",
    },
    worker: {
      provider: "anthropic",
      model: "claude-sonnet-5",
      reasoning_effort: "by_level",
      effort_by_level: {
        low: { default: "low" },
        medium: { default: "medium" },
        high: { default: "medium", critical: "high" },
        max: { default: "high", critical: "max" },
      },
      adapter: "claude-code",
    },
  };
}

function buildCatalog(): SettingsCatalog {
  return {
    schema_version: 1,
    models: {
      capabilities: {
        "anthropic/claude-haiku-4-5": {
          tools: false,
          reasoning_efforts: ["none", "low", "medium"],
          context_window: 200000,
        },
        "anthropic/claude-sonnet-4-5": {
          tools: true,
          reasoning_efforts: ["none", "low", "medium", "high", "max"],
          context_window: 200000,
        },
        "anthropic/claude-sonnet-5": {
          tools: true,
          reasoning_efforts: ["low", "medium", "high", "max"],
          context_window: 400000,
        },
        "anthropic/claude-opus-4-1": {
          tools: true,
          reasoning_efforts: ["low", "medium", "high", "max"],
          context_window: 200000,
        },
      },
      adapters: {
        "claude-code": { provider_kinds: ["anthropic"], supports_fallback: false },
      },
    },
    levels: {
      graph_templates: {
        none: [],
        low: ["low.single"],
        medium: ["medium.plan_build_verify"],
        high: ["high.multi_review"],
        max: ["max.adversarial"],
        ultimate: ["ultimate.milestone"],
      },
    },
    policies: {
      actions: {
        merge_pr: { default: "approve", allowed: ["approve", "deny"], dangerous: true, reserved: false },
        force_push: { default: "approve", allowed: ["approve", "deny"], dangerous: true, reserved: false },
        create_pr: { default: "allow", allowed: ["allow", "deny"], dangerous: false, reserved: false },
      },
    },
    guardian: {
      triage: {
        nit: { default: "batch", allowed: ["batch", "escalate"], locked: false },
        security_finding: { locked: true, value: "escalate", allowed: [] },
      },
    },
    roles: { approver_keys: ["models.providers", "levels.*.graph_template", "github.*"] },
    egress: {
      suggested_groups: {},
      forbidden_hosts: ["169.254.169.254", "metadata.google.internal"],
    },
    notify: {
      channels: {
        desktop: { available: true },
        mobile_push: { available: false, reason: "reserved" },
      },
    },
  };
}

export const DEFAULT_PREFS: Prefs = {
  theme: "dark",
  density: "compact",
  language: "zh-CN",
  systemNotifications: true,
  layout: {
    leftWidth: 240,
    inspectorWidth: 300,
    leftCollapsed: false,
    inspectorCollapsed: false,
  },
};

export const DEFAULT_PROJECT_UI: ProjectUiState = {
  view: "chat",
  sel: null,
  draft: "",
  ctx: null,
  levelOverride: null,
  needsOnly: false,
  showAll: { issues: false, prs: false, worktrees: false },
  expandedGroups: [],
  lastViewed: null,
  chatScroll: { atBottom: true, anchor_message_id: null, offset: 0 },
  outbox: [],
};

export const DEFAULT_LEVELS: Record<string, Level> = {
  prj_gantry: "Medium",
  prj_specgit: "High",
  prj_graphagent: "Low",
};

export function summarizeInbox(items: InboxItem[]): InboxSummary {
  const open = items.filter((i) => i.status === "open");
  const byProject = new Map<string | null, InboxSummary["by_project"][number]>();
  const rank = { info: 0, warn: 1, blocker: 2 } as const;
  for (const item of open) {
    const current = byProject.get(item.project_id);
    if (current) {
      current.open += 1;
      if (rank[item.severity] > rank[current.max_severity]) {
        current.max_severity = item.severity;
      }
    } else {
      byProject.set(item.project_id, {
        project_id: item.project_id,
        open: 1,
        max_severity: item.severity,
      });
    }
  }
  return {
    total_open: open.length,
    by_project: [...byProject.values()],
  };
}
