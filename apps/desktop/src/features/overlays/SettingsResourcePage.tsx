/**
 * 服务端设置资源页。
 *
 * 字段来自 docs/interfaces/config.md §4–§10 的文字契约（手工转写，不是生成代码）；
 * 枚举选项尽量取 settings.catalog。写入仍走外壳的 guardedWrite，开发场景下
 * 由 ScenarioIpcAdapter 只改内存并回读，不产生真实签名、凭据或网络写入。
 */
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import type {
  SettingsCatalog,
  SettingsDocument,
  SettingsIssue,
  SettingsStatus,
} from "@/api/types";
import { isScenarioAdapter } from "@/ipc";
import { useGuardedWrite } from "./useGuardedWrite";
import "./settings-resources.css";
import {
  asRecord,
  asString,
  etagFrom,
  ipcErrorText,
  issueText,
  parseApiFailure,
} from "./settingsData";
import {
  BUDGET_LEVELS,
  FieldGrid,
  FieldIssues,
  INBOX_KINDS,
  LEVELS,
  RecordTable,
  SecretRefEditor,
  collectRequiredIssues,
  getAtPath,
  injectSecrets,
  issuesAtPath,
  issuesUnderPath,
  normalizeForCompare,
  pathPointer,
  secretKey,
  setAtPath,
  type FieldSpec,
  type Path,
  type TableSpec,
} from "./settingsFields";

export type SettingsResourcePageId =
  | "levels"
  | "github"
  | "sandbox"
  | "policies"
  | "guardian"
  | "notify";

export interface SettingsResourcePageProps {
  controller: DesktopController;
  state: DesktopState;
  page: SettingsResourcePageId;
  onDirtyChange: (dirty: boolean) => void;
}

type SettingsResource =
  | "levels"
  | "github"
  | "sandbox"
  | "egress"
  | "policies"
  | "guardian"
  | "notify";

type Which = "primary" | "secondary";

interface ResourceDoc {
  doc: SettingsDocument;
  etag: string;
}

interface LoadData {
  primary: ResourceDoc;
  secondary: ResourceDoc | null;
  catalog: SettingsCatalog;
  models: SettingsDocument | null;
  status: SettingsStatus | null;
  statusError: string | null;
}

interface LoadState {
  status: "loading" | "ready" | "error";
  data: LoadData | null;
  error: string | null;
}

interface Draft {
  value: Record<string, unknown>;
  baseline: string;
  secrets: Record<string, string>;
  secretSpent: boolean;
  needsReentry: boolean;
}

interface ValidationContext {
  primary: Record<string, unknown>;
  secondary: Record<string, unknown> | null;
  catalog: SettingsCatalog;
  models: SettingsDocument | null;
  state: DesktopState;
}

interface PageDefinition {
  title: string;
  primaryResource: SettingsResource;
  secondaryResource: SettingsResource | null;
  primaryLabel: string;
  secondaryLabel: string | null;
  modules: string[];
  loadModels: boolean;
  buildPrimary: (ctx: ValidationContext) => SectionSpec[];
  buildSecondary: (ctx: ValidationContext) => SectionSpec[];
  validatePrimary: (ctx: ValidationContext) => SettingsIssue[];
  validateSecondary: (ctx: ValidationContext) => SettingsIssue[];
}

type SectionSpec =
  | { kind: "fields"; title: string; note?: string; fields: FieldSpec[] }
  | { kind: "table"; table: TableSpec }
  | { kind: "budget"; title: string; note?: string }
  | { kind: "actions"; title: string; note?: string }
  | { kind: "triage"; title: string; note?: string }
  | { kind: "secret"; title: string; path: Path; label: string; required?: boolean; removable?: boolean }
  | { kind: "provider-routes"; title: string; note?: string }
  | { kind: "installations"; title: string; note?: string }
  | { kind: "note"; title: string; body: string };

const PAGE_RESOURCES: Record<
  SettingsResourcePageId,
  {
    primary: SettingsResource;
    secondary: SettingsResource | null;
    modules: string[];
    loadModels: boolean;
  }
> = {
  levels: {
    primary: "levels",
    secondary: "policies",
    modules: ["engine"],
    loadModels: false,
  },
  github: {
    primary: "github",
    secondary: null,
    modules: ["forge"],
    loadModels: false,
  },
  sandbox: {
    primary: "sandbox",
    secondary: "egress",
    modules: ["sandbox", "egress"],
    loadModels: true,
  },
  policies: {
    primary: "policies",
    secondary: null,
    modules: ["policy"],
    loadModels: false,
  },
  guardian: {
    primary: "guardian",
    secondary: null,
    modules: ["guardian"],
    loadModels: false,
  },
  notify: {
    primary: "notify",
    secondary: null,
    modules: ["notify"],
    loadModels: false,
  },
};

const SCENARIO_NOTE =
  "开发场景：保存只写入内存设置，提交后立即回读；不产生真实签名、凭据、证书或网络写入。生产保存仍走外壳的 dry-run，并在需要时发起审批签名。";

const PATCH_KEYS = ["max_files", "max_bytes", "max_changed_lines", "max_file_bytes"] as const;

const TOKEN_WEIGHTS = ["input", "output", "cache_read", "cache_write"] as const;

const IO_FIELDS: Array<{ path: Path; label: string; kind: "integer" | "number" }> = [
  { path: ["io", "progress", "max_line_bytes"], label: "进度单行上限", kind: "integer" },
  { path: ["io", "progress", "max_lines"], label: "进度行数上限", kind: "integer" },
  { path: ["io", "progress", "max_rate_per_s"], label: "进度速率上限", kind: "integer" },
  { path: ["io", "artifacts", "result_max_bytes"], label: "result 产物上限", kind: "integer" },
  { path: ["io", "artifacts", "report_max_bytes"], label: "report 产物上限", kind: "integer" },
  { path: ["io", "artifacts", "proposals_max_bytes"], label: "proposals 上限", kind: "integer" },
  { path: ["io", "artifacts", "proposals_max_count"], label: "proposals 条数上限", kind: "integer" },
  { path: ["io", "materials", "max_item_bytes"], label: "单条材料上限", kind: "integer" },
  { path: ["io", "materials", "max_total_bytes"], label: "材料总量上限", kind: "integer" },
  { path: ["io", "logs_tail_bytes"], label: "日志尾部上限", kind: "integer" },
];

const LIMIT_FIELDS: Array<{ path: Path; label: string; kind: "integer" | "number" }> = [
  { path: ["limits", "cpu_cores"], label: "CPU 核数", kind: "number" },
  { path: ["limits", "mem_mb"], label: "内存", kind: "integer" },
  { path: ["limits", "disk_mb"], label: "磁盘", kind: "integer" },
  { path: ["limits", "pids"], label: "进程数", kind: "integer" },
  { path: ["limits", "wall_time_s"], label: "墙钟上限", kind: "integer" },
];

function definitionFor(
  page: SettingsResourcePageId,
  ctx: ValidationContext,
): PageDefinition {
  switch (page) {
    case "levels":
      return levelsDefinition(ctx);
    case "github":
      return githubDefinition();
    case "sandbox":
      return sandboxDefinition(ctx);
    case "policies":
      return policiesDefinition(ctx);
    case "guardian":
      return guardianDefinition(ctx);
    case "notify":
      return notifyDefinition();
  }
}

function levelsDefinition(ctx: ValidationContext): PageDefinition {
  return {
    title: "档位与预算",
    primaryResource: "levels",
    secondaryResource: "policies",
    primaryLabel: "档位与预算",
    secondaryLabel: "自动加码",
    modules: ["engine"],
    loadModels: false,
    buildPrimary: () => [
      {
        kind: "fields",
        title: "默认档位与 None 档",
        note: "default_level 是任务默认上限；单个任务仍可在消息里指定。None 档不建交付单元，只受每日 token 上限约束。",
        fields: [
          {
            path: ["default_level"],
            label: "默认档位上限",
            kind: "enum",
            options: LEVELS,
            required: true,
          },
          {
            path: ["none", "daily_tokens"],
            label: "None 档每日 token 上限",
            kind: "integer",
            min: 1,
            required: true,
            help: "加权 token，按 UTC 日统计；项目只能收紧。",
          },
        ],
      },
      {
        kind: "fields",
        title: "token 加权",
        note: "一次调用的加权 token = Σ 权重 × 该类别 token 数；预算与台账都按加权值计（config.md §4）。仅全局。",
        fields: TOKEN_WEIGHTS.map((key) => ({
          path: ["token_weights", key],
          label: `权重 ${key}`,
          kind: "number" as const,
          min: 0.000001,
          required: true,
        })),
      },
      {
        kind: "budget",
        title: "各档位图模板与预算",
        note: "模板选项来自 settings.catalog；所有预算必须有限且为正整数，max 不超过运行时上限。预算在交付单元创建时按 level_cap 固定。",
      },
    ],
    buildSecondary: () => [
      {
        kind: "fields",
        title: "自动加码",
        note: "只在档位上限内抬高 level_effective；外部文本触发的规则只能提高审视。求值由 engine 执行。",
        fields: [
          {
            path: ["escalation", "enabled"],
            label: "启用自动加码",
            kind: "boolean",
          },
        ],
      },
      {
        kind: "table",
        table: escalationTable(),
      },
    ],
    validatePrimary: (context) => validateLevels(context),
    validateSecondary: (context) => validateEscalation(context.secondary ?? {}),
  };
}

function githubDefinition(): PageDefinition {
  return {
    title: "GitHub App",
    primaryResource: "github",
    secondaryResource: null,
    primaryLabel: "GitHub 设置",
    secondaryLabel: null,
    modules: ["forge"],
    loadModels: false,
    buildPrimary: () => [
      {
        kind: "fields",
        title: "App 标识与提交身份",
        note: "commit_identity 决定 Gantry 创建提交时的作者与提交者。改它会改变提交归属，属于仅 approver 键。",
        fields: [
          {
            path: ["app", "app_id"],
            label: "App ID",
            kind: "integer",
            min: 1,
            required: true,
          },
          {
            path: ["commit_identity", "name"],
            label: "提交身份名称",
            kind: "text",
            required: true,
          },
          {
            path: ["commit_identity", "email"],
            label: "提交身份邮箱",
            kind: "text",
            required: true,
          },
        ],
      },
      {
        kind: "secret",
        title: "App 私钥",
        path: ["app", "private_key"],
        label: "App 私钥",
        required: true,
      },
      {
        kind: "secret",
        title: "Webhook secret",
        path: ["app", "webhook_secret"],
        label: "Webhook secret",
        required: true,
      },
      {
        kind: "secret",
        title: "上一个 Webhook secret",
        path: ["app", "webhook_secret_previous"],
        label: "上一个 Webhook secret",
        removable: true,
      },
      {
        kind: "installations",
        title: "项目安装状态",
        note: "installation_id 与 bot_user_id 是 Project 字段，不是设置键；本页只读展示。安装引导由 gantry github setup 提供，本页不创建安装。",
      },
    ],
    buildSecondary: () => [],
    validatePrimary: (context) => validateGithub(context.primary),
    validateSecondary: () => [],
  };
}

function sandboxDefinition(ctx: ValidationContext): PageDefinition {
  return {
    title: "沙盒与出网",
    primaryResource: "sandbox",
    secondaryResource: "egress",
    primaryLabel: "沙盒",
    secondaryLabel: "出网",
    modules: ["sandbox", "egress"],
    loadModels: true,
    buildPrimary: () => [
      {
        kind: "fields",
        title: "驱动与镜像",
        note: "driver 第一版只支持 podman；runtime 从 runsc 降级到 runc 是放宽。base_image 必须以 @sha256 固定摘要。",
        fields: [
          {
            path: ["driver"],
            label: "沙盒驱动",
            kind: "text",
            readOnly: true,
            readOnlyText: "podman（第一版唯一实现）",
          },
          {
            path: ["runtime"],
            label: "运行时",
            kind: "enum",
            options: ["runsc", "runc"],
            required: true,
          },
          {
            path: ["base_image"],
            label: "基础镜像",
            kind: "text",
            required: true,
            help: "必须包含 @sha256:<64 位十六进制>。",
          },
          {
            path: ["max_concurrent_sandboxes"],
            label: "并发布盒上限",
            kind: "integer",
            min: 1,
            required: true,
          },
          {
            path: ["prewarm", "enabled"],
            label: "预热池",
            kind: "boolean",
          },
          {
            path: ["prewarm", "pool_size"],
            label: "预热池大小",
            kind: "integer",
            min: 0,
          },
          {
            path: ["devcontainer", "mode"],
            label: "devcontainer 模式",
            kind: "enum",
            options: ["off", "safe_subset"],
            required: true,
          },
          {
            path: ["idle_timeout_s"],
            label: "空闲超时",
            kind: "integer",
            min: 1,
            unit: "秒",
            required: true,
          },
          {
            path: ["kill_grace_s"],
            label: "终止宽限",
            kind: "integer",
            min: 1,
            unit: "秒",
            required: true,
          },
        ],
      },
      {
        kind: "fields",
        title: "默认资源限制",
        note: "limits 固化到 Sandbox.limits；项目只能收紧。",
        fields: LIMIT_FIELDS.map((field) => ({
          path: field.path,
          label: field.label,
          kind: field.kind,
          min: field.kind === "number" ? 0.1 : 1,
          required: true,
          unit: field.kind === "number" ? undefined : field.path[1] === "wall_time_s" ? "秒" : undefined,
        })),
      },
      {
        kind: "fields",
        title: "worker 协议 I/O 上限",
        note: "来自 config.md §7 io.*；非法值会让 worker 进度流或产物被截断。",
        fields: IO_FIELDS.map((field) => ({
          path: field.path,
          label: field.label,
          kind: field.kind,
          min: 1,
          required: true,
          unit: "字节",
        })),
      },
    ],
    buildSecondary: (context) => {
      const groups = asRecord(context.secondary?.groups) ?? {};
      const profiles = asRecord(context.secondary?.profiles) ?? {};
      const groupNames = Object.keys(groups);
      const profileNames = Object.keys(profiles);
      return [
        {
          kind: "fields",
          title: "默认出网档案",
          note: "沙盒网络只经 Egress；白名单之外的连接一律拒绝。model_api 分组是模型 API 的默认落点。",
          fields: [
            {
              path: ["default_profile"],
              label: "默认档案",
              kind: "enum",
              options: profileNames,
              required: true,
            },
          ],
        },
        {
          kind: "table",
          table: {
            path: ["groups"],
            title: "出网分组",
            entryLabel: "分组名",
            addLabel: "新增分组",
            description:
              "hosts 是 DNS 主机名，允许一层 *.example.com 前缀通配。GitHub 域名族、IP 字面量与裸 * 会被拒绝。",
            createEntry: () => ({ hosts: [] }),
            columns: [
              {
                key: "hosts",
                label: "hosts",
                kind: "lines",
                placeholder: "每行一个主机",
                required: true,
              },
            ],
          },
        },
        {
          kind: "table",
          table: {
            path: ["profiles"],
            title: "出网档案",
            entryLabel: "档案名",
            addLabel: "新增档案",
            description: "档案是分组集合；项目只能收窄。",
            createEntry: () => ({ groups: [] }),
            columns: [
              {
                key: "groups",
                label: "分组",
                kind: "enum-list",
                options: groupNames,
              },
            ],
          },
        },
        {
          kind: "table",
          table: {
            path: ["injection"],
            title: "私有包仓库注入",
            entryLabel: "注入项",
            addLabel: "新增注入",
            description:
              "只放私有包仓库；模型 API 路由由 models.providers 自动派生，不在这里重复声明。凭据只写不读。",
            createEntry: () => ({
              hosts: [],
              kind: "bearer",
              credential: { secret_ref: "", present: false },
            }),
            columns: [
              {
                key: "hosts",
                label: "hosts",
                kind: "lines",
                placeholder: "每行一个主机",
                required: true,
              },
              {
                key: "kind",
                label: "类型",
                kind: "enum",
                options: ["bearer", "header"],
                required: true,
              },
              {
                key: "header",
                label: "请求头（header 类型）",
                kind: "text",
                placeholder: "例如 x-api-key",
              },
              {
                key: "credential",
                label: "凭据",
                kind: "secret-ref",
                required: true,
              },
            ],
          },
        },
        {
          kind: "provider-routes",
          title: "模型 API 反向代理路由（只读）",
          note: "由 models.providers 的 base_url 与 credential.secret_ref 自动派生；凭据只由 Egress 注入，沙盒内不可见。",
        },
      ];
    },
    validatePrimary: (context) => validateSandbox(context.primary),
    validateSecondary: (context) => validateEgress(context.secondary ?? {}, context.catalog),
  };
}

function policiesDefinition(ctx: ValidationContext): PageDefinition {
  return {
    title: "危险动作策略",
    primaryResource: "policies",
    secondaryResource: null,
    primaryLabel: "危险动作策略",
    secondaryLabel: null,
    modules: ["policy"],
    loadModels: false,
    buildPrimary: (context) => [
      {
        kind: "actions",
        title: "动作处置",
        note: "动作与允许值来自 settings.catalog。危险动作不能自动放行；workflow 文件的处置固定在路径规则里。",
      },
      {
        kind: "table",
        table: {
          path: ["paths"],
          title: "路径规则",
          entryLabel: "规则 id",
          addLabel: "新增路径规则",
          fixedKeys: ["workflows", "sensitive_files"],
          description:
            "匹配补丁与推送涉及的仓库相对路径；多条命中取最严。workflows 与 sensitive_files 必须保持 deny。",
          createEntry: () => ({ globs: [], disposition: "approve" }),
          columns: [
            {
              key: "globs",
              label: "globs",
              kind: "lines",
              placeholder: "每行一项",
              required: true,
            },
            {
              key: "disposition",
              label: "处置",
              kind: "enum",
              options: ["approve", "deny"],
              required: true,
              lockedValue: (key) =>
                key === "workflows" || key === "sensitive_files" ? "deny" : undefined,
              lockedText: () => "deny（不可放宽）",
            },
          ],
        },
      },
      {
        kind: "fields",
        title: "补丁放行范围",
        fields: [
          {
            path: ["allow_globs"],
            label: "allow_globs",
            kind: "lines",
            required: true,
            help: "每行一个 gitignore 风格 glob；仅全局。",
          },
        ],
      },
      {
        kind: "fields",
        title: "审批有效期",
        fields: [
          {
            path: ["approval", "ttl_s"],
            label: "审批有效期",
            kind: "integer",
            min: 60,
            max: 604_800,
            unit: "秒",
            required: true,
          },
          {
            path: ["unlisted_action"],
            label: "未列动作兜底",
            kind: "enum",
            options: ["approve", "deny"],
            required: true,
          },
        ],
      },
      {
        kind: "fields",
        title: "补丁大小上限",
        fields: PATCH_KEYS.map((key) => ({
          path: ["patch", key],
          label: key,
          kind: "integer" as const,
          min: 1,
          required: true,
        })),
      },
      {
        kind: "note",
        title: "自动加码",
        body: "policies.escalation 在「档位与预算」页编辑；它只影响档位抬高，不影响本页动作处置。",
      },
    ],
    buildSecondary: () => [],
    validatePrimary: (context) => validatePolicies(context.primary, context.catalog),
    validateSecondary: () => [],
  };
}

function guardianDefinition(ctx: ValidationContext): PageDefinition {
  return {
    title: "后台守护",
    primaryResource: "guardian",
    secondaryResource: null,
    primaryLabel: "守护设置",
    secondaryLabel: null,
    modules: ["guardian"],
    loadModels: false,
    buildPrimary: () => [
      {
        kind: "triage",
        title: "分流覆盖",
        note: "类 id 与允许值来自 settings.catalog。锁定类不可配置，只读展示 escalate。",
      },
      {
        kind: "fields",
        title: "无进展阈值",
        fields: [
          {
            path: ["no_progress", "same_class_repeats"],
            label: "同类问题重复次数",
            kind: "integer",
            min: 1,
            required: true,
          },
          {
            path: ["no_progress", "stalled_rounds"],
            label: "连续无进展轮数",
            kind: "integer",
            min: 1,
            required: true,
          },
        ],
      },
      {
        kind: "fields",
        title: "状态清单",
        note: "关闭只影响 PR 上的可见性，不影响收件箱与事件日志审计。",
        fields: [
          {
            path: ["status_checklist", "enabled"],
            label: "维护状态清单",
            kind: "boolean",
          },
          {
            path: ["status_checklist", "min_update_interval_s"],
            label: "最小更新间隔",
            kind: "integer",
            min: 0,
            unit: "秒",
            required: true,
          },
        ],
      },
      {
        kind: "fields",
        title: "对账巡检",
        note: "低频对账巡检补齐丢失或乱序的 webhook；绝不轮询。",
        fields: [
          {
            path: ["patrol_interval_s"],
            label: "巡检周期",
            kind: "integer",
            min: 300,
            unit: "秒",
            required: true,
          },
        ],
      },
    ],
    buildSecondary: () => [],
    validatePrimary: (context) => validateGuardian(context.primary, context.catalog),
    validateSecondary: () => [],
  };
}

function notifyDefinition(): PageDefinition {
  return {
    title: "通知",
    primaryResource: "notify",
    secondaryResource: null,
    primaryLabel: "通知设置",
    secondaryLabel: null,
    modules: ["notify"],
    loadModels: false,
    buildPrimary: () => [
      {
        kind: "fields",
        title: "服务端渠道",
        note: "收件箱写入始终发生，不可关闭；这里只决定是否推送到渠道。桌面系统通知还需要本机「外观」页的开关，两层都开才会弹窗。",
        fields: [
          {
            path: ["channels", "desktop", "enabled"],
            label: "桌面通道",
            kind: "boolean",
            help: "服务端是否把事项推给桌面通道；不代表本机弹窗。",
          },
          {
            path: ["channels", "mobile_push", "enabled"],
            label: "手机推送（保留）",
            kind: "text",
            readOnly: true,
            readOnlyText: "保留渠道，第一版不可用；schema 暂不接受启用。",
          },
        ],
      },
      {
        kind: "table",
        table: {
          path: ["routes"],
          title: "通知路由",
          entryLabel: "路由名",
          addLabel: "新增路由",
          description:
            "每条路由选择上报类型和渠道。第一版只支持 desktop；手机推送保留。urgent 类至少要有可达的桌面渠道。",
          createEntry: () => ({ kinds: [], channels: ["desktop"] }),
          columns: [
            {
              key: "kinds",
              label: "上报类型",
              kind: "enum-list",
              options: INBOX_KINDS,
            },
            {
              key: "channels",
              label: "渠道",
              kind: "enum-list",
              options: ["desktop"],
            },
          ],
        },
      },
      {
        kind: "note",
        title: "模拟边界",
        body: "模拟保存不会触发任何系统通知，也不代表 Tauri 原生通知已接入。收件箱写入和审计不受本页开关影响。",
      },
    ],
    buildSecondary: () => [],
    validatePrimary: (context) => validateNotify(context.primary),
    validateSecondary: () => [],
  };
}

function escalationTable(): TableSpec {
  return {
    path: ["escalation", "rules"],
    title: "自动加码规则",
    entryLabel: "规则 id",
    addLabel: "新增规则",
    description:
      "when 恰好一种条件；raise_to 取 Medium / High / Max。外部文本只能提高审视。",
    createEntry: () => ({ when: { paths: [] }, raise_to: "High" }),
    columns: [
      { key: "when", label: "when", kind: "when" },
      {
        key: "raise_to",
        label: "raise_to",
        kind: "enum",
        options: ["Medium", "High", "Max"],
        required: true,
      },
    ],
  };
}

function validateLevels(ctx: ValidationContext): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const root = ctx.primary;
  const defaultLevel = root.default_level;
  if (typeof defaultLevel !== "string" || !LEVELS.includes(defaultLevel)) {
    issues.push(issue("/default_level", "enum", "默认档位必须是合法 Level。"));
  }
  const weights = asRecord(root.token_weights);
  for (const key of TOKEN_WEIGHTS) {
    if (!weights || !isPositiveNumber(weights[key])) {
      issues.push(
        issue(`/token_weights/${key}`, "token_weights_invalid", "权重必须是正数。"),
      );
    }
  }
  const none = asRecord(root.none);
  if (!none || !isPositiveInteger(none.daily_tokens)) {
    issues.push(
      issue("/none/daily_tokens", "range", "None 档日上限必须是正整数。"),
    );
  }
  const templates = ctx.catalog.levels?.graph_templates ?? {};
  for (const level of BUDGET_LEVELS) {
    const entry = asRecord(root[level]);
    if (!entry) {
      issues.push(issue(`/${level}`, "required", `${level} 档位条目缺失。`));
      continue;
    }
    const template = entry.graph_template;
    if (typeof template !== "string" || !template) {
      issues.push(issue(`/${level}/graph_template`, "required", "必须选择图模板。"));
    } else if (
      Array.isArray(templates[level]) &&
      templates[level].length > 0 &&
      !templates[level].includes(template)
    ) {
      issues.push(
        issue(
          `/${level}/graph_template`,
          "template_unknown",
          `模板 ${template} 不在目录中，或与档位不匹配。`,
        ),
      );
    }
    const budget = asRecord(entry.budget);
    if (!budget) {
      issues.push(issue(`/${level}/budget`, "required", "预算字段缺失。"));
      continue;
    }
    const tiers = asRecord(budget.tokens_by_tier);
    for (const tier of ["utility", "planner", "worker"] as const) {
      if (!tiers || !isPositiveInteger(tiers[tier])) {
        issues.push(
          issue(
            `/${level}/budget/tokens_by_tier/${tier}`,
            "budget_not_finite",
            "预算必须是正整数。",
          ),
        );
      }
    }
    for (const key of [
      "nodes",
      "rounds",
      "wall_time_s",
      "concurrency",
      "forge_writes",
      "forge_writes_per_hour",
    ]) {
      if (!isPositiveInteger(budget[key])) {
        issues.push(
          issue(
            `/${level}/budget/${key}`,
            "budget_not_finite",
            "预算必须是正整数。",
          ),
        );
      }
    }
  }
  return issues;
}

function validateEscalation(value: Record<string, unknown>): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const escalation = asRecord(value.escalation);
  if (!escalation) {
    issues.push(issue("/escalation", "required", "缺少自动加码配置。"));
    return issues;
  }
  if (typeof escalation.enabled !== "boolean") {
    issues.push(issue("/escalation/enabled", "type", "启用开关必须是布尔值。"));
  }
  const rules = asRecord(escalation.rules);
  if (!rules) {
    issues.push(issue("/escalation/rules", "type", "规则必须是对象。"));
    return issues;
  }
  for (const [id, rawRule] of Object.entries(rules)) {
    const rule = asRecord(rawRule);
    const pointer = `/escalation/rules/${id}`;
    if (!rule) {
      issues.push(issue(pointer, "type", "规则必须是对象。"));
      continue;
    }
    if (!["Medium", "High", "Max"].includes(asString(rule.raise_to))) {
      issues.push(issue(`${pointer}/raise_to`, "enum", "raise_to 不在允许范围。"));
    }
    const when = asRecord(rule.when);
    const keys = when ? Object.keys(when) : [];
    if (keys.length !== 1 || !["paths", "diff", "labels"].includes(keys[0])) {
      issues.push(
        issue(`${pointer}/when`, "escalation_when_invalid", "when 必须恰好一种条件。"),
      );
      continue;
    }
    if (keys[0] === "diff") {
      const diff = asRecord(when?.diff);
      if (!diff) {
        issues.push(
          issue(`${pointer}/when/diff`, "escalation_when_invalid", "diff 条件缺失。"),
        );
        continue;
      }
      const values = [diff.files_gte, diff.lines_gte].filter(
        (entry) => entry !== undefined,
      );
      if (
        values.length === 0 ||
        values.some((entry) => !isPositiveInteger(entry))
      ) {
        issues.push(
          issue(
            `${pointer}/when/diff`,
            "escalation_when_invalid",
            "diff 阈值必须是正整数。",
          ),
        );
      }
    } else {
      const list = when?.[keys[0]];
      if (!Array.isArray(list) || list.length === 0) {
        issues.push(
          issue(`${pointer}/when/${keys[0]}`, "escalation_when_invalid", "条件列表不能为空。"),
        );
      }
    }
  }
  return issues;
}

function validateGithub(value: Record<string, unknown>): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const app = asRecord(value.app);
  if (!app) {
    issues.push(issue("/app", "required", "缺少 App 配置。"));
  } else {
    if (!isPositiveInteger(app.app_id)) {
      issues.push(issue("/app/app_id", "range", "App ID 必须是正整数。"));
    }
    for (const key of ["private_key", "webhook_secret"] as const) {
      const ref = asRecord(app[key]);
      if (!ref || typeof ref.secret_ref !== "string" || !ref.secret_ref.trim()) {
        issues.push(issue(`/app/${key}`, "secret_ref_missing", "缺少 secret_ref。"));
      }
    }
    const previous = asRecord(app.webhook_secret_previous);
    if (previous && (typeof previous.secret_ref !== "string" || !previous.secret_ref.trim())) {
      issues.push(
        issue(
          "/app/webhook_secret_previous",
          "secret_ref_missing",
          "上一个 webhook secret 需要 secret_ref，或移除该键。",
        ),
      );
    }
  }
  const identity = asRecord(value.commit_identity);
  if (!identity || !asString(identity.name).trim()) {
    issues.push(issue("/commit_identity/name", "required", "提交身份名称必填。"));
  }
  const email = asString(identity?.email).trim();
  if (!email) {
    issues.push(issue("/commit_identity/email", "required", "提交身份邮箱必填。"));
  } else if (!email.includes("@")) {
    issues.push(issue("/commit_identity/email", "type", "提交身份邮箱格式不正确。"));
  }
  return issues;
}

function validateSandbox(value: Record<string, unknown>): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  if (!["runsc", "runc"].includes(asString(value.runtime))) {
    issues.push(issue("/runtime", "enum", "运行时只允许 runsc 或 runc。"));
  }
  const image = asString(value.base_image);
  if (!image) {
    issues.push(issue("/base_image", "required", "基础镜像必填。"));
  } else if (!/@sha256:[0-9a-f]{64}$/i.test(image)) {
    issues.push(
      issue("/base_image", "image_not_pinned", "基础镜像必须以 @sha256 固定摘要。"),
    );
  }
  if (!isPositiveInteger(value.max_concurrent_sandboxes)) {
    issues.push(issue("/max_concurrent_sandboxes", "range", "并发布盒数必须是正整数。"));
  }
  const limits = asRecord(value.limits);
  for (const field of LIMIT_FIELDS) {
    const current = getAtPath(limits, field.path.slice(1));
    if (!isPositiveNumber(current)) {
      issues.push(
        issue(pathPointer(field.path), "range", `${field.label}必须是正数。`),
      );
    }
  }
  const io = asRecord(value.io);
  for (const field of IO_FIELDS) {
    const current = getAtPath(io, field.path.slice(1));
    if (!isPositiveInteger(current)) {
      issues.push(
        issue(pathPointer(field.path), "range", `${field.label}必须是正整数。`),
      );
    }
  }
  const prewarm = asRecord(value.prewarm);
  if (!prewarm || !Number.isInteger(prewarm.pool_size) || Number(prewarm.pool_size) < 0) {
    issues.push(issue("/prewarm/pool_size", "range", "预热池大小必须是非负整数。"));
  }
  if (!["off", "safe_subset"].includes(asString(asRecord(value.devcontainer)?.mode))) {
    issues.push(issue("/devcontainer/mode", "enum", "devcontainer 模式不在允许范围。"));
  }
  for (const key of ["idle_timeout_s", "kill_grace_s"]) {
    if (!isPositiveInteger(value[key])) {
      issues.push(issue(`/${key}`, "range", "必须是正整数秒。"));
    }
  }
  return issues;
}

function validateEgress(
  value: Record<string, unknown>,
  catalog: SettingsCatalog,
): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const groups = asRecord(value.groups) ?? {};
  const forbidden = catalog.egress?.forbidden_hosts ?? [
    "github.com",
    "*.github.com",
    "metadata.google.internal",
  ];
  const allHosts = new Set<string>();
  for (const [name, rawGroup] of Object.entries(groups)) {
    const group = asRecord(rawGroup);
    const hosts = Array.isArray(group?.hosts) ? group.hosts : null;
    const pointer = `/groups/${name}/hosts`;
    if (!hosts || hosts.length === 0) {
      issues.push(issue(pointer, "required", "分组至少需要一个主机。"));
      continue;
    }
    for (const host of hosts) {
      if (typeof host !== "string" || !host.trim()) {
        issues.push(issue(pointer, "type", "主机必须是非空字符串。"));
        continue;
      }
      allHosts.add(host);
      if (isForbiddenHost(host, forbidden)) {
        issues.push(
          issue(
            pointer,
            "egress_forbidden_host",
            `主机 ${host} 属于硬性拒绝范围（GitHub 域名族、IP、裸通配或内网）。`,
          ),
        );
      }
    }
  }
  const profiles = asRecord(value.profiles) ?? {};
  for (const [name, rawProfile] of Object.entries(profiles)) {
    const profile = asRecord(rawProfile);
    const groupsList = Array.isArray(profile?.groups) ? profile.groups : [];
    if (groupsList.length === 0) {
      issues.push(issue(`/profiles/${name}/groups`, "required", "档案至少选择一个分组。"));
      continue;
    }
    for (const group of groupsList) {
      if (typeof group !== "string" || !Object.hasOwn(groups, group)) {
        issues.push(
          issue(
            `/profiles/${name}/groups`,
            "egress_forbidden_host",
            `档案引用了不存在的分组 ${String(group)}。`,
          ),
        );
      }
    }
  }
  const defaultProfile = asString(value.default_profile);
  if (!defaultProfile || !Object.hasOwn(profiles, defaultProfile)) {
    issues.push(
      issue("/default_profile", "enum", "默认档案必须引用已有档案。"),
    );
  }
  const injection = asRecord(value.injection) ?? {};
  const seen = new Set<string>();
  for (const [id, rawItem] of Object.entries(injection)) {
    const item = asRecord(rawItem);
    const pointer = `/injection/${id}`;
    if (!item) {
      issues.push(issue(pointer, "type", "注入项必须是对象。"));
      continue;
    }
    if (!Array.isArray(item.hosts) || item.hosts.length === 0) {
      issues.push(issue(`${pointer}/hosts`, "required", "注入至少需要一个主机。"));
    } else {
      for (const host of item.hosts) {
        if (typeof host !== "string" || !allHosts.has(host)) {
          issues.push(
            issue(
              `${pointer}/hosts`,
              "injection_host_not_in_group",
              "注入主机必须属于某个出网分组。",
            ),
          );
        }
      }
    }
    if (!["bearer", "header"].includes(asString(item.kind))) {
      issues.push(issue(`${pointer}/kind`, "enum", "注入类型只允许 bearer 或 header。"));
    }
    const header = asString(item.header).toLowerCase();
    if (header && ["host", "content-length", "transfer-encoding"].includes(header)) {
      issues.push(issue(`${pointer}/header`, "injection_conflict", "不能使用逐跳头。"));
    }
    const credential = asRecord(item.credential);
    if (!credential || !asString(credential.secret_ref).trim()) {
      issues.push(
        issue(`${pointer}/credential/secret_ref`, "secret_ref_missing", "缺少 secret_ref。"),
      );
    }
    for (const host of Array.isArray(item.hosts) ? item.hosts : []) {
      if (typeof host !== "string") continue;
      const key = `${host.toLowerCase()}:${header || "authorization"}`;
      if (seen.has(key)) {
        issues.push(
          issue(`${pointer}/hosts`, "injection_conflict", "同一主机与请求头只能配置一个注入。"),
        );
      }
      seen.add(key);
    }
  }
  return issues;
}

function validatePolicies(
  value: Record<string, unknown>,
  catalog: SettingsCatalog,
): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const definitions = catalog.policies?.actions ?? {};
  const actions = asRecord(value.actions) ?? {};
  for (const [id, disposition] of Object.entries(actions)) {
    const definition = definitions[id];
    if (!definition) {
      issues.push(issue(`/actions/${id}`, "action_unknown", "未知动作。"));
      continue;
    }
    if (
      typeof disposition !== "string" ||
      !definition.allowed.includes(disposition)
    ) {
      issues.push(
        issue(`/actions/${id}`, "disposition_not_allowed", "动作处置不在允许范围。"),
      );
    } else if (definition.dangerous && disposition === "auto") {
      issues.push(
        issue(`/actions/${id}`, "disposition_not_allowed", "危险动作不能自动放行。"),
      );
    }
  }
  if (!["approve", "deny"].includes(asString(value.unlisted_action))) {
    issues.push(
      issue("/unlisted_action", "disposition_not_allowed", "未列动作只允许 approve 或 deny。"),
    );
  }
  const paths = asRecord(value.paths) ?? {};
  for (const [id, rawRule] of Object.entries(paths)) {
    const rule = asRecord(rawRule);
    const pointer = `/paths/${id}`;
    const globs = Array.isArray(rule?.globs) ? rule.globs : null;
    if (!globs || globs.length === 0) {
      issues.push(issue(`${pointer}/globs`, "required", "路径规则至少需要一个 glob。"));
    }
    if (!["approve", "deny"].includes(asString(rule?.disposition))) {
      issues.push(
        issue(`${pointer}/disposition`, "disposition_not_allowed", "路径规则只允许 approve 或 deny。"),
      );
    } else if (
      (id === "workflows" || id === "sensitive_files") &&
      rule?.disposition !== "deny"
    ) {
      issues.push(
        issue(`${pointer}/disposition`, "invariant_floor", "该规则必须保持 deny。"),
      );
    }
  }
  const allowGlobs = Array.isArray(value.allow_globs) ? value.allow_globs : [];
  if (allowGlobs.length === 0) {
    issues.push(issue("/allow_globs", "glob_invalid", "allow_globs 不能为空。"));
  }
  const approval = asRecord(value.approval);
  const ttl = approval?.ttl_s;
  if (
    typeof ttl !== "number" ||
    !Number.isInteger(ttl) ||
    ttl < 60 ||
    ttl > 604_800
  ) {
    issues.push(
      issue("/approval/ttl_s", "range", "审批有效期必须在 60–604800 秒。"),
    );
  }
  const patch = asRecord(value.patch);
  for (const key of PATCH_KEYS) {
    if (!patch || !isPositiveInteger(patch[key])) {
      issues.push(issue(`/patch/${key}`, "range", "补丁上限必须是正整数。"));
    }
  }
  return issues;
}

function validateGuardian(
  value: Record<string, unknown>,
  catalog: SettingsCatalog,
): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const triage = asRecord(value.triage) ?? {};
  const definitions = catalog.guardian?.triage ?? {};
  for (const [id, disposition] of Object.entries(triage)) {
    const definition = definitions[id];
    if (!definition) {
      issues.push(issue(`/triage/${id}`, "triage_value_invalid", "未知分流类。"));
    } else if (definition.locked) {
      issues.push(issue(`/triage/${id}`, "locked_key", "锁定类不可配置。"));
    } else if (
      typeof disposition !== "string" ||
      !definition.allowed.includes(disposition)
    ) {
      issues.push(
        issue(`/triage/${id}`, "triage_value_invalid", "分流处置不在允许范围。"),
      );
    }
  }
  const noProgress = asRecord(value.no_progress);
  for (const key of ["same_class_repeats", "stalled_rounds"]) {
    if (!noProgress || !isPositiveInteger(noProgress[key])) {
      issues.push(
        issue(`/no_progress/${key}`, "range", "阈值必须是正整数。"),
      );
    }
  }
  const checklist = asRecord(value.status_checklist);
  if (
    checklist &&
    (typeof checklist.min_update_interval_s !== "number" ||
      !Number.isInteger(checklist.min_update_interval_s) ||
      checklist.min_update_interval_s < 0)
  ) {
    issues.push(
      issue(
        "/status_checklist/min_update_interval_s",
        "range",
        "状态清单间隔必须是非负整数。",
      ),
    );
  }
  if (!isPositiveInteger(value.patrol_interval_s) || Number(value.patrol_interval_s) < 300) {
    issues.push(issue("/patrol_interval_s", "range", "巡检间隔不得小于 300 秒。"));
  }
  return issues;
}

function validateNotify(value: Record<string, unknown>): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  const channels = asRecord(value.channels);
  const desktop = asRecord(channels?.desktop);
  if (!channels || !desktop || typeof desktop.enabled !== "boolean") {
    issues.push(issue("/channels/desktop", "type", "桌面通道必须是布尔开关。"));
  }
  const mobile = asRecord(channels?.mobile_push);
  if (mobile && mobile.enabled === true) {
    issues.push(
      issue("/channels/mobile_push", "channel_unavailable", "手机推送在第一版不可启用。"),
    );
  }
  const routes = asRecord(value.routes) ?? {};
  let urgentReachable = false;
  for (const [id, rawRoute] of Object.entries(routes)) {
    const route = asRecord(rawRoute);
    const pointer = `/routes/${id}`;
    if (!route) {
      issues.push(issue(pointer, "type", "路由必须是对象。"));
      continue;
    }
    const kinds = Array.isArray(route.kinds) ? route.kinds : [];
    const routeChannels = Array.isArray(route.channels) ? route.channels : [];
    if (kinds.length === 0) {
      issues.push(issue(`${pointer}/kinds`, "required", "路由至少选择一种上报类型。"));
    }
    for (const kind of kinds) {
      if (!INBOX_KINDS.includes(String(kind))) {
        issues.push(issue(`${pointer}/kinds`, "enum", "上报类型不在允许范围。"));
      }
    }
    if (routeChannels.length === 0) {
      issues.push(issue(`${pointer}/channels`, "required", "路由至少选择一个渠道。"));
    }
    for (const channel of routeChannels) {
      if (channel !== "desktop") {
        issues.push(
          issue(`${pointer}/channels`, "channel_unavailable", "第一版只支持 desktop 渠道。"),
        );
      }
    }
    const urgentKinds = [
      "approval_needed",
      "security_finding",
      "budget_exhausted",
      "escalation",
    ];
    if (
      desktop?.enabled === true &&
      routeChannels.includes("desktop") &&
      kinds.some((kind) => urgentKinds.includes(String(kind)))
    ) {
      urgentReachable = true;
    }
  }
  if (desktop?.enabled === true && !urgentReachable) {
    issues.push(
      issue(
        "/routes",
        "urgent_unrouted",
        "approval_needed / security_finding / budget_exhausted / escalation 至少要有可达的已启用渠道。",
      ),
    );
  } else if (desktop && desktop.enabled === false) {
    issues.push(
      issue(
        "/channels/desktop/enabled",
        "urgent_unrouted",
        "关闭桌面通道后，urgent 类上报没有任何已启用渠道可达。",
      ),
    );
  }
  return issues;
}

function isPositiveInteger(value: unknown): boolean {
  return typeof value === "number" && Number.isInteger(value) && value > 0;
}

function isPositiveNumber(value: unknown): boolean {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

function isForbiddenHost(host: string, patterns: string[]): boolean {
  const name = host.trim().toLowerCase();
  if (!name || name === "*" || name.includes("**")) return true;
  if (/^(?:\d{1,3}\.){3}\d{1,3}$/.test(name) || name.includes(":")) return true;
  for (const pattern of patterns) {
    const normalized = pattern.toLowerCase();
    if (normalized.startsWith("*.")) {
      const suffix = normalized.slice(2);
      if (name === suffix || name.endsWith(`.${suffix}`)) return true;
    } else if (name === normalized) {
      return true;
    }
  }
  return false;
}

function issue(pointer: string, code: string, message: string): SettingsIssue {
  return { pointer, code, message };
}

function fingerprint(value: unknown): string {
  return JSON.stringify(value);
}

function buildDraft(doc: SettingsDocument): Draft {
  const value = structuredClone(asRecord(doc.value) ?? {});
  return {
    value,
    baseline: fingerprint(value),
    secrets: {},
    secretSpent: false,
    needsReentry: false,
  };
}

function hasSecrets(draft: Draft): boolean {
  return Object.values(draft.secrets).some(Boolean);
}

export function SettingsResourcePage({
  controller,
  state,
  page,
  onDirtyChange,
}: SettingsResourcePageProps): ReactNode {
  const info = PAGE_RESOURCES[page];
  const [load, setLoad] = useState<LoadState>({
    status: "loading",
    data: null,
    error: null,
  });
  const [primaryDraft, setPrimaryDraft] = useState<Draft | null>(null);
  const [secondaryDraft, setSecondaryDraft] = useState<Draft | null>(null);
  const [conflict, setConflict] = useState(false);
  const [saved, setSaved] = useState<{ which: Which; text: string } | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  const [statusReloading, setStatusReloading] = useState(false);
  const primaryWrite = useGuardedWrite(controller.ipc);
  const secondaryWrite = useGuardedWrite(controller.ipc);
  const editVersions = useRef<Record<Which, number>>({ primary: 0, secondary: 0 });
  const pendingExpected = useRef<Record<Which, unknown>>({
    primary: undefined,
    secondary: undefined,
  });
  const baselineRevision = useRef(state.settingsRevision);
  const ownWrite = useRef(false);
  const alive = useRef(true);
  const requestSeq = useRef(0);
  const statusSeq = useRef(0);
  const pageRef = useRef(page);
  const loadedPage = useRef<SettingsResourcePageId | null>(null);
  const scenario = isScenarioAdapter(controller.ipc);
  // 渲染期同步页面身份，供迟到的异步结果校验。
  pageRef.current = page;

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const loadModels = info.loadModels;

  const applyLoad = useCallback(
    (
      data: LoadData,
      expectation: { which: Which; rev: number | null } | undefined,
      pageId: SettingsResourcePageId,
    ): void => {
      if (!alive.current || pageRef.current !== pageId) return;
      setLoad({ status: "ready", data, error: null });
      setPrimaryDraft(buildDraft(data.primary.doc));
      setSecondaryDraft(data.secondary ? buildDraft(data.secondary.doc) : null);
      setConflict(false);
      loadedPage.current = pageId;
      baselineRevision.current = controller.settingsRevision;
      if (expectation) {
        const actual =
          expectation.which === "primary"
            ? normalizeForCompare(data.primary.doc.value)
            : normalizeForCompare(data.secondary?.doc.value);
        const equal =
          fingerprint(actual) === fingerprint(pendingExpected.current[expectation.which]);
        const rev = expectation.rev === null ? "" : `（rev ${expectation.rev}）`;
        setSaved({
          which: expectation.which,
          text: equal
            ? scenario
              ? `模拟设置已应用${rev}，已重新读取，回读一致。仅写入开发场景内存；生效时点仍按 §2.6：判定类立即，创建类从下一个对象开始。`
              : `设置已应用${rev}，已重新读取，回读一致。生效时点按 config.md §2.6：判定类立即生效，创建类从下一个对象开始。`
            : scenario
              ? `模拟设置已应用${rev}，已重新读取；回读值与提交值不同，请检查。`
              : `设置已应用${rev}，已重新读取；回读值与提交值不同，请检查。`,
        });
      } else {
        setSaved(null);
      }
      pendingExpected.current = { primary: undefined, secondary: undefined };
      setLocalError(null);
    },
    [controller, scenario],
  );

  const reload = useCallback(
    async (expectation?: { which: Which; rev: number | null }): Promise<void> => {
      const pageAtReload = page;
      if (pageRef.current !== pageAtReload) return;
      const seq = ++requestSeq.current;
      statusSeq.current += 1;
      const isCurrent = (): boolean =>
        alive.current &&
        requestSeq.current === seq &&
        pageRef.current === pageAtReload;
      setLoad((prev) => ({ ...prev, status: "loading", error: null }));
      setSaved(null);
      setLocalError(null);
      setConflict(false);
      setStatusReloading(false);
      try {
        const [catalogResult, primaryResult, secondaryResult, modelsResult] =
          await Promise.all([
            controller.ipc.api("settings.catalog", {}),
            controller.ipc.api("settings.get", { resource: info.primary }),
            info.secondary
              ? controller.ipc.api("settings.get", { resource: info.secondary })
              : Promise.resolve(null),
            loadModels
              ? controller.ipc.api("settings.get", { resource: "models" })
              : Promise.resolve(null),
          ]);
        if (!isCurrent()) return;
        if (!catalogResult.ok || !catalogResult.body) {
          setLoad({
            status: "error",
            data: null,
            error: catalogResult.ok
              ? "服务端没有返回设置目录。"
              : parseApiFailure(catalogResult),
          });
          return;
        }
        if (!primaryResult.ok || !primaryResult.body) {
          setLoad({
            status: "error",
            data: null,
            error: primaryResult.ok
              ? "服务端没有返回设置。"
              : parseApiFailure(primaryResult),
          });
          return;
        }
        if (secondaryResult && (!secondaryResult.ok || !secondaryResult.body)) {
          setLoad({
            status: "error",
            data: null,
            error: secondaryResult.ok
              ? "服务端没有返回关联设置。"
              : parseApiFailure(secondaryResult),
          });
          return;
        }
        let status: SettingsStatus | null = null;
        let statusError: string | null = null;
        try {
          const statusResult = await controller.ipc.api("settings.status", {});
          if (statusResult.ok && statusResult.body) status = statusResult.body;
          else statusError = statusResult.ok ? "服务端没有返回设置状态。" : parseApiFailure(statusResult);
        } catch (failure) {
          statusError = ipcErrorText(failure);
        }
        if (!isCurrent()) return;
        const data: LoadData = {
          primary: {
            doc: primaryResult.body,
            etag: etagFrom(primaryResult),
          },
          secondary:
            secondaryResult && secondaryResult.ok && secondaryResult.body
              ? {
                  doc: secondaryResult.body,
                  etag: etagFrom(secondaryResult),
                }
              : null,
          catalog: catalogResult.body,
          models:
            modelsResult && modelsResult.ok && modelsResult.body
              ? modelsResult.body
              : null,
          status,
          statusError,
        };
        applyLoad(data, expectation, pageAtReload);
      } catch (failure) {
        if (!isCurrent()) return;
        setLoad({ status: "error", data: null, error: ipcErrorText(failure) });
      }
    },
    [applyLoad, controller, info.primary, info.secondary, loadModels, page],
  );

  useEffect(() => {
    primaryWrite.clear();
    secondaryWrite.clear();
    pendingExpected.current = { primary: undefined, secondary: undefined };
  }, [page, primaryWrite.clear, secondaryWrite.clear]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const revision = state.settingsRevision;
  useEffect(() => {
    if (load.status !== "ready" || revision === baselineRevision.current) return;
    baselineRevision.current = revision;
    if (ownWrite.current || loadedPage.current !== page) return;
    setConflict(true);
    primaryWrite.clear();
    secondaryWrite.clear();
    setPrimaryDraft((prev) =>
      prev ? { ...prev, needsReentry: prev.secretSpent } : prev,
    );
    setSecondaryDraft((prev) =>
      prev ? { ...prev, needsReentry: prev.secretSpent } : prev,
    );
  }, [load.status, page, primaryWrite, revision, secondaryWrite]);

  const primaryDirty =
    primaryDraft !== null &&
    (primaryDraft.needsReentry ||
      hasSecrets(primaryDraft) ||
      fingerprint(primaryDraft.value) !== primaryDraft.baseline ||
      primaryWrite.preview !== null);
  const secondaryDirty =
    secondaryDraft !== null &&
    (secondaryDraft.needsReentry ||
      hasSecrets(secondaryDraft) ||
      fingerprint(secondaryDraft.value) !== secondaryDraft.baseline ||
      secondaryWrite.preview !== null);
  const dirty = primaryDirty || secondaryDirty;

  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange(false), [onDirtyChange]);

  const busy =
    primaryWrite.phase !== "idle" || secondaryWrite.phase !== "idle";

  const updateDraft = useCallback(
    (which: Which, mutate: (draft: Draft) => Draft): void => {
      const write = which === "primary" ? primaryWrite : secondaryWrite;
      if (write.phase !== "idle") return;
      if (primaryWrite.phase !== "idle" || secondaryWrite.phase !== "idle") {
        return;
      }
      editVersions.current[which] += 1;
      write.discard();
      setSaved(null);
      setLocalError(null);
      const setter = which === "primary" ? setPrimaryDraft : setSecondaryDraft;
      setter((prev) => (prev ? mutate(prev) : prev));
    },
    [primaryWrite, secondaryWrite],
  );

  const editValue = useCallback(
    (which: Which, path: Path, next: unknown): void => {
      updateDraft(which, (draft) => ({
        ...draft,
        value: setAtPath(draft.value, path, next) as Record<string, unknown>,
        needsReentry: draft.secretSpent ? true : draft.needsReentry,
      }));
    },
    [updateDraft],
  );

  const editSecret = useCallback(
    (which: Which, path: Path, next: string): void => {
      updateDraft(which, (draft) => {
        const secrets = { ...draft.secrets };
        if (next) secrets[secretKey(path)] = next;
        else delete secrets[secretKey(path)];
        return {
          ...draft,
          secrets,
          needsReentry: next ? false : draft.secretSpent || draft.needsReentry,
        };
      });
    },
    [updateDraft],
  );

  const markNeedsReentry = useCallback((which: Which, value: boolean): void => {
    const setter = which === "primary" ? setPrimaryDraft : setSecondaryDraft;
    setter((prev) => (prev ? { ...prev, needsReentry: value } : prev));
  }, []);

  const clearSecrets = useCallback((which: Which, hadSecret: boolean): void => {
    const setter = which === "primary" ? setPrimaryDraft : setSecondaryDraft;
    setter((prev) =>
      prev
        ? {
            ...prev,
            secrets: {},
            secretSpent: prev.secretSpent || hadSecret,
          }
        : prev,
    );
  }, []);

  const validate = useCallback(
    (which: Which): SettingsIssue[] => {
      if (!load.data) return [];
      const definition = definitionFor(page, {
        primary: primaryDraft?.value ?? {},
        secondary: secondaryDraft?.value ?? null,
        catalog: load.data.catalog,
        models: load.data.models,
        state,
      });
      const sections = which === "primary" ? definition.buildPrimary({
        primary: primaryDraft?.value ?? {},
        secondary: secondaryDraft?.value ?? null,
        catalog: load.data.catalog,
        models: load.data.models,
        state,
      }) : definition.buildSecondary({
        primary: primaryDraft?.value ?? {},
        secondary: secondaryDraft?.value ?? null,
        catalog: load.data.catalog,
        models: load.data.models,
        state,
      });
      const fieldSpecs: FieldSpec[] = [];
      for (const section of sections) {
        if (section.kind === "fields") fieldSpecs.push(...section.fields);
      }
      const draft = which === "primary" ? primaryDraft : secondaryDraft;
      const requiredIssues = collectRequiredIssues(fieldSpecs, draft?.value);
      const baseIssues =
        which === "primary"
          ? definition.validatePrimary({
              primary: primaryDraft?.value ?? {},
              secondary: secondaryDraft?.value ?? null,
              catalog: load.data.catalog,
              models: load.data.models,
              state,
            })
          : definition.validateSecondary({
              primary: primaryDraft?.value ?? {},
              secondary: secondaryDraft?.value ?? null,
              catalog: load.data.catalog,
              models: load.data.models,
              state,
            });
      const merged = [...requiredIssues, ...baseIssues];
      const seen = new Set<string>();
      return merged.filter((entry) => {
        const key = `${entry.pointer}|${entry.code}`;
        if (seen.has(key)) return false;
        seen.add(key);
        return true;
      });
    },
    [load.data, page, primaryDraft, secondaryDraft, state],
  );

  const otherResourceBlocked = useCallback(
    (which: Which): boolean => {
      const otherDraft = which === "primary" ? secondaryDraft : primaryDraft;
      const otherWrite = which === "primary" ? secondaryWrite : primaryWrite;
      if (!otherDraft) return false;
      return (
        otherDraft.needsReentry ||
        hasSecrets(otherDraft) ||
        fingerprint(otherDraft.value) !== otherDraft.baseline ||
        otherWrite.preview !== null
      );
    },
    [primaryDraft, primaryWrite, secondaryDraft, secondaryWrite],
  );

  const handleSave = useCallback(
    async (which: Which): Promise<void> => {
      if (!load.data) return;
      const draft = which === "primary" ? primaryDraft : secondaryDraft;
      const write = which === "primary" ? primaryWrite : secondaryWrite;
      const doc = which === "primary" ? load.data.primary : load.data.secondary;
      if (!draft || !doc) return;
      if (conflict) {
        setLocalError("此页已被其他设备修改；请先载入最新值，不能自动合并。");
        return;
      }
      if (otherResourceBlocked(which)) {
        setLocalError(
          "另一个关联资源有未保存改动或预览；请先保存或放弃它，再保存当前资源。",
        );
        return;
      }
      if (draft.needsReentry) {
        setLocalError("凭据值已在准备时清除，请重新输入凭据后再保存。");
        return;
      }
      const clientIssues = validate(which);
      if (clientIssues.length > 0) {
        setLocalError(`请先修正标出的 ${clientIssues.length} 项字段错误。`);
        return;
      }
      const resource = which === "primary" ? info.primary : info.secondary;
      if (!resource) return;
      const built = injectSecrets(draft.value, draft.secrets);
      const hadSecret = hasSecrets(draft);
      const version = editVersions.current[which];
      const pageAtSave = page;
      setLocalError(null);
      setSaved(null);
      const request = write.prepare({
        kind: "settings_put",
        scope: "global",
        resource,
        value: built,
        ifMatch: doc.etag,
      });
      pendingExpected.current[which] = normalizeForCompare(built);
      clearSecrets(which, hadSecret);
      const result = await request;
      if (pageRef.current !== pageAtSave) {
        write.discard();
        return;
      }
      if (editVersions.current[which] !== version) {
        write.discard();
        if (hadSecret) markNeedsReentry(which, true);
        setLocalError("准备期间草稿已变化，旧预览已丢弃；请重新保存。");
        return;
      }
      if (hadSecret && (!result || !result.valid)) {
        markNeedsReentry(which, true);
        setLocalError("凭据值已从界面清除，请重新输入凭据后再保存。");
      }
    },
    [
      conflict,
      info.primary,
      info.secondary,
      load.data,
      markNeedsReentry,
      clearSecrets,
      otherResourceBlocked,
      page,
      primaryDraft,
      primaryWrite,
      secondaryDraft,
      secondaryWrite,
      validate,
    ],
  );

  const handleSubmit = useCallback(
    async (which: Which): Promise<void> => {
      const draft = which === "primary" ? primaryDraft : secondaryDraft;
      const write = which === "primary" ? primaryWrite : secondaryWrite;
      if (!draft || !write.preview || !write.preview.can_submit.ok || conflict) return;
      if (otherResourceBlocked(which)) {
        setLocalError(
          "另一个关联资源有未保存改动或预览；已保留当前预览，请先处理后再提交。",
        );
        return;
      }
      const pageAtSubmit = page;
      ownWrite.current = true;
      const result = await write.submit();
      ownWrite.current = false;
      if (pageRef.current !== pageAtSubmit) {
        // 旧页面的写入不影响当前页；接受已变化的 rev，避免误报外部冲突。
        baselineRevision.current = controller.settingsRevision;
        return;
      }
      if (!result) {
        if (draft.secretSpent) markNeedsReentry(which, true);
        return;
      }
      if (result.ok) {
        const rev = result.headers.x_event_seq ?? null;
        await reload({ which, rev });
        return;
      }
      if (
        "status" in result &&
        (result.status === 412 || result.problem.code === "precondition_failed")
      ) {
        setConflict(true);
        if (draft.secretSpent) markNeedsReentry(which, true);
        return;
      }
      if (draft.secretSpent) markNeedsReentry(which, true);
    },
    [
      conflict,
      controller,
      markNeedsReentry,
      otherResourceBlocked,
      page,
      primaryDraft,
      primaryWrite,
      reload,
      secondaryDraft,
      secondaryWrite,
    ],
  );

  const handleDiscard = useCallback(
    (which: Which): void => {
      const write = which === "primary" ? primaryWrite : secondaryWrite;
      const draft = which === "primary" ? primaryDraft : secondaryDraft;
      if (draft?.secretSpent) markNeedsReentry(which, true);
      write.clear();
    },
    [markNeedsReentry, primaryDraft, primaryWrite, secondaryDraft, secondaryWrite],
  );

  const handleReload = useCallback(async (): Promise<void> => {
    primaryWrite.clear();
    secondaryWrite.clear();
    await reload();
  }, [primaryWrite, reload, secondaryWrite]);

  const reloadStatus = useCallback(async (): Promise<void> => {
    const pageAtStatus = page;
    const seq = ++statusSeq.current;
    const isCurrent = (): boolean =>
      alive.current &&
      pageRef.current === pageAtStatus &&
      statusSeq.current === seq;
    setStatusReloading(true);
    try {
      const result = await controller.ipc.api("settings.status", {});
      if (!isCurrent()) return;
      setLoad((prev) =>
        prev.data
          ? {
              ...prev,
              data: {
                ...prev.data,
                status: result.ok ? result.body : prev.data.status,
                statusError: result.ok
                  ? null
                  : parseApiFailure(result),
              },
            }
          : prev,
      );
    } catch (failure) {
      if (!isCurrent()) return;
      setLoad((prev) =>
        prev.data
          ? {
              ...prev,
              data: { ...prev.data, statusError: ipcErrorText(failure) },
            }
          : prev,
      );
    } finally {
      if (isCurrent()) setStatusReloading(false);
    }
  }, [controller, page]);

  if (load.status === "loading") {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">{titleForPage(page)}</h1>
        <p role="status" className="ov-settings__muted">
          正在载入{titleForPage(page)}设置…
        </p>
      </section>
    );
  }

  if (load.status === "error" || !load.data || !primaryDraft) {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">{titleForPage(page)}</h1>
        <div role="alert" className="ov-alert">
          {load.error ?? "设置不可用。"}
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => void reload()}
          >
            重试
          </button>
        </div>
      </section>
    );
  }

  const data = load.data;
  const baseCtx: ValidationContext = {
    primary: primaryDraft.value,
    secondary: secondaryDraft?.value ?? null,
    catalog: data.catalog,
    models: data.models,
    state,
  };
  const definition = definitionFor(page, baseCtx);
  const primarySections = definition.buildPrimary(baseCtx);
  const secondarySections = definition.buildSecondary(baseCtx);
  const clientPrimaryIssues = validate("primary");
  const clientSecondaryIssues = secondaryDraft ? validate("secondary") : [];
  const primaryIssues = mergeIssues(
    clientPrimaryIssues,
    primaryWrite.preview?.errors ?? [],
  );
  const secondaryIssues = mergeIssues(
    clientSecondaryIssues,
    secondaryWrite.preview?.errors ?? [],
  );
  const primaryBlocked = primaryWrite.preview
    ? submitBlockReason(primaryWrite.preview.can_submit, scenario)
    : null;
  const secondaryBlocked = secondaryWrite.preview
    ? submitBlockReason(secondaryWrite.preview.can_submit, scenario)
    : null;

  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">{definition.title}</h1>
        <p className="ov-settings__muted">{introForPage(page)}</p>
        <p className="ov-settings__meta">
          字段来源：docs/interfaces/config.md 的文字契约（手工转写，非生成代码）。
          枚举与动态选项优先取 settings.catalog。
        </p>
      </div>

      {scenario ? (
        <div className="ov-notice ov-notice--info" role="status">
          {SCENARIO_NOTE}
        </div>
      ) : null}

      {conflict ? (
        <div className="ov-notice ov-notice--warn" role="alert">
          <span>此页已被其他设备修改。不会自动合并。</span>
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => void handleReload()}
          >
            载入最新值（丢弃我的改动）
          </button>
        </div>
      ) : null}

      {localError ? (
        <div role="alert" className="ov-alert">
          {localError}
        </div>
      ) : null}

      <div className="sr-sections">
        {primarySections.map((section, index) => (
          <SectionRenderer
            key={`${sectionTitle(section)}-${index}`}
            section={section}
            primary
            value={primaryDraft.value}
            issues={primaryIssues}
            secrets={primaryDraft.secrets}
            disabled={busy || conflict}
            scenario={scenario}
            catalog={data.catalog}
            models={data.models}
            projects={state.projects}
            onChange={editValue}
            onSecretChange={editSecret}
            onConfigureSecret={(path) => {
              if (getAtPath(primaryDraft.value, path) === undefined) {
                editValue("primary", path, { secret_ref: "", present: false });
              }
            }}
            onRemoveSecret={(path) => editValue("primary", path, undefined)}
          />
        ))}
      </div>

      {secondarySections.length > 0 && secondaryDraft ? (
        <div className="sr-sections sr-sections--secondary">
          <div className="sr-subhead">
            <h2 className="ov-settings__h2">{definition.secondaryLabel}</h2>
            <span className="ov-settings__meta">
              关联资源：{info.secondary}；独立保存与回读。
            </span>
          </div>
          {secondarySections.map((section, index) => (
            <SectionRenderer
              key={`${sectionTitle(section)}-${index}`}
              section={section}
              primary={false}
              value={secondaryDraft.value}
              issues={secondaryIssues}
              secrets={secondaryDraft.secrets}
              disabled={busy || conflict}
              scenario={scenario}
              catalog={data.catalog}
              models={data.models}
              projects={state.projects}
              onChange={editValue}
              onSecretChange={editSecret}
              onConfigureSecret={(path) => {
                if (getAtPath(secondaryDraft.value, path) === undefined) {
                  editValue("secondary", path, { secret_ref: "", present: false });
                }
              }}
              onRemoveSecret={(path) => editValue("secondary", path, undefined)}
            />
          ))}
        </div>
      ) : null}

      <SettingsStatusLine
        status={data.status}
        error={data.statusError}
        modules={definition.modules}
        reloading={statusReloading}
        onRetry={() => void reloadStatus()}
      />

      <WritePanel
        which="primary"
        label={definition.primaryLabel}
        draft={primaryDraft}
        write={primaryWrite}
        scenario={scenario}
        conflict={conflict}
        busy={busy}
        blocked={primaryBlocked}
        saved={saved?.which === "primary" ? saved.text : null}
        onSave={() => void handleSave("primary")}
        onSubmit={() => void handleSubmit("primary")}
        onDiscard={() => handleDiscard("primary")}
        onReload={() => void handleReload()}
        onReenter={() => setLocalError("请重新输入测试占位凭据后再保存。")}
      />

      {definition.secondaryLabel && secondaryDraft ? (
        <WritePanel
          which="secondary"
          label={definition.secondaryLabel}
          draft={secondaryDraft}
          write={secondaryWrite}
          scenario={scenario}
          conflict={conflict}
          busy={busy}
          blocked={secondaryBlocked}
          saved={saved?.which === "secondary" ? saved.text : null}
          onSave={() => void handleSave("secondary")}
          onSubmit={() => void handleSubmit("secondary")}
          onDiscard={() => handleDiscard("secondary")}
          onReload={() => void handleReload()}
          onReenter={() => setLocalError("请重新输入测试占位凭据后再保存。")}
        />
      ) : null}
    </section>
  );
}

function mergeIssues(
  left: SettingsIssue[],
  right: SettingsIssue[],
): SettingsIssue[] {
  const seen = new Set<string>();
  return [...left, ...right].filter((entry) => {
    const key = `${entry.pointer}|${entry.code}|${entry.message ?? ""}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function submitBlockReason(
  canSubmit: import("@/api/shell").GuardedWritePreview["can_submit"],
  scenario: boolean,
): string | null {
  if (canSubmit.ok) return null;
  switch (canSubmit.reason) {
    case "not_connected":
      return "无法提交：外壳未连接。";
    case "not_approver":
      return "此改动需要审批设备签名，而本机不是审批设备；开发场景不产生签名，生产需在审批设备上完成。";
    case "invalid":
      return scenario
        ? "无法提交：预览未通过校验。"
        : "无法提交：预览未通过校验。";
  }
}

function titleForPage(page: SettingsResourcePageId): string {
  return {
    levels: "档位与预算",
    github: "GitHub App",
    sandbox: "沙盒与出网",
    policies: "危险动作策略",
    guardian: "后台守护",
    notify: "通知",
  }[page];
}

function introForPage(page: SettingsResourcePageId): string {
  switch (page) {
    case "levels":
      return "档位是任务的上限；预算模板在交付单元创建时按 level_cap 固定，运行中的单元不随配置变化。自动加码属于 policies.escalation，在本页单独保存。";
    case "github":
      return "GitHub App 标识、凭据引用与提交身份只保存在服务端。凭据只写不读；安装状态来自项目实体，不在本页创建安装。";
    case "sandbox":
      return "沙盒驱动、镜像与资源限制只对新沙盒生效；出网档案在沙盒创建时固化，收紧立即作用于存量沙盒。凭据只经 Egress 注入。";
    case "policies":
      return "危险动作由 policy 唯一放行；审批绑定动作、目标与 subject_digest。本页只编辑已有契约键，不提供跳过 GitHub review 或自动合并的开关。";
    case "guardian":
      return "分流处置、无进展阈值、状态清单与对账巡检。锁定类不可配置；所有静默处理仍写收件箱与审计。";
    case "notify":
      return "服务端通知渠道与路由。收件箱写入与审计不可关闭；本页不控制系统通知，也不代替原生通知权限。";
  }
}

function sectionTitle(section: SectionSpec): string {
  if (section.kind === "table") return section.table.title;
  if (section.kind === "budget" || section.kind === "actions" || section.kind === "triage") {
    return section.title;
  }
  if (section.kind === "provider-routes" || section.kind === "installations") {
    return section.title;
  }
  return section.title;
}

function SectionRenderer({
  section,
  primary,
  value,
  issues,
  secrets,
  disabled,
  scenario,
  catalog,
  models,
  projects,
  onChange,
  onSecretChange,
  onConfigureSecret,
  onRemoveSecret,
}: {
  section: SectionSpec;
  primary: boolean;
  value: Record<string, unknown>;
  issues: SettingsIssue[];
  secrets: Record<string, string>;
  disabled?: boolean;
  scenario: boolean;
  catalog: SettingsCatalog;
  models: SettingsDocument | null;
  projects: DesktopState["projects"];
  onChange: (which: Which, path: Path, next: unknown) => void;
  onSecretChange: (which: Which, path: Path, next: string) => void;
  onConfigureSecret: (path: Path) => void;
  onRemoveSecret: (path: Path) => void;
}): ReactNode {
  const which: Which = primary ? "primary" : "secondary";
  switch (section.kind) {
    case "fields":
      return (
        <section className="ov-settings__section">
          <h2 className="ov-settings__h2">{section.title}</h2>
          {section.note ? (
            <p className="ov-settings__muted">{section.note}</p>
          ) : null}
          <FieldGrid
            fields={section.fields}
            value={value}
            issues={issues}
            disabled={disabled}
            secrets={secrets}
            onSecretChange={(path, next) => onSecretChange(which, path, next)}
            onChange={(path, next) => onChange(which, path, next)}
          />
        </section>
      );
    case "table":
      return (
        <RecordTable
          spec={section.table}
          value={value}
          issues={issues}
          secrets={secrets}
          disabled={disabled}
          scenario={scenario}
          onSecretChange={(path, next) => onSecretChange(which, path, next)}
          onChange={(path, next) => onChange(which, path, next)}
        />
      );
    case "budget":
      return (
        <BudgetTable
          title={section.title}
          note={section.note}
          value={value}
          issues={issues}
          disabled={disabled}
          catalog={catalog}
          onChange={(path, next) => onChange(which, path, next)}
        />
      );
    case "actions":
      return (
        <ActionsTable
          title={section.title}
          note={section.note}
          value={value}
          issues={issues}
          disabled={disabled}
          catalog={catalog}
          onChange={(path, next) => onChange(which, path, next)}
        />
      );
    case "triage":
      return (
        <TriageTable
          title={section.title}
          note={section.note}
          value={value}
          issues={issues}
          disabled={disabled}
          catalog={catalog}
          onChange={(path, next) => onChange(which, path, next)}
        />
      );
    case "secret": {
      const path = section.path;
      const secret = secrets[secretKey(path)] ?? "";
      return (
        <section className="ov-settings__section">
          <h2 className="ov-settings__h2">{section.title}</h2>
          <SecretRefEditor
            label={section.label}
            value={getAtPath(value, path)}
            secret={secret}
            issues={issuesUnderPath(issues, path)}
            disabled={disabled}
            scenario={scenario}
            required={section.required}
            removable={section.removable}
            onConfigure={() => onConfigureSecret(path)}
            onRemove={() => onRemoveSecret(path)}
            onChange={(next) => onChange(which, path, next)}
            onSecretChange={(next) => onSecretChange(which, path, next)}
          />
        </section>
      );
    }
    case "provider-routes":
      return <ProviderRoutes title={section.title} note={section.note} models={models} />;
    case "installations":
      return (
        <Installations
          title={section.title}
          note={section.note}
          projects={projects}
        />
      );
    case "note":
      return (
        <section className="ov-settings__section">
          <h2 className="ov-settings__h2">{section.title}</h2>
          <p className="ov-notice ov-notice--info">{section.body}</p>
        </section>
      );
  }
}

function BudgetTable({
  title,
  note,
  value,
  issues,
  disabled,
  catalog,
  onChange,
}: {
  title: string;
  note?: string;
  value: Record<string, unknown>;
  issues: SettingsIssue[];
  disabled?: boolean;
  catalog: SettingsCatalog;
  onChange: (path: Path, next: unknown) => void;
}): ReactNode {
  const templates = catalog.levels?.graph_templates ?? {};
  return (
    <section className="ov-settings__section">
      <h2 className="ov-settings__h2">{title}</h2>
      {note ? <p className="ov-settings__muted">{note}</p> : null}
      <div
        className="ov-table-wrap sr-table-wrap"
        tabIndex={0}
        role="region"
        aria-label={title}
      >
        <table className="ov-table sr-budget">
          <thead>
            <tr>
              <th>档位</th>
              <th>图模板</th>
              <th>打杂 tokens</th>
              <th>大脑 tokens</th>
              <th>干活 tokens</th>
              <th>节点上限</th>
              <th>轮次上限</th>
              <th>墙钟上限（秒）</th>
              <th>并发上限</th>
              <th>写动作上限</th>
              <th>每小时写上限</th>
            </tr>
          </thead>
          <tbody>
            {BUDGET_LEVELS.map((level) => {
              const entry = asRecord(value[level]) ?? {};
              const budget = asRecord(entry.budget) ?? {};
              const options = [
                ...(templates[level] ?? []),
                ...(typeof entry.graph_template === "string" &&
                entry.graph_template &&
                !(templates[level] ?? []).includes(entry.graph_template)
                  ? [entry.graph_template]
                  : []),
              ];
              return (
                <tr key={level}>
                  <td className="ov-mono">{level}</td>
                  <td>
                    <select
                      className="ov-select ov-select--sm ov-select--mono"
                      aria-label={`${level} 图模板`}
                      value={asString(entry.graph_template)}
                      disabled={disabled}
                      onChange={(event) =>
                        onChange([level, "graph_template"], event.target.value)
                      }
                    >
                      {!entry.graph_template ? <option value="">未配置</option> : null}
                      {options.map((option) => (
                        <option value={option} key={option}>
                          {option}
                        </option>
                      ))}
                    </select>
                    <FieldIssues
                      issues={issuesAtPath(issues, [level, "graph_template"])}
                    />
                  </td>
                  {[
                    ["tokens_by_tier", "utility"],
                    ["tokens_by_tier", "planner"],
                    ["tokens_by_tier", "worker"],
                  ].map(([group, key]) => {
                    const pointer = [level, "budget", group, key];
                    const current = getAtPath(budget, [group, key]);
                    return (
                      <td key={key}>
                        <input
                          type="number"
                          className="ov-input ov-input--sm"
                          aria-label={`${level} ${key} tokens`}
                          min={1}
                          step={1}
                          value={typeof current === "number" ? String(current) : ""}
                          disabled={disabled}
                          onChange={(event) => {
                            const text = event.target.value;
                            onChange(
                              pointer,
                              text === "" ? undefined : Number(text),
                            );
                          }}
                        />
                        <FieldIssues issues={issuesAtPath(issues, pointer)} />
                      </td>
                    );
                  })}
                  {["nodes", "rounds", "wall_time_s", "concurrency", "forge_writes", "forge_writes_per_hour"].map(
                    (key) => {
                      const pointer = [level, "budget", key];
                      const current = budget[key];
                      return (
                        <td key={key}>
                          <input
                            type="number"
                            className="ov-input ov-input--sm"
                            aria-label={`${level} ${key}`}
                            min={1}
                            step={1}
                            value={
                              typeof current === "number" ? String(current) : ""
                            }
                            disabled={disabled}
                            onChange={(event) => {
                              const text = event.target.value;
                              onChange(pointer, text === "" ? undefined : Number(text));
                            }}
                          />
                          <FieldIssues issues={issuesAtPath(issues, pointer)} />
                        </td>
                      );
                    },
                  )}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <TableIssues issues={issues} base={[]} />
    </section>
  );
}

function ActionsTable({
  title,
  note,
  value,
  issues,
  disabled,
  catalog,
  onChange,
}: {
  title: string;
  note?: string;
  value: Record<string, unknown>;
  issues: SettingsIssue[];
  disabled?: boolean;
  catalog: SettingsCatalog;
  onChange: (path: Path, next: unknown) => void;
}): ReactNode {
  const definitions = catalog.policies?.actions ?? {};
  const actions = asRecord(value.actions) ?? {};
  const extra = Object.keys(actions).filter((id) => definitions[id] === undefined);
  return (
    <section className="ov-settings__section">
      <h2 className="ov-settings__h2">{title}</h2>
      {note ? <p className="ov-settings__muted">{note}</p> : null}
      <div
        className="ov-table-wrap sr-table-wrap"
        tabIndex={0}
        role="region"
        aria-label={title}
      >
        <table className="ov-table sr-table sr-table--actions">
          <thead>
            <tr>
              <th>动作</th>
              <th>处置</th>
              <th>默认</th>
              <th>说明</th>
            </tr>
          </thead>
          <tbody>
            {Object.entries(definitions).map(([id, definition]) => (
              <tr key={id}>
                <td className="ov-mono">{id}</td>
                <td>
                  <select
                    className="ov-select ov-select--sm"
                    aria-label={`${id} 处置`}
                    value={asString(actions[id])}
                    disabled={disabled}
                    onChange={(event) =>
                      onChange(["actions", id], event.target.value || undefined)
                    }
                  >
                    {!actions[id] ? <option value="">未配置</option> : null}
                    {definition.allowed.map((option) => (
                      <option value={option} key={option}>
                        {option}
                      </option>
                    ))}
                  </select>
                  <FieldIssues issues={issuesAtPath(issues, ["actions", id])} />
                </td>
                <td className="ov-mono">{definition.default}</td>
                <td className="ov-settings__meta">
                  {definition.reserved
                    ? "预留动作，第一版不执行"
                    : definition.dangerous
                      ? "危险动作，不能 auto"
                      : "常规动作"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {extra.length > 0 ? (
        <div className="sr-table-issues" role="alert">
          {extra.map((id) => (
            <span key={id}>{id}：目录未列出该动作。</span>
          ))}
        </div>
      ) : null}
    </section>
  );
}

function TriageTable({
  title,
  note,
  value,
  issues,
  disabled,
  catalog,
  onChange,
}: {
  title: string;
  note?: string;
  value: Record<string, unknown>;
  issues: SettingsIssue[];
  disabled?: boolean;
  catalog: SettingsCatalog;
  onChange: (path: Path, next: unknown) => void;
}): ReactNode {
  const definitions = catalog.guardian?.triage ?? {};
  const triage = asRecord(value.triage) ?? {};
  return (
    <section className="ov-settings__section">
      <h2 className="ov-settings__h2">{title}</h2>
      {note ? <p className="ov-settings__muted">{note}</p> : null}
      <div
        className="ov-table-wrap sr-table-wrap"
        tabIndex={0}
        role="region"
        aria-label={title}
      >
        <table className="ov-table sr-table sr-table--triage">
          <thead>
            <tr>
              <th>类 id</th>
              <th>处置</th>
              <th>状态</th>
            </tr>
          </thead>
          <tbody>
            {Object.entries(definitions).map(([id, definition]) => (
              <tr key={id}>
                <td className="ov-mono">{id}</td>
                <td>
                  {definition.locked ? (
                    <span className="sr-readonly">
                      {definition.value ?? "escalate"}
                    </span>
                  ) : (
                    <select
                      className="ov-select ov-select--sm"
                      aria-label={`${id} 分流`}
                      value={asString(triage[id])}
                      disabled={disabled}
                      onChange={(event) =>
                        onChange(["triage", id], event.target.value || undefined)
                      }
                    >
                      {!triage[id] ? <option value="">继承默认</option> : null}
                      {definition.allowed.map((option) => (
                        <option value={option} key={option}>
                          {option}
                        </option>
                      ))}
                    </select>
                  )}
                  <FieldIssues issues={issuesAtPath(issues, ["triage", id])} />
                </td>
                <td className="ov-settings__meta">
                  {definition.locked
                    ? "锁定：不可配置"
                    : `默认 ${definition.default ?? "—"}`}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <TableIssues issues={issues} base={[]} />
    </section>
  );
}

function ProviderRoutes({
  title,
  note,
  models,
}: {
  title: string;
  note?: string;
  models: SettingsDocument | null;
}): ReactNode {
  const providers = asRecord(asRecord(models?.value)?.providers) ?? {};
  const entries = Object.entries(providers);
  return (
    <section className="ov-settings__section">
      <h2 className="ov-settings__h2">{title}</h2>
      {note ? <p className="ov-settings__muted">{note}</p> : null}
      {entries.length === 0 ? (
        <p className="ov-settings__muted">
          无法读取 models.providers，或服务端尚未配置模型提供方；本页不展示派生路由。
        </p>
      ) : (
        <div
          className="ov-table-wrap sr-table-wrap"
          tabIndex={0}
          role="region"
          aria-label={title}
        >
          <table className="ov-table sr-table sr-table--compact">
            <thead>
              <tr>
                <th>提供方</th>
                <th>base_url</th>
                <th>credential.secret_ref</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              {entries.map(([id, rawProvider]) => {
                const provider = asRecord(rawProvider) ?? {};
                const credential = asRecord(provider.credential) ?? {};
                return (
                  <tr key={id}>
                    <td className="ov-mono">{id}</td>
                    <td className="ov-mono">{asString(provider.base_url) || "—"}</td>
                    <td className="ov-mono">
                      {asString(credential.secret_ref) || "—"}
                    </td>
                    <td>{credential.present === true ? "已设置 ●●●●" : "未设置"}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function Installations({
  title,
  note,
  projects,
}: {
  title: string;
  note?: string;
  projects: DesktopState["projects"];
}): ReactNode {
  return (
    <section className="ov-settings__section">
      <h2 className="ov-settings__h2">{title}</h2>
      {note ? <p className="ov-settings__muted">{note}</p> : null}
      <div
        className="ov-table-wrap sr-table-wrap"
        tabIndex={0}
        role="region"
        aria-label={title}
      >
        <table className="ov-table sr-table sr-table--compact">
          <thead>
            <tr>
              <th>项目</th>
              <th>installation_id</th>
              <th>bot_user_id</th>
              <th>默认分支</th>
            </tr>
          </thead>
          <tbody>
            {projects.map((project) => (
              <tr key={project.id}>
                <td className="ov-mono">{project.repo}</td>
                <td className="ov-mono">{project.installation_id}</td>
                <td className="ov-mono">{project.bot_user_id}</td>
                <td className="ov-mono">{project.default_branch}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function SettingsStatusLine({
  status,
  error,
  modules,
  reloading,
  onRetry,
}: {
  status: SettingsStatus | null;
  error: string | null;
  modules: string[];
  reloading: boolean;
  onRetry: () => void;
}): ReactNode {
  return (
    <section className="ov-settings__section sr-status" aria-label="设置应用状态">
      <h2 className="ov-settings__h2">应用状态</h2>
      {!status ? (
        <div className="ov-notice ov-notice--warn" role="status">
          <span>{error ?? "尚未读取设置状态。"}</span>
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            disabled={reloading}
            onClick={onRetry}
          >
            {reloading ? "读取中…" : "重试读取状态"}
          </button>
        </div>
      ) : (
        <>
          <span className="ov-settings__meta">
            最新设置 rev {status.latest_rev}。applied_rev 来自 settings.status。
          </span>
          <div className="ov-table-wrap sr-table-wrap">
          <table className="ov-table sr-status__table">
            <thead>
              <tr>
                <th>模块</th>
                <th>applied_rev</th>
                <th>健康</th>
                <th>说明</th>
              </tr>
            </thead>
            <tbody>
              {modules.map((module) => {
                const entry = status.modules[module];
                if (!entry) {
                  return (
                    <tr key={module}>
                      <td className="ov-mono">{module}</td>
                      <td colSpan={3} className="ov-settings__muted">
                        服务端未上报该模块。
                      </td>
                    </tr>
                  );
                }
                return (
                  <tr key={module}>
                    <td className="ov-mono">{module}</td>
                    <td className="ov-mono">{entry.applied_rev}</td>
                    <td>
                      <span
                        className={`tone-dot ${entry.health === "ok" ? "tone-ok" : entry.health === "degraded" ? "tone-warn" : "tone-idle"}`}
                        aria-hidden="true"
                      />{" "}
                      {entry.health}
                    </td>
                    <td className="ov-settings__meta">
                      {entry.applied_rev < status.latest_rev
                        ? `尚未应用最新 rev ${status.latest_rev}`
                        : "已应用最新 rev"}
                      {entry.detail ? ` · ${entry.detail}` : ""}
                      {entry.unresolved_secret_refs &&
                      entry.unresolved_secret_refs.length > 0
                        ? ` · 缺失凭据：${entry.unresolved_secret_refs.join("、")}`
                        : ""}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          </div>
          {error ? (
            <div className="ov-notice ov-notice--warn" role="status">
              <span>{error}</span>
              <button
                type="button"
                className="ov-btn ov-btn--sm"
                disabled={reloading}
                onClick={onRetry}
              >
                重试读取状态
              </button>
            </div>
          ) : null}
        </>
      )}
    </section>
  );
}

function WritePanel({
  which,
  label,
  draft,
  write,
  scenario,
  conflict,
  busy,
  blocked,
  saved,
  onSave,
  onSubmit,
  onDiscard,
  onReload,
  onReenter,
}: {
  which: Which;
  label: string;
  draft: Draft;
  write: ReturnType<typeof useGuardedWrite>;
  scenario: boolean;
  conflict: boolean;
  busy: boolean;
  blocked: string | null;
  saved: string | null;
  onSave: () => void;
  onSubmit: () => void;
  onDiscard: () => void;
  onReload: () => void;
  onReenter: () => void;
}): ReactNode {
  const dirty =
    fingerprint(draft.value) !== draft.baseline ||
    hasSecrets(draft) ||
    draft.needsReentry ||
    write.preview !== null;
  const preview = write.preview;
  return (
    <section className="ov-save sr-save" aria-label={`${label}保存`}>
      <h2 className="ov-settings__h2">{label}</h2>
      <p className="ov-settings__muted">
        保存先做 dry-run 预览；确认后才写入。凭据与放宽类改动在生产需要审批设备签名。
      </p>

      {write.error && !conflict ? (
        <div role="alert" className="ov-alert">
          {write.error}
          {!preview ? (
            <button type="button" className="ov-btn ov-btn--sm" onClick={onSave}>
              重试保存
            </button>
          ) : null}
        </div>
      ) : null}

      {draft.needsReentry ? (
        <div role="status" className="ov-notice ov-notice--warn">
          凭据值已在准备时清除；请重新输入测试占位凭据后再保存。
          <button type="button" className="ov-btn ov-btn--sm" onClick={onReenter}>
            查看说明
          </button>
        </div>
      ) : null}

      {saved ? (
        <div role="status" className="ov-notice ov-notice--ok">
          {saved}
        </div>
      ) : null}

      {!preview ? (
        <div className="ov-save__actions">
          <button
            type="button"
            className="ov-btn ov-btn--primary"
            disabled={!dirty || busy || conflict || draft.needsReentry}
            onClick={onSave}
          >
            保存{label}
          </button>
          {scenario ? (
            <span className="ov-settings__meta">
              开发场景只写入内存；不会提交到真实服务端。
            </span>
          ) : null}
        </div>
      ) : (
        <div className="sr-preview">
          <h3 className="ov-settings__h3">保存预览</h3>
          {preview.errors.length > 0 ? (
            <div role="alert" className="ov-alert">
              预览未通过校验：
              <IssuesList issues={preview.errors} />
            </div>
          ) : null}
          {preview.warnings.length > 0 ? (
            <div role="status" className="ov-notice ov-notice--warn">
              <IssuesList issues={preview.warnings} />
            </div>
          ) : null}
          {preview.relaxes.length > 0 ? (
            <div role="status" className="ov-notice ov-notice--warn">
              此改动放宽了安全限制：{preview.relaxes.join("、")}
            </div>
          ) : null}
          {preview.requires_signature ? (
            <div role="status" className="ov-notice ov-notice--info">
              {scenario
                ? "此改动在生产需要审批设备签名。开发场景只做内存模拟应用，不产生真实签名。"
                : "此改动需要审批设备签名。提交时由外壳发起系统确认。"}
            </div>
          ) : null}
          {blocked ? (
            <div role="status" className="ov-notice ov-notice--warn">
              {blocked}
            </div>
          ) : null}
          <div className="ov-save__actions">
            <button
              type="button"
              className="ov-btn ov-btn--primary"
              disabled={!preview.can_submit.ok || busy || conflict}
              onClick={onSubmit}
            >
              {scenario ? "模拟应用设置" : "确认提交"}
            </button>
            <button
              type="button"
              className="ov-btn"
              disabled={busy}
              onClick={onDiscard}
            >
              取消预览
            </button>
            <button
              type="button"
              className="ov-btn"
              disabled={busy}
              onClick={onReload}
            >
              重新载入
            </button>
          </div>
          {scenario ? (
            <span className="ov-settings__meta">
              提交后立即回读；不会产生签名、证书或网络写入。
            </span>
          ) : null}
        </div>
      )}
      <span className="ov-settings__meta">资源作用域：global · 独立保存与回读。</span>
    </section>
  );
}

function IssuesList({ issues }: { issues: SettingsIssue[] }): ReactNode {
  return (
    <span>
      {issues.map((issue) => (
        <span
          className="ov-issues__item"
          key={`${issue.pointer}-${issue.code}-${issue.message ?? ""}`}
        >
          {issue.pointer || "（整体）"}：{issue.message ?? issueTextFallback(issue)}
        </span>
      ))}
    </span>
  );
}

function issueTextFallback(issue: SettingsIssue): string {
  return issueText(issue);
}

function TableIssues({
  issues,
  base,
}: {
  issues: SettingsIssue[];
  base: Path;
}): ReactNode {
  const pointer = pathPointer(base);
  const relevant = issues.filter(
    (entry) =>
      pointer === "/" ||
      entry.pointer === pointer ||
      entry.pointer.startsWith(`${pointer}/`),
  );
  if (relevant.length === 0) return null;
  return (
    <div className="sr-table-issues" role="alert">
      <IssuesList issues={relevant} />
    </div>
  );
}
