/**
 * 开发场景示例数据：只构造契约形状实体，不模拟引擎、策略或证书。
 * 与 prototype/Main.dc.html 的示例内容接近；需要你计数为 gantry 3 / specgit 1 / graphagent 0。
 */
import type {
  Approval,
  Attempt,
  BudgetLedger,
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
  /** 尝试不可变；重试只追加，不覆盖旧值。 */
  attempts: Attempt[];
  /** 按交付单元 ID 索引的只读预算台账。 */
  budgets: Record<string, BudgetLedger>;
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
        node(
          "nod_gantry_5_verify",
          "wfl_gantry_5_build",
          "verify",
          "pending",
          null,
          ["nod_gantry_5_build"],
        ),
      ],
    },
    wfl_gantry_7_review: {
      id: "wfl_gantry_7_review",
      delivery_id: "dlv_gantry_7",
      conversation_id: null,
      stage: "review",
      graph_rev: 3,
      status: "running",
      template: "max.review",
      nodes: [
        node("nod_gantry_7_sec", "wfl_gantry_7_review", "review:security", "succeeded", "ACCEPT"),
        node("nod_gantry_7_perf", "wfl_gantry_7_review", "review:performance", "succeeded", "REJECT"),
        node(
          "nod_gantry_7_perf_old",
          "wfl_gantry_7_review",
          "review:performance",
          "superseded",
          null,
          [],
          "nod_gantry_7_perf",
        ),
        node(
          "nod_gantry_7_arb",
          "wfl_gantry_7_review",
          "arbiter",
          "running",
          null,
          ["nod_gantry_7_sec", "nod_gantry_7_perf"],
        ),
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
      value: levelsValue(),
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
      value: policiesValue(),
    },
    guardian: {
      resource: "guardian",
      scope: "global",
      rev: 18131,
      value: guardianValue(),
    },
    sandbox: {
      resource: "sandbox",
      scope: "global",
      rev: 18140,
      value: sandboxValue(),
    },
    egress: {
      resource: "egress",
      scope: "global",
      rev: 18148,
      value: egressValue(),
    },
    notify: {
      resource: "notify",
      scope: "global",
      rev: 18153,
      value: notifyValue(),
    },
    github: {
      resource: "github",
      scope: "global",
      rev: 18160,
      value: githubValue(),
    },
  };

  const projectSettings: Record<string, Record<string, SettingsDocument>> = {};
  for (const project of projects) {
    projectSettings[project.id] = {};
    for (const resource of SETTINGS_RESOURCES) {
      projectSettings[project.id][resource] = {
        resource,
        scope: "project",
        project_id: project.id,
        rev: 0,
        value: null,
      };
    }
  }
  projectSettings.prj_gantry.models = projectDoc("prj_gantry", "models", 7, {
    worker: {
      provider: "anthropic",
      model: "claude-sonnet-5",
      reasoning_effort: "high",
    },
  });
  projectSettings.prj_gantry.levels = projectDoc("prj_gantry", "levels", 5, {
    none: { daily_tokens: 400_000 },
    high: {
      budget: { wall_time_s: 9_000, concurrency: 2 },
    },
  });
  projectSettings.prj_gantry.policies = projectDoc("prj_gantry", "policies", 4, {
    actions: { create_comment: "deny" },
    paths: {
      simulated_auth: {
        globs: ["**/auth/**"],
        disposition: "deny",
      },
    },
    approval: { ttl_s: 3_600 },
    patch: { max_files: 100 },
  });
  projectSettings.prj_gantry.guardian = projectDoc("prj_gantry", "guardian", 3, {
    triage: { nit: "escalate" },
    no_progress: { same_class_repeats: 1 },
    status_checklist: { enabled: false, min_update_interval_s: 60 },
  });
  projectSettings.prj_gantry.sandbox = projectDoc("prj_gantry", "sandbox", 2, {
    limits: { cpu_cores: 1, mem_mb: 2_048, wall_time_s: 3_000 },
    devcontainer: { mode: "safe_subset" },
  });
  projectSettings.prj_gantry.egress = projectDoc("prj_gantry", "egress", 2, {
    profiles: { default: { groups: ["model_api", "npm"] } },
  });
  projectSettings.prj_specgit.levels = projectDoc("prj_specgit", "levels", 1, {
    default_level: "High",
  });
  projectSettings.prj_graphagent.levels = projectDoc("prj_graphagent", "levels", 1, {
    default_level: "Low",
  });

  const catalog = buildCatalog();
  const status: SettingsStatus = {
    latest_rev: 18235,
    modules: {
      models: { applied_rev: 18235, health: "ok" },
      engine: { applied_rev: 18102, health: "degraded", detail: "等待应用上级档位预算" },
      policy: { applied_rev: 18120, health: "ok" },
      guardian: { applied_rev: 18131, health: "ok" },
      sandbox: { applied_rev: 18140, health: "ok" },
      egress: { applied_rev: 18148, health: "ok", unresolved_secret_refs: [] },
      notify: { applied_rev: 18153, health: "ok" },
      forge: { applied_rev: 18160, health: "ok" },
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

  const attempts = buildAttempts();
  const budgets = buildBudgets(deliveries, attempts);

  return {
    scenario,
    projects,
    conversations,
    messages: stampMessages(messages),
    deliveries,
    worktrees,
    inbox,
    approvals,
    workflows,
    attempts,
    budgets,
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
  dependsOn: string[] = [],
  supersededBy: string | null = null,
): WorkflowNode {
  return {
    id,
    workflow_id: workflowId,
    role,
    tier: role.startsWith("review:") ? "worker" : "planner",
    exec: "sandbox",
    depends_on: dependsOn,
    required: true,
    status,
    verdict,
    superseded_by: supersededBy,
  };
}

const SETTINGS_RESOURCES = [
  "models",
  "levels",
  "policies",
  "guardian",
  "sandbox",
  "egress",
  "notify",
  "github",
] as const;

function projectDoc(
  projectId: string,
  resource: string,
  rev: number,
  value: unknown,
): SettingsDocument {
  return {
    resource,
    scope: "project",
    project_id: projectId,
    rev,
    value,
  };
}

function stampMessages(messages: Record<string, Message[]>): Record<string, Message[]> {
  const base = Date.parse("2026-10-24T12:00:00.000Z");
  const result: Record<string, Message[]> = {};
  for (const [projectId, items] of Object.entries(messages)) {
    result[projectId] = items.map((message, index) => ({
      ...message,
      created_at:
        message.created_at ??
        new Date(base + index * 60_000 + (projectId === "prj_specgit" ? 300_000 : 0)).toISOString(),
      updated_at: message.updated_at ?? undefined,
    }));
  }
  return result;
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

export const SCENARIO_LEVEL_BUDGETS: Record<
  string,
  {
    template: string;
    utility: number;
    planner: number;
    worker: number;
    nodes: number;
    rounds: number;
    wall_time_s: number;
    concurrency: number;
    forge_writes: number;
    forge_writes_per_hour: number;
  }
> = {
  low: {
    template: "low.single",
    utility: 100_000,
    planner: 50_000,
    worker: 1_000_000,
    nodes: 6,
    rounds: 2,
    wall_time_s: 1_800,
    concurrency: 1,
    forge_writes: 30,
    forge_writes_per_hour: 20,
  },
  medium: {
    template: "medium.plan_build_verify",
    utility: 200_000,
    planner: 150_000,
    worker: 3_000_000,
    nodes: 20,
    rounds: 3,
    wall_time_s: 5_400,
    concurrency: 2,
    forge_writes: 60,
    forge_writes_per_hour: 40,
  },
  high: {
    template: "high.multi_review",
    utility: 400_000,
    planner: 300_000,
    worker: 8_000_000,
    nodes: 40,
    rounds: 4,
    wall_time_s: 10_800,
    concurrency: 3,
    forge_writes: 120,
    forge_writes_per_hour: 60,
  },
  max: {
    template: "max.adversarial",
    utility: 800_000,
    planner: 800_000,
    worker: 20_000_000,
    nodes: 80,
    rounds: 6,
    wall_time_s: 21_600,
    concurrency: 4,
    forge_writes: 200,
    forge_writes_per_hour: 80,
  },
  ultimate: {
    template: "ultimate.milestone",
    utility: 3_000_000,
    planner: 3_000_000,
    worker: 100_000_000,
    nodes: 400,
    rounds: 40,
    wall_time_s: 172_800,
    concurrency: 4,
    forge_writes: 1_000,
    forge_writes_per_hour: 150,
  },
};

const EGRESS_GROUPS: Record<string, { hosts: string[] }> = {
  model_api: { hosts: ["api.anthropic.com"] },
  npm: { hosts: ["registry.npmjs.org"] },
  pypi: { hosts: ["pypi.org", "files.pythonhosted.org"] },
  crates: { hosts: ["crates.io", "index.crates.io", "static.crates.io"] },
  go: { hosts: ["proxy.golang.org", "sum.golang.org"] },
  maven: { hosts: ["repo.maven.apache.org", "repo1.maven.org"] },
  private_npm: { hosts: ["npm.internal.example"] },
};

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
    retry: { max_attempts: 3, backoff_ms: 1000 },
    utility: {
      provider: "anthropic",
      model: "claude-haiku-4-5",
      reasoning_effort: "low",
      fallback: [],
      request_timeout_s: 60,
      total_timeout_s: 120,
    },
    planner: {
      provider: "anthropic",
      model: "claude-sonnet-4-5",
      reasoning_effort: "high",
      fallback: [],
      request_timeout_s: 300,
      total_timeout_s: 900,
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
      fallback: [],
      adapter: "claude-code",
      request_timeout_s: 600,
      attempt_wall_s: 3600,
      max_turns: 60,
    },
  };
}

function levelsValue(): unknown {
  const level = (name: keyof typeof SCENARIO_LEVEL_BUDGETS): unknown => {
    const budget = SCENARIO_LEVEL_BUDGETS[name];
    return {
      graph_template: budget.template,
      budget: {
        tokens_by_tier: {
          utility: budget.utility,
          planner: budget.planner,
          worker: budget.worker,
        },
        nodes: budget.nodes,
        rounds: budget.rounds,
        wall_time_s: budget.wall_time_s,
        concurrency: budget.concurrency,
        forge_writes: budget.forge_writes,
        forge_writes_per_hour: budget.forge_writes_per_hour,
      },
    };
  };
  return {
    default_level: "Medium",
    token_weights: {
      input: 1,
      output: 5,
      cache_read: 0.1,
      cache_write: 1.25,
    },
    none: { daily_tokens: 500_000 },
    low: level("low"),
    medium: level("medium"),
    high: level("high"),
    max: level("max"),
    ultimate: level("ultimate"),
  };
}

export interface ActionDefinition {
  id: string;
  default: "auto" | "approve" | "deny";
  allowed: string[];
  dangerous: boolean;
  reserved: boolean;
}

export const ACTION_DEFINITIONS: ActionDefinition[] = [
  { id: "merge_pr", default: "approve", allowed: ["approve", "deny"], dangerous: true, reserved: false },
  { id: "force_push", default: "approve", allowed: ["approve", "deny"], dangerous: true, reserved: false },
  { id: "push_outside_prefix", default: "approve", allowed: ["approve", "deny"], dangerous: true, reserved: false },
  { id: "resolve_review_thread_human", default: "deny", allowed: ["approve", "deny"], dangerous: true, reserved: false },
  { id: "delete_branch", default: "deny", allowed: ["approve", "deny"], dangerous: true, reserved: true },
  { id: "publish_release", default: "deny", allowed: ["approve", "deny"], dangerous: true, reserved: true },
  { id: "change_repo_settings", default: "deny", allowed: ["approve", "deny"], dangerous: true, reserved: true },
  { id: "push_branch", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "create_pr", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "update_pr", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "upsert_status_comment", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "create_comment", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "reply_review_thread", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "resolve_review_thread", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "create_issue", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "update_issue", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "close_issue", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "add_labels", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "request_reviewers", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
  { id: "rerun_failed_jobs", default: "auto", allowed: ["auto", "approve", "deny"], dangerous: false, reserved: false },
];

function policiesValue(): unknown {
  return {
    actions: Object.fromEntries(
      ACTION_DEFINITIONS.map((action) => [action.id, action.default]),
    ),
    unlisted_action: "approve",
    paths: {
      workflows: {
        globs: [".github/workflows/**"],
        disposition: "deny",
      },
      dependency_manifests: {
        globs: [
          "package.json",
          "**/package.json",
          "Cargo.toml",
          "**/Cargo.toml",
          "pyproject.toml",
          "**/pyproject.toml",
          "requirements*.txt",
          "go.mod",
          "**/go.mod",
          "Gemfile",
          "pom.xml",
          "build.gradle*",
        ],
        disposition: "approve",
      },
      lockfiles: {
        globs: [
          "package-lock.json",
          "**/package-lock.json",
          "pnpm-lock.yaml",
          "**/pnpm-lock.yaml",
          "yarn.lock",
          "**/yarn.lock",
          "Cargo.lock",
          "**/Cargo.lock",
          "poetry.lock",
          "**/poetry.lock",
          "uv.lock",
          "**/uv.lock",
          "go.sum",
          "**/go.sum",
          "Gemfile.lock",
          "**/Gemfile.lock",
        ],
        disposition: "approve",
      },
      sensitive_files: {
        globs: [".env", ".env.*", "**/*.pem", "**/*.key", "**/id_rsa*", "**/.npmrc"],
        disposition: "deny",
      },
      repo_meta: {
        globs: [
          ".github/actions/**",
          ".github/CODEOWNERS",
          "CODEOWNERS",
          ".github/dependabot.yml",
          ".devcontainer/**",
        ],
        disposition: "approve",
      },
    },
    allow_globs: ["**"],
    escalation: {
      enabled: true,
      rules: {
        sensitive_paths: {
          when: { paths: ["**/auth/**", "**/security/**", "**/crypto/**", "**/migrations/**"] },
          raise_to: "Max",
        },
        large_diff: {
          when: { diff: { files_gte: 30, lines_gte: 1000 } },
          raise_to: "High",
        },
        risk_labels: {
          when: { labels: ["security", "breaking-change", "risk:high"] },
          raise_to: "Max",
        },
      },
    },
    approval: { ttl_s: 86_400 },
    patch: {
      max_files: 200,
      max_bytes: 5_000_000,
      max_changed_lines: 10_000,
      max_file_bytes: 1_000_000,
    },
  };
}

function guardianValue(): unknown {
  return {
    triage: {
      ci_failure_in_scope: "silent_fix",
      bot_finding_valid: "silent_fix",
      bot_finding_invalid: "silent_reply",
      nit: "batch",
      mechanical_conflict: "escalate",
    },
    no_progress: {
      same_class_repeats: 2,
      stalled_rounds: 2,
    },
    status_checklist: {
      enabled: true,
      min_update_interval_s: 30,
    },
    patrol_interval_s: 1_800,
  };
}

function sandboxValue(): unknown {
  return {
    driver: "podman",
    runtime: "runsc",
    base_image:
      "ghcr.io/lexwdex/gantry-worker@sha256:5b1e68b8a4c9c3f0ed9a0c6f2b8d7e4a1f3c5b7d9e1a2c4b6d8f0a1c3e5b7d9f",
    limits: {
      cpu_cores: 2,
      mem_mb: 4_096,
      disk_mb: 10_240,
      pids: 512,
      wall_time_s: 3_600,
    },
    max_concurrent_sandboxes: 6,
    prewarm: { enabled: false, pool_size: 0 },
    devcontainer: { mode: "safe_subset" },
    idle_timeout_s: 300,
    kill_grace_s: 10,
    io: {
      progress: { max_line_bytes: 8_192, max_lines: 20_000, max_rate_per_s: 20 },
      artifacts: {
        result_max_bytes: 1_048_576,
        report_max_bytes: 262_144,
        proposals_max_bytes: 262_144,
        proposals_max_count: 20,
      },
      materials: { max_item_bytes: 65_536, max_total_bytes: 524_288 },
      logs_tail_bytes: 1_048_576,
    },
  };
}

function egressValue(): unknown {
  return {
    groups: EGRESS_GROUPS,
    profiles: {
      default: {
        groups: ["model_api", "npm", "pypi", "crates", "go", "maven"],
      },
      model_only: { groups: ["model_api"] },
    },
    default_profile: "default",
    injection: {
      private_npm: {
        hosts: ["npm.internal.example"],
        kind: "header",
        header: "x-api-key",
        credential: {
          secret_ref: "package.npm.internal",
          present: false,
        },
      },
    },
  };
}

function notifyValue(): unknown {
  return {
    channels: {
      desktop: { enabled: true },
      mobile_push: { enabled: false },
    },
    routes: {
      urgent: {
        kinds: [
          "approval_needed",
          "security_finding",
          "budget_exhausted",
          "escalation",
        ],
        channels: ["desktop"],
      },
      normal: {
        kinds: ["decision_needed", "spec_needed"],
        channels: ["desktop"],
      },
    },
  };
}

function githubValue(): unknown {
  return {
    app: {
      app_id: 482139,
      private_key: {
        secret_ref: "github.app.private_key",
        present: true,
      },
      webhook_secret: {
        secret_ref: "github.app.webhook_secret",
        present: true,
      },
      webhook_secret_previous: {
        secret_ref: "github.app.webhook_secret_previous",
        present: false,
      },
    },
    commit_identity: {
      name: "gantry[bot]",
      email: "9988776+gantry[bot]@users.noreply.github.com",
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
          context_window: 200_000,
        },
        "anthropic/claude-sonnet-4-5": {
          tools: true,
          reasoning_efforts: ["none", "low", "medium", "high", "max"],
          context_window: 200_000,
        },
        "anthropic/claude-sonnet-5": {
          tools: true,
          reasoning_efforts: ["low", "medium", "high", "max"],
          context_window: 400_000,
        },
        "anthropic/claude-opus-4-1": {
          tools: true,
          reasoning_efforts: ["low", "medium", "high", "max"],
          context_window: 200_000,
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
      actions: Object.fromEntries(
        ACTION_DEFINITIONS.map((action) => [
          action.id,
          {
            default: action.default,
            allowed: action.allowed,
            dangerous: action.dangerous,
            reserved: action.reserved,
          },
        ]),
      ),
    },
    guardian: {
      triage: {
        ci_failure_in_scope: {
          default: "silent_fix",
          allowed: ["silent_fix", "escalate"],
          locked: false,
        },
        bot_finding_valid: {
          default: "silent_fix",
          allowed: ["silent_fix", "escalate"],
          locked: false,
        },
        bot_finding_invalid: {
          default: "silent_reply",
          allowed: ["silent_reply", "escalate"],
          locked: false,
        },
        nit: { default: "batch", allowed: ["batch", "escalate"], locked: false },
        mechanical_conflict: {
          default: "escalate",
          allowed: ["escalate", "silent_fix"],
          locked: false,
        },
        security_finding: { locked: true, value: "escalate", allowed: [] },
        touches_protected_path: { locked: true, value: "escalate", allowed: [] },
        scope_expanding_fix: { locked: true, value: "escalate", allowed: [] },
        repeated_or_no_progress: { locked: true, value: "escalate", allowed: [] },
        budget_exhausted: { locked: true, value: "escalate", allowed: [] },
        approval_needed: { locked: true, value: "escalate", allowed: [] },
        push_after_approval: { locked: true, value: "escalate", allowed: [] },
        design_or_api_change: { locked: true, value: "escalate", allowed: [] },
        unknown: { locked: true, value: "escalate", allowed: [] },
      },
    },
    roles: {
      approver_keys: [
        "models.providers",
        "levels.*.graph_template",
        "sandbox.base_image",
        "github.*",
      ],
    },
    egress: {
      suggested_groups: Object.fromEntries(
        Object.entries(EGRESS_GROUPS).filter(([name]) => name !== "private_npm"),
      ),
      forbidden_hosts: [
        "github.com",
        "*.github.com",
        "169.254.169.254",
        "metadata.google.internal",
      ],
    },
    notify: {
      channels: {
        desktop: { available: true },
        mobile_push: { available: false, reason: "reserved" },
      },
    },
  };
}

function usage(
  input: number,
  output: number,
  cacheRead = 0,
  cacheWrite = 0,
  requests = 1,
): Attempt["usage"] {
  return {
    input_tokens: input,
    output_tokens: output,
    cache_read_tokens: cacheRead,
    cache_write_tokens: cacheWrite,
    requests,
  };
}

function attempt(
  id: string,
  nodeId: string,
  n: number,
  status: Attempt["status"],
  options: {
    sandbox?: string | null;
    started?: string;
    ended?: string | null;
    failure?: Attempt["failure_class"];
    detail?: string | null;
    used?: Attempt["usage"];
  } = {},
): Attempt {
  return {
    id,
    node_id: nodeId,
    n,
    sandbox_id: options.sandbox ?? `sbx_${id.slice(4, 18)}`,
    status,
    started_at: options.started ?? "2026-10-24T12:30:00.000Z",
    ended_at: options.ended ?? null,
    result_ref:
      status === "running" || status === "preparing"
        ? "sha256:pending"
        : `sha256:${"5c1e0a93b7d24f68e1a0c3d5b79f2e4681a3c5d7e9f0b2a4c6d8e0f1a3b5c7d9"}`,
    failure_class: options.failure ?? null,
    failure_detail: options.detail ?? null,
    usage: options.used ?? usage(0, 0, 0, 0, 0),
  };
}

function buildAttempts(): Attempt[] {
  return [
    attempt("att_gantry_5_build_1", "nod_gantry_5_build", 1, "failed", {
      started: "2026-10-24T12:20:00.000Z",
      ended: "2026-10-24T12:24:00.000Z",
      failure: "exec_failed",
      detail: "sandbox.start: image pull timeout",
      used: usage(18_400, 2_300, 5_000, 900, 4),
    }),
    attempt("att_gantry_5_build_2", "nod_gantry_5_build", 2, "running", {
      started: "2026-10-24T12:30:10.000Z",
      used: usage(41_200, 6_800, 30_500, 2_100, 9),
    }),
    attempt("att_gantry_7_sec_1", "nod_gantry_7_sec", 1, "succeeded", {
      started: "2026-10-24T12:10:00.000Z",
      ended: "2026-10-24T12:16:40.000Z",
      used: usage(58_000, 7_600, 20_000, 1_800, 11),
    }),
    attempt("att_gantry_7_perf_1", "nod_gantry_7_perf_old", 1, "failed", {
      started: "2026-10-24T12:12:00.000Z",
      ended: "2026-10-24T12:15:20.000Z",
      failure: "verdict_fail",
      detail: "review context truncated; retrying with a fresh node",
      used: usage(22_000, 3_100, 8_000, 500, 5),
    }),
    attempt("att_gantry_7_perf_2", "nod_gantry_7_perf", 1, "succeeded", {
      started: "2026-10-24T12:16:00.000Z",
      ended: "2026-10-24T12:23:30.000Z",
      used: usage(44_800, 5_900, 17_200, 1_100, 8),
    }),
    attempt("att_gantry_7_arb_1", "nod_gantry_7_arb", 1, "running", {
      started: "2026-10-24T12:25:00.000Z",
      used: usage(12_400, 1_900, 4_200, 300, 3),
    }),
    attempt("att_gantry_6_single_1", "nod_gantry_6_single", 1, "failed", {
      started: "2026-10-24T12:40:00.000Z",
      ended: "2026-10-24T12:41:12.000Z",
      failure: "timeout",
      detail: "attempt wall time exceeded while rendering the settings shell",
      used: usage(16_000, 2_200, 3_000, 500, 3),
    }),
    attempt("att_specgit_admission_1", "nod_specgit_31_admission", 1, "failed", {
      started: "2026-10-24T11:20:00.000Z",
      ended: "2026-10-24T11:22:04.000Z",
      failure: "verdict_fail",
      detail: "missing acceptance criteria",
      used: usage(9_600, 1_100, 2_500, 300, 2),
    }),
  ];
}

const DELIVERY_FOR_NODE: Record<string, string> = {
  nod_gantry_5: "dlv_gantry_5",
  nod_gantry_6: "dlv_gantry_6",
  nod_gantry_7: "dlv_gantry_7",
  nod_specgit_31: "dlv_specgit_31",
};

function deliveryForNode(nodeId: string): string | null {
  for (const [prefix, deliveryId] of Object.entries(DELIVERY_FOR_NODE)) {
    if (nodeId.startsWith(prefix)) return deliveryId;
  }
  return null;
}

function buildBudgets(
  deliveries: Delivery[],
  attempts: Attempt[],
): Record<string, BudgetLedger> {
  const result: Record<string, BudgetLedger> = {};
  for (const delivery of deliveries) {
    const level = delivery.level_cap.toLowerCase();
    const budget = SCENARIO_LEVEL_BUDGETS[level] ?? SCENARIO_LEVEL_BUDGETS.medium;
    const ownAttempts = attempts.filter(
      (entry) => deliveryForNode(entry.node_id) === delivery.id,
    );
    result[delivery.id] = {
      id: `bdg_${delivery.id}`,
      delivery_id: delivery.id,
      subject: "delivery",
      template: budget.template,
      limits: {
        tokens_by_tier: {
          utility: budget.utility,
          planner: budget.planner,
          worker: budget.worker,
        },
        nodes: budget.nodes,
        rounds: budget.rounds,
        wall_time: budget.wall_time_s,
        forge_writes: budget.forge_writes,
        concurrency: budget.concurrency,
      },
      used: {
        tokens_by_tier: {
          utility: 18_400,
          planner: 52_100,
          worker: 611_000,
        },
        nodes: delivery.stage === "closed" ? 1 : 2,
        attempts: ownAttempts.length,
        rounds: delivery.stage === "review" ? 1 : 0,
        wall_time: ownAttempts.reduce((sum, entry) => {
          if (!entry.ended_at) return sum;
          return sum + Math.max(0, (Date.parse(entry.ended_at) - Date.parse(entry.started_at)) / 1000);
        }, 0),
        forge_writes: delivery.pr ? 3 : 0,
      },
    };
  }
  return result;
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
