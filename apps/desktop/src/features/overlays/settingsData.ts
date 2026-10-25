/**
 * 设置页的读取辅助。
 *
 * 共享机器可读 schema 尚未交付；这里只按 docs/interfaces/config.md §3/§4
 * 读取已知字段，并保留未知键。不是生成代码。
 */
import type {
  SettingsCatalog,
  SettingsCatalogModel,
  SettingsDocument,
  SettingsIssue,
} from "@/api/types";
import type { ApiResult } from "@/api/shell";

export type ApiFailure = Extract<ApiResult<unknown>, { ok: false }>;
import { errorMessage } from "@/i18n";

export type ModelTier = "planner" | "worker" | "utility";

export interface TierMeta {
  id: ModelTier;
  short: string;
  name: string;
  description: string;
}

export const TIERS: TierMeta[] = [
  {
    id: "planner",
    short: "大脑",
    name: "大脑模型",
    description:
      "理解意图、规划拆图、需求准入、reasoner 预演、arbiter 裁决。量少、要准。",
  },
  {
    id: "worker",
    short: "干活",
    name: "干活模型",
    description: "在沙盒内实现、修复、担任各 review 视角。主力产量。",
  },
  {
    id: "utility",
    short: "打杂",
    name: "打杂模型",
    description: "事件摘要、CI 日志分类、提交说明、状态类问答。量大、要便宜。",
  },
];

export interface TierDraft {
  provider: string;
  model: string;
  effort: string;
  hasByLevel: boolean;
}

export interface ModelsDraft {
  planner: TierDraft;
  worker: TierDraft;
  utility: TierDraft;
}

export interface ProviderInfo {
  id: string;
  kind: string;
  baseUrl: string;
  credential: Record<string, unknown> | null;
  secretRef: string;
  present: boolean;
}

export interface ModelsValue {
  root: Record<string, unknown>;
  providers: ProviderInfo[];
  tiers: Record<ModelTier, TierDraft>;
}

export interface LoadedModels {
  doc: SettingsDocument;
  etag: string;
  value: ModelsValue;
}

export interface ProjectModelsCoverage {
  projectId: string;
  repo: string;
  overrides: ModelTier[];
  failed: boolean;
}

export function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

export function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

export function parseApiFailure(result: ApiFailure): string {
  if ("network" in result) {
    switch (result.network) {
      case "offline":
        return "连接中断，请稍后重试。";
      case "timeout":
        return "请求超时，请稍后重试。";
      case "tls":
        return "安全连接失败，请检查证书。";
      case "aborted":
        return "请求已取消。";
    }
  }
  return errorMessage(result.problem.code);
}

export function ipcErrorText(error: unknown): string {
  if (error && typeof error === "object" && "code" in error) {
    return errorMessage(String((error as { code: unknown }).code));
  }
  return "操作失败，请稍后重试。";
}

export function issueText(issue: SettingsIssue): string {
  switch (issue.code) {
    case "required":
    case "type":
    case "enum":
    case "range":
    case "unknown_field":
      return "取值为空、类型不符或不在允许范围内。";
    case "override_not_allowed":
      return "该键不能按项目覆盖。";
    case "override_loosens_global":
      return "项目覆盖只能收紧，不能放宽。";
    case "secret_ref_missing":
      return "凭据引用不存在，请填写新值。";
    case "secret_clear_in_use":
      return "使用中的凭据不能被清空。";
    case "scenario_mode":
      return "示例模式不提交、不保存。";
    case "model_class_mismatch":
      return "模型定位与用途不匹配。";
    case "effort_unsupported":
      return "所选模型不支持这个推理深度。";
    case "effort_by_level_required":
      return "选择随档位时必须提供完整的档位表。";
    case "worker_model_no_tools":
      return "干活模型必须支持工具调用。";
    default:
      return "未通过校验。";
  }
}

export function issuesAt(issues: SettingsIssue[], pointer: string): SettingsIssue[] {
  const normalized = pointer.startsWith("/") ? pointer : `/${pointer}`;
  return issues.filter((issue) => issue.pointer === normalized);
}

export function etagFrom(result: ApiResult<SettingsDocument>): string {
  if (result.ok && result.headers.etag) return result.headers.etag;
  if (result.ok && result.body) return `"${result.body.rev}"`;
  return '"0"';
}

export function readTier(root: Record<string, unknown>, tier: ModelTier): TierDraft {
  const raw = asRecord(root[tier]) ?? {};
  const byLevel = asRecord(raw.effort_by_level);
  return {
    provider: asString(raw.provider),
    model: asString(raw.model),
    effort: asString(raw.reasoning_effort),
    hasByLevel:
      byLevel !== null &&
      ["low", "medium", "high", "max"].every(
        (key) => asRecord(byLevel[key]) !== null,
      ),
  };
}

export function parseModels(value: unknown): ModelsValue {
  const root = asRecord(value) ?? {};
  const providerMap = asRecord(root.providers) ?? {};
  const providers: ProviderInfo[] = Object.entries(providerMap).map(([id, raw]) => {
    const provider = asRecord(raw) ?? {};
    const credential = asRecord(provider.credential);
    return {
      id,
      kind: asString(provider.kind),
      baseUrl: asString(provider.base_url),
      credential,
      secretRef: credential ? asString(credential.secret_ref) : "",
      present: credential?.present === true,
    };
  });
  return {
    root,
    providers,
    tiers: {
      planner: readTier(root, "planner"),
      worker: readTier(root, "worker"),
      utility: readTier(root, "utility"),
    },
  };
}

export function draftFromModels(value: ModelsValue): ModelsDraft {
  return {
    planner: { ...value.tiers.planner },
    worker: { ...value.tiers.worker },
    utility: { ...value.tiers.utility },
  };
}

export function modelKey(tier: TierDraft): string {
  return tier.provider && tier.model ? `${tier.provider}/${tier.model}` : "";
}

export function splitModelKey(key: string): { provider: string; model: string } {
  const slash = key.indexOf("/");
  if (slash <= 0) return { provider: "", model: key };
  return { provider: key.slice(0, slash), model: key.slice(slash + 1) };
}

export function catalogModels(
  catalog: SettingsCatalog | null,
  tier: ModelTier,
): Array<{ key: string; capability: SettingsCatalogModel }> {
  const capabilities = catalog?.models.capabilities ?? {};
  return Object.entries(capabilities)
    .filter(([, capability]) => tier === "utility" || capability.tools)
    .map(([key, capability]) => ({ key, capability }));
}

export function effortChoices(
  catalog: SettingsCatalog | null,
  model: string,
  current: string,
  allowByLevel: boolean,
): string[] {
  const capability = catalog?.models.capabilities[model];
  const values = [...(capability?.reasoning_efforts ?? [])];
  if (allowByLevel) values.push("by_level");
  if (current && !values.includes(current)) values.unshift(current);
  return values;
}

export function effortLabel(effort: string): string {
  if (effort === "by_level") return "随档位";
  return effort || "未配置";
}

function cloneJson(value: unknown): unknown {
  if (value === undefined) return undefined;
  return JSON.parse(JSON.stringify(value)) as unknown;
}

/** 只改已知的模型字段与本次输入的凭据值；未知键原样保留。 */
export function buildModelsValue(
  base: unknown,
  draft: ModelsDraft,
  secrets: Record<string, string>,
): unknown {
  const root = (cloneJson(base) ?? {}) as Record<string, unknown>;
  for (const tier of ["planner", "worker", "utility"] as const) {
    const entry = draft[tier];
    // 稀疏文档里未配置的档位不因保存凭据或别的档位而被创建或清空。
    if (!entry.provider && !entry.model && !entry.effort) continue;
    const next = { ...(asRecord(root[tier]) ?? {}) };
    if (entry.provider && entry.model) {
      next.provider = entry.provider;
      next.model = entry.model;
    } else {
      delete next.provider;
      delete next.model;
    }
    if (entry.effort) next.reasoning_effort = entry.effort;
    else delete next.reasoning_effort;
    root[tier] = next;
  }

  const providers = { ...(asRecord(root.providers) ?? {}) };
  for (const [id, rawProvider] of Object.entries(providers)) {
    const provider = { ...(asRecord(rawProvider) ?? {}) };
    const credential = asRecord(provider.credential);
    if (credential && "value" in credential) {
      const cleaned = { ...credential };
      delete cleaned.value;
      provider.credential = cleaned;
      providers[id] = provider;
    }
  }
  for (const [id, secret] of Object.entries(secrets)) {
    if (!secret) continue;
    const provider = { ...(asRecord(providers[id]) ?? {}) };
    const credential = { ...(asRecord(provider.credential) ?? {}) };
    credential.value = secret;
    provider.credential = credential;
    providers[id] = provider;
  }
  if (Object.keys(providers).length > 0) root.providers = providers;
  return root;
}

export function modelsFingerprint(draft: ModelsDraft): string {
  return JSON.stringify(draft);
}

export function projectLabel(repo: string, fallback: string): string {
  const slash = repo.lastIndexOf("/");
  return slash >= 0 ? repo.slice(slash + 1) : fallback;
}

export function formatDateTime(iso: string | null | undefined): string {
  if (!iso) return "未提供";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return new Intl.DateTimeFormat("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(date);
}

export function formatClock(iso: string | null | undefined): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return new Intl.DateTimeFormat("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

export function formatRemaining(ms: number): string {
  if (ms <= 0) return "已过期";
  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours} 小时 ${minutes} 分`;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

export function coversTier(
  doc: SettingsDocument | null,
  tier: ModelTier,
): boolean {
  const root = asRecord(doc?.value);
  if (!root) return false;
  const entry = asRecord(root[tier]);
  return Boolean(
    entry !== null &&
      (entry.provider ||
        entry.model ||
        entry.reasoning_effort ||
        asRecord(entry.effort_by_level)),
  );
}
