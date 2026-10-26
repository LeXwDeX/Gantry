/**
 * 项目覆盖设置页。
 *
 * 只列 docs/interfaces/config.md §2.2 允许项目覆盖的键；全局值、覆盖值与生效值
 * 同屏展示。写入仍走外壳 guardedWrite，开发场景下由 ScenarioIpcAdapter 只改内存，
 * 提交后立即回读；不产生真实签名、凭据或网络写入。
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
} from "@/api/types";
import { isScenarioAdapter } from "@/ipc";
import { useGuardedWrite } from "./useGuardedWrite";
import "./settings-project.css";
import {
  asRecord,
  etagFrom,
  ipcErrorText,
  issueText,
  parseApiFailure,
  projectLabel,
} from "./settingsData";
import {
  PROJECT_RESOURCES,
  PROJECT_RESOURCE_LABELS,
  ProjectOverrideFieldRow,
  RecordTable,
  cloneJson,
  collectRequiredIssues,
  deleteAtPath,
  formatInline,
  getAtPath,
  pathPointer,
  projectOverrideFields,
  projectOverrideTables,
  setAtPath,
  type Path,
  type ProjectOverrideField,
  type ProjectResource,
} from "./settingsFields";

export interface SettingsProjectPageProps {
  controller: DesktopController;
  state: DesktopState;
  projectId: string;
  onDirtyChange: (dirty: boolean) => void;
}

interface ResourceDocs {
  global: SettingsDocument;
  override: SettingsDocument;
  effective: SettingsDocument;
  etag: string;
}

interface LoadData {
  catalog: SettingsCatalog;
  docs: Record<ProjectResource, ResourceDocs>;
}

interface Load {
  status: "loading" | "ready" | "error";
  data: LoadData | null;
  error: string | null;
}

type Drafts = Record<ProjectResource, Record<string, unknown>>;

const SCENARIO_NOTE =
  "开发场景：预览与模拟应用只修改内存中的项目覆盖；不产生真实签名、凭据、证书或网络写入。生产保存仍走外壳受保护写入。";

const GROUP_LABELS: Record<string, string> = {
  planner: "大脑（planner）",
  worker: "干活（worker）",
  utility: "打杂（utility）",
  default_level: "默认档位上限",
  none: "None 档",
  low: "Low 档",
  medium: "Medium 档",
  high: "High 档",
  max: "Max 档",
  ultimate: "Ultimate 档",
  actions: "动作处置",
  unlisted_action: "未列动作兜底",
  approval: "审批有效期",
  patch: "补丁上限",
  escalation: "自动加码",
  triage: "后台守护分流",
  no_progress: "无进展判定",
  status_checklist: "状态清单",
  limits: "沙盒限制",
  devcontainer: "devcontainer",
  profiles: "出网档案",
};

const TIER_LABELS: Record<string, string> = {
  planner: "大脑",
  worker: "干活",
  utility: "打杂",
};

const TIERS = ["planner", "worker", "utility"] as const;
const BY_LEVELS = ["low", "medium", "high", "max"] as const;

function emptyDrafts(): Drafts {
  return Object.fromEntries(
    PROJECT_RESOURCES.map((resource) => [resource, {}]),
  ) as Drafts;
}

function emptyBaselines(): Record<ProjectResource, string> {
  return Object.fromEntries(
    PROJECT_RESOURCES.map((resource) => [resource, "{}"]),
  ) as Record<ProjectResource, string>;
}

function draftFrom(doc: SettingsDocument | null | undefined): Record<string, unknown> {
  return cloneJson(asRecord(doc?.value) ?? {});
}

/** 保存时去掉空对象；空数组与 false、0 保留，因为它们有覆盖语义。 */
function prune(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(prune);
  const record = asRecord(value);
  if (!record) return value;
  const clean: Record<string, unknown> = {};
  for (const [key, entry] of Object.entries(record)) {
    const pruned = prune(entry);
    const nested = asRecord(pruned);
    if (nested && Object.keys(nested).length === 0) continue;
    clean[key] = pruned;
  }
  return clean;
}

function stable(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stable).join(",")}]`;
  const record = asRecord(value);
  if (record) {
    return `{${Object.keys(record)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stable(record[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value ?? null);
}

function overrideCount(value: unknown): number {
  // 显式空数组也是覆盖（例如 models.*.fallback: []），只有 undefined 才表示继承。
  if (Array.isArray(value)) return 1;
  const record = asRecord(value);
  if (record) {
    return Object.values(record).reduce<number>(
      (total, entry) => total + overrideCount(entry),
      0,
    );
  }
  return value === undefined ? 0 : 1;
}

function clampedPaths(doc: SettingsDocument | null | undefined): string[] {
  return (doc?.clamped ?? []).map((entry) => entry.replace(/\//g, "."));
}

function isClamped(path: Path, clamped: string[]): boolean {
  const pointer = path.join(".");
  return clamped.some(
    (entry) => entry === pointer || entry.startsWith(`${pointer}.`),
  );
}

function fallbackField(tier: string): ProjectOverrideField {
  return {
    path: [tier, "fallback"],
    label: `${TIER_LABELS[tier] ?? tier} 降级模型`,
    kind: "lines",
    tightenOnly: false,
    keepEmptyList: true,
    help: "每行一个 provider/model；最多 3 项。留空表示覆盖为空列表，点“恢复继承”回到全局。",
  };
}

function buildFields(
  resource: ProjectResource,
  catalog: SettingsCatalog,
  globalValue: unknown,
): ProjectOverrideField[] {
  const base = projectOverrideFields(resource, catalog, globalValue);
  if (resource !== "models") return base;
  return TIERS.flatMap((tier) => [
    ...base.filter((field) => field.path[0] === tier),
    fallbackField(tier),
  ]);
}

function groupFields(
  fields: ProjectOverrideField[],
): Array<{ key: string; label: string; fields: ProjectOverrideField[] }> {
  const groups: Array<{
    key: string;
    label: string;
    fields: ProjectOverrideField[];
  }> = [];
  for (const field of fields) {
    const key = field.path[0] ?? "";
    const last = groups[groups.length - 1];
    if (last && last.key === key) last.fields.push(field);
    else
      groups.push({
        key,
        label: GROUP_LABELS[key] ?? key,
        fields: [field],
      });
  }
  return groups;
}

function issuesForDisplay(issues: SettingsIssue[]): ReactNode {
  if (issues.length === 0) return null;
  return (
    <div role="alert" className="ov-alert">
      <span className="ov-issues__item">
        {issues.map((issue) => (
          <span
            className="ov-issues__item"
            key={`${issue.pointer}-${issue.code}-${issue.message ?? ""}`}
          >
            {issue.pointer ? `${issue.pointer}：` : "（整体）"}
            {issue.message ?? issueText(issue)}
          </span>
        ))}
      </span>
    </div>
  );
}

export function SettingsProjectPage({
  controller,
  state,
  projectId,
  onDirtyChange,
}: SettingsProjectPageProps): ReactNode {
  const project = state.projects.find((item) => item.id === projectId) ?? null;
  const scenario = isScenarioAdapter(controller.ipc);
  const [load, setLoad] = useState<Load>({
    status: "loading",
    data: null,
    error: null,
  });
  const [resource, setResource] = useState<ProjectResource>("models");
  const [draftState, setDraftState] = useState<{
    drafts: Drafts;
    baselines: Record<ProjectResource, string>;
  }>({ drafts: emptyDrafts(), baselines: emptyBaselines() });
  const { drafts, baselines } = draftState;
  const [saved, setSaved] = useState<{
    resource: ProjectResource;
    text: string;
  } | null>(null);
  const [conflictResource, setConflictResource] =
    useState<ProjectResource | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  const [clientIssues, setClientIssues] = useState<SettingsIssue[]>([]);
  const write = useGuardedWrite(controller.ipc);
  const previewResource = useRef<ProjectResource | null>(null);
  const expectedValue = useRef<unknown>(null);
  const canSubmitRef = useRef(false);
  const loadSeq = useRef(0);
  canSubmitRef.current = write.preview?.can_submit.ok ?? false;

  const fetchResource = useCallback(
    async (target: ProjectResource): Promise<ResourceDocs> => {
      const [globalResult, overrideResult, effectiveResult] = await Promise.all([
        controller.ipc.api("settings.get", { resource: target }),
        controller.ipc.api("project_settings.get", {
          projectId,
          resource: target,
        }),
        controller.ipc.api("project_settings.get", {
          projectId,
          resource: target,
          view: "effective",
        }),
      ]);
      if (!globalResult.ok) throw new Error(parseApiFailure(globalResult));
      if (!overrideResult.ok) throw new Error(parseApiFailure(overrideResult));
      if (!effectiveResult.ok) throw new Error(parseApiFailure(effectiveResult));
      if (!globalResult.body || !overrideResult.body || !effectiveResult.body) {
        throw new Error("项目设置响应不完整。");
      }
      return {
        global: globalResult.body,
        override: overrideResult.body,
        effective: effectiveResult.body,
        etag: etagFrom(overrideResult),
      };
    },
    [controller, projectId],
  );

  const loadAll = useCallback(async (): Promise<void> => {
    const seq = ++loadSeq.current;
    setLoad((prev) => ({ ...prev, status: "loading", error: null }));
    setConflictResource(null);
    setSaved(null);
    setLocalError(null);
    setClientIssues([]);
    write.clear();
    previewResource.current = null;
    try {
      const catalogResult = await controller.ipc.api("settings.catalog", {});
      if (!catalogResult.ok) throw new Error(parseApiFailure(catalogResult));
      if (!catalogResult.body) throw new Error("设置目录响应不完整。");
      const entries = await Promise.all(
        PROJECT_RESOURCES.map(
          async (target) => [target, await fetchResource(target)] as const,
        ),
      );
      if (loadSeq.current !== seq) return;
      const docs = Object.fromEntries(entries) as Record<
        ProjectResource,
        ResourceDocs
      >;
      const nextDrafts = emptyDrafts();
      const nextBaselines = emptyBaselines();
      for (const target of PROJECT_RESOURCES) {
        const draft = draftFrom(docs[target].override);
        nextDrafts[target] = draft;
        nextBaselines[target] = stable(draft);
      }
      setDraftState({ drafts: nextDrafts, baselines: nextBaselines });
      setLoad({
        status: "ready",
        data: { catalog: catalogResult.body, docs },
        error: null,
      });
    } catch (failure) {
      if (loadSeq.current !== seq) return;
      setLoad({
        status: "error",
        data: null,
        error: failure instanceof Error ? failure.message : ipcErrorText(failure),
      });
    }
  }, [controller, fetchResource, write.clear]);

  useEffect(() => {
    void loadAll();
  }, [loadAll]);

  const dirty = PROJECT_RESOURCES.some(
    (target) => stable(drafts[target]) !== baselines[target],
  );
  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(
    () => () => {
      onDirtyChange(false);
    },
    [onDirtyChange],
  );

  const clearPreview = useCallback((): void => {
    write.clear();
    previewResource.current = null;
    expectedValue.current = null;
  }, [write.clear]);

  const changeDraft = useCallback(
    (target: ProjectResource, next: unknown): void => {
      clearPreview();
      setClientIssues([]);
      setLocalError(null);
      setSaved(null);
      setDraftState((prev) => ({
        ...prev,
        drafts: {
          ...prev.drafts,
          [target]: cloneJson(asRecord(next) ?? {}),
        },
      }));
    },
    [clearPreview],
  );

  const switchResource = useCallback(
    (target: ProjectResource): void => {
      if (target === resource) return;
      clearPreview();
      setClientIssues([]);
      setLocalError(null);
      setSaved(null);
      setResource(target);
    },
    [clearPreview, resource],
  );

  const applyFresh = useCallback(
    (target: ProjectResource, fresh: ResourceDocs): void => {
      setLoad((prev) =>
        prev.data
          ? {
              ...prev,
              data: {
                ...prev.data,
                docs: { ...prev.data.docs, [target]: fresh },
              },
            }
          : prev,
      );
      const draft = draftFrom(fresh.override);
      setDraftState((prev) => ({
        ...prev,
        drafts: { ...prev.drafts, [target]: draft },
        baselines: { ...prev.baselines, [target]: stable(draft) },
      }));
      setConflictResource((current) => (current === target ? null : current));
      setClientIssues([]);
      setLocalError(null);
    },
    [],
  );

  const reloadOne = useCallback(
    async (target: ProjectResource): Promise<ResourceDocs | null> => {
      const seq = ++loadSeq.current;
      clearPreview();
      try {
        const fresh = await fetchResource(target);
        if (loadSeq.current !== seq) return null;
        applyFresh(target, fresh);
        return fresh;
      } catch (failure) {
        if (loadSeq.current !== seq) return null;
        setLocalError(
          `重新载入失败：${
            failure instanceof Error ? failure.message : ipcErrorText(failure)
          }`,
        );
        return null;
      }
    },
    [applyFresh, clearPreview, fetchResource],
  );

  const handleSave = useCallback(async (): Promise<void> => {
    if (!load.data) return;
    const docs = load.data.docs[resource];
    const draft = drafts[resource];
    const fields = buildFields(
      resource,
      load.data.catalog,
      docs.global.value,
    );
    const issues = collectRequiredIssues(fields, draft);
    if (issues.length > 0) {
      setClientIssues(issues);
      setLocalError(`请先修正标出的 ${issues.length} 项字段错误。`);
      return;
    }
    const value = prune(draft);
    const record = asRecord(value) ?? {};
    const isEmpty = Object.keys(record).length === 0;
    const args = isEmpty
      ? ({
          kind: "settings_delete" as const,
          projectId,
          resource,
          ifMatch: docs.etag,
        })
      : ({
          kind: "settings_put" as const,
          scope: "project" as const,
          projectId,
          resource,
          value: record,
          ifMatch: docs.etag,
        });
    setClientIssues([]);
    setLocalError(null);
    setSaved(null);
    const expected = isEmpty ? null : value;
    expectedValue.current = expected;
    const result = await write.prepare(args);
    if (!result) {
      previewResource.current = null;
      return;
    }
    previewResource.current = resource;
    if (!result.valid) {
      setLocalError("预览未通过校验，请先修正标出的字段。");
    }
  }, [drafts, load.data, projectId, resource, write.prepare]);

  const handleSubmit = useCallback(async (): Promise<void> => {
    const target = previewResource.current;
    if (!target || !canSubmitRef.current || conflictResource) return;
    const expected = expectedValue.current;
    const result = await write.submit();
    if (!result) return;
    if (result.ok) {
      const rev = result.headers.x_event_seq;
      const fresh = await reloadOne(target);
      if (!fresh) return;
      const equal = stable(fresh.override.value) === stable(expected);
      setSaved({
        resource: target,
        text: equal
          ? `模拟设置已应用${
              rev === undefined ? "" : `（rev ${rev}）`
            }，已重新读取，回读一致。仅写入开发场景内存。`
          : "模拟设置已应用，已重新读取；回读值与提交值不同，请检查。",
      });
      previewResource.current = null;
      expectedValue.current = null;
      return;
    }
    if (
      "status" in result &&
      (result.status === 412 || result.problem.code === "precondition_failed")
    ) {
      setConflictResource(target);
      setLocalError("提交冲突：此资源的覆盖已被其他设备修改。请载入最新值后再编辑。");
    }
  }, [conflictResource, reloadOne, write.submit]);

  if (!project) {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">项目</h1>
        <div role="alert" className="ov-alert">
          找不到项目 {projectId}。
        </div>
      </section>
    );
  }

  if (load.status === "loading") {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">
          项目 · <span className="ov-mono">{projectLabel(project.repo, project.id)}</span>
        </h1>
        <p role="status" className="ov-settings__muted">
          正在载入六类项目覆盖…
        </p>
      </section>
    );
  }

  if (load.status === "error" || !load.data) {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">
          项目 · <span className="ov-mono">{projectLabel(project.repo, project.id)}</span>
        </h1>
        <div role="alert" className="ov-alert">
          {load.error ?? "项目设置不可用。"}
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => void loadAll()}
          >
            重试载入
          </button>
        </div>
      </section>
    );
  }

  const docs = load.data.docs[resource];
  const catalog = load.data.catalog;
  const draft = drafts[resource];
  const fields = buildFields(resource, catalog, docs.global.value);
  const groups = groupFields(fields);
  const tables = projectOverrideTables(resource);
  const clamped = clampedPaths(docs.effective);
  const issues = [...clientIssues, ...(write.preview?.errors ?? [])];
  const busy = write.phase !== "idle" || write.preview !== null;
  const resourceDirty = stable(draft) !== baselines[resource];
  const previewValue = prune(draft);
  const deletePreview =
    Object.keys(asRecord(previewValue) ?? {}).length === 0;

  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">
          项目 · <span className="ov-mono">{projectLabel(project.repo, project.id)}</span>
        </h1>
        <p className="ov-settings__muted">
          六类资源按 config.md §2.2 只列项目可覆盖的键。未覆盖的键继承全局；
          “只能收紧”的键放宽会被拒绝，全局后来收紧时以 clamped 标注。
        </p>
        {scenario ? (
          <p className="ov-settings__meta">{SCENARIO_NOTE}</p>
        ) : null}
      </div>

      <nav className="sp-tabs" aria-label="项目覆盖资源">
        {PROJECT_RESOURCES.map((target) => {
          const count = overrideCount(drafts[target]);
          const targetDirty = stable(drafts[target]) !== baselines[target];
          return (
            <button
              type="button"
              key={target}
              className={`sp-tab${target === resource ? " sp-tab--active" : ""}${
                targetDirty ? " sp-tab--dirty" : ""
              }`}
              aria-pressed={target === resource}
              aria-label={`${PROJECT_RESOURCE_LABELS[target]}覆盖`}
              onClick={() => switchResource(target)}
            >
              <span>{PROJECT_RESOURCE_LABELS[target]}</span>
              <span className="sp-tab__count">
                {targetDirty
                  ? "未保存"
                  : count === 0
                    ? "继承全部"
                    : `覆盖 ${count} 项`}
              </span>
            </button>
          );
        })}
      </nav>

      <div className="sp-resource-meta">
        <span>
          全局 rev {docs.global.rev} · 覆盖 rev {docs.override.rev} · 生效 rev{" "}
          {docs.effective.rev}
        </span>
        {stable(draft) !== baselines[resource] ? (
          <span className="sp-dirty" role="status">
            此资源有未保存改动
          </span>
        ) : null}
      </div>

      {localError ? (
        <div role="alert" className="ov-alert">
          {localError}
        </div>
      ) : null}
      {conflictResource === resource ? (
        <div className="ov-notice ov-notice--warn" role="alert">
          <span>此资源的覆盖已被其他设备修改；不会自动合并。</span>
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => void reloadOne(resource)}
          >
            载入最新值（丢弃我的改动）
          </button>
        </div>
      ) : null}
      {clamped.length > 0 ? (
        <div role="status" className="ov-notice ov-notice--warn">
          生效值被全局约束收紧：{clamped.join("、")}
        </div>
      ) : null}
      {saved && saved.resource === resource ? (
        <div role="status" className="ov-notice ov-notice--ok">
          {saved.text}
        </div>
      ) : null}

      {issues.length > 0 ? issuesForDisplay(issues) : null}

      <div className="sp-fields">
        {groups.map((group) => (
          <section className="sp-group" key={group.key} aria-label={group.label}>
            <h2 className="ov-settings__h2">{group.label}</h2>
            <div className="sp-rows">
              {group.fields.map((field) => {
                const fieldClamped = isClamped(field.path, clamped);
                return (
                  <div
                    className={`sp-row${fieldClamped ? " sp-row--clamped" : ""}`}
                    key={pathPointer(field.path)}
                  >
                    <div className="sp-row__global">
                      <span className="sp-row__caption">全局</span>
                      <span
                        className="ov-mono ov-break"
                        role="note"
                        aria-label={`${field.label} 全局值`}
                      >
                        {formatInline(getAtPath(docs.global.value, field.path))}
                      </span>
                      {fieldClamped ? (
                        <span className="sp-clamp">已按全局收紧</span>
                      ) : null}
                    </div>
                    <div
                      className="sp-row__editor"
                      role="group"
                      aria-label={`${field.label} 覆盖行`}
                    >
                      <ProjectOverrideFieldRow
                        field={field}
                        effectiveValue={getAtPath(
                          docs.effective.value,
                          field.path,
                        )}
                        overrideValue={getAtPath(draft, field.path)}
                        disabled={busy}
                        onChange={(next) =>
                          changeDraft(
                            resource,
                            setAtPath(draft, field.path, next),
                          )
                        }
                        onClear={() =>
                          changeDraft(
                            resource,
                            deleteAtPath(draft, field.path),
                          )
                        }
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          </section>
        ))}
      </div>

      {resource === "models" ? (
        <section className="sp-group" aria-label="随档推理映射">
          <h2 className="ov-settings__h2">随档推理映射（reasoning_effort = by_level）</h2>
          <p className="ov-settings__muted">
            每个档位必须提供 default；未覆盖时整张映射继承全局。修改任一档位会整体写入
            effort_by_level 覆盖。
          </p>
          {(["planner", "worker"] as const).map((tier) => (
            <EffortByLevelRow
              key={tier}
              tier={tier}
              globalValue={getAtPath(docs.global.value, [tier, "effort_by_level"])}
              effectiveValue={getAtPath(docs.effective.value, [
                tier,
                "effort_by_level",
              ])}
              overrideValue={getAtPath(draft, [tier, "effort_by_level"])}
              capabilities={effortsForTier(docs.effective.value, tier, catalog)}
              disabled={busy}
              onChange={(next) =>
                changeDraft(
                  resource,
                  setAtPath(draft, [tier, "effort_by_level"], next),
                )
              }
              onClear={() =>
                changeDraft(
                  resource,
                  deleteAtPath(draft, [tier, "effort_by_level"]),
                )
              }
            />
          ))}
        </section>
      ) : null}

      {tables.map((table) => {
        const globalRules = asRecord(getAtPath(docs.global.value, table.path)) ?? {};
        const overrideRules = asRecord(getAtPath(draft, table.path)) ?? {};
        return (
          <section className="sp-group" key={pathPointer(table.path)}>
            <div className="sp-inherit" role="note">
              全局继承：{Object.keys(globalRules).join("、") || "无"}；覆盖中：
              {Object.keys(overrideRules).join("、") || "无（继承全部）"}。
            </div>
            <RecordTable
              spec={table}
              value={draft}
              issues={issues}
              secrets={{}}
              disabled={busy}
              scenario={scenario}
              onSecretChange={() => undefined}
              onChange={(path, next) => changeDraft(resource, setAtPath(draft, path, next))}
            />
          </section>
        );
      })}

      <section className="sp-save" aria-label="项目覆盖保存">
        <div className="ov-save__actions">
          <button
            type="button"
            className="ov-btn ov-btn--primary"
            disabled={
              !resourceDirty ||
              busy ||
              conflictResource === resource
            }
            onClick={() => void handleSave()}
          >
            保存{PROJECT_RESOURCE_LABELS[resource]}覆盖
          </button>
          <span className="ov-settings__meta">
            保存只做预览；确认后才模拟应用并回读。
          </span>
        </div>

        {write.error ? (
          <div role="alert" className="ov-alert">
            {write.error}
          </div>
        ) : null}

        {write.preview ? (
          <div className="sp-preview">
            <h3 className="ov-settings__h3">保存预览</h3>
            <p className="sp-preview__kind">
              {deletePreview
                ? "恢复继承（DELETE）：删除此资源的全部项目覆盖，生效值回退到全局。"
                : "写入项目覆盖（PUT）：只含显式覆盖的键，不是完整生效值。"}
            </p>
            <pre className="sp-preview__json" aria-label="覆盖写入预览">
              {JSON.stringify(previewValue, null, 2)}
            </pre>
            {write.preview.warnings.length > 0 ? (
              <div role="status" className="ov-notice ov-notice--warn">
                {write.preview.warnings.map((warning) => (
                  <span
                    className="ov-issues__item"
                    key={`${warning.pointer}-${warning.code}`}
                  >
                    {warning.message ?? issueText(warning)}
                  </span>
                ))}
              </div>
            ) : null}
            {write.preview.relaxes.length > 0 ? (
              <div role="status" className="ov-notice ov-notice--warn">
                此改动放宽了安全限制：{write.preview.relaxes.join("、")}
              </div>
            ) : null}
            {write.preview.requires_signature ? (
              <div role="status" className="ov-notice ov-notice--info">
                {scenario
                  ? "此改动在生产需要审批设备签名；开发场景只做内存模拟应用，不产生签名。"
                  : "此改动需要审批设备签名。"}
              </div>
            ) : null}
            {!write.preview.can_submit.ok ? (
              <div role="status" className="ov-notice ov-notice--warn">
                无法提交：
                {write.preview.can_submit.reason === "not_connected"
                  ? "外壳未连接；生产环境不会提交。"
                  : write.preview.can_submit.reason === "not_approver"
                    ? "此改动需要审批设备签名，而本机不是审批设备。"
                    : "预览未通过校验。"}
              </div>
            ) : null}
            <div className="ov-save__actions">
              <button
                type="button"
                className="ov-btn ov-btn--primary"
                disabled={
                  !write.preview.can_submit.ok ||
                  write.phase !== "idle" ||
                  conflictResource !== null
                }
                onClick={() => void handleSubmit()}
              >
                {scenario ? "模拟应用设置" : "确认提交"}
              </button>
              <button
                type="button"
                className="ov-btn"
                disabled={write.phase !== "idle"}
                onClick={() => clearPreview()}
              >
                取消预览
              </button>
              <button
                type="button"
                className="ov-btn"
                disabled={write.phase !== "idle"}
                onClick={() => void reloadOne(resource)}
              >
                重新载入此资源
              </button>
            </div>
            {scenario ? (
              <span className="ov-settings__meta">
                提交后立即回读；不会产生签名、证书或网络写入。
              </span>
            ) : null}
          </div>
        ) : null}

        <p className="ov-settings__muted">
          恢复继承会从覆盖文档删除对应键；整份覆盖清空时提交 DELETE，回退到全局。
          clamped 只表示全局收紧后的生效结果，不会改写已保存的项目覆盖。
        </p>
      </section>
    </section>
  );
}

function effortsForTier(
  modelsValue: unknown,
  tier: string,
  catalog: SettingsCatalog,
): string[] {
  const entry = asRecord(getAtPath(modelsValue, [tier])) ?? {};
  const key =
    typeof entry.provider === "string" && typeof entry.model === "string"
      ? `${entry.provider}/${entry.model}`
      : "";
  const capability = catalog.models.capabilities[key];
  if (capability) return capability.reasoning_efforts;
  const union = new Set<string>();
  for (const item of Object.values(catalog.models.capabilities)) {
    for (const effort of item.reasoning_efforts) union.add(effort);
  }
  return [...union];
}

function EffortByLevelRow({
  tier,
  globalValue,
  effectiveValue,
  overrideValue,
  capabilities,
  disabled,
  onChange,
  onClear,
}: {
  tier: string;
  globalValue: unknown;
  effectiveValue: unknown;
  overrideValue: unknown;
  capabilities: string[];
  disabled?: boolean;
  onChange: (next: Record<string, unknown>) => void;
  onClear: () => void;
}): ReactNode {
  const globalMap = asRecord(globalValue) ?? {};
  const overrideMap = asRecord(overrideValue);
  const active = overrideMap ?? {};
  const tierLabel = TIER_LABELS[tier] ?? tier;
  return (
    <div className="sp-effort">
      <div className="sp-effort__head">
        <span className="sp-effort__title">{tierLabel} 随档推理映射</span>
        <span className="sp-row__caption">全局</span>
        <span className="ov-mono">
          {BY_LEVELS.map(
            (level) =>
              `${level}: ${String(
                (asRecord(globalMap[level]) ?? {}).default ?? "—",
              )}`,
          ).join(" · ")}
        </span>
      </div>
      <div className="sp-effort__grid">
        {BY_LEVELS.map((level) => {
          const current =
            (asRecord(active[level]) ?? {}).default ?? "";
          const inherited =
            (asRecord(globalMap[level]) ?? {}).default ?? "";
          return (
            <label className="sp-effort__field" key={level}>
              <span>{level} default</span>
              <select
                className="ov-select ov-select--sm"
                aria-label={`${tierLabel} 随档推理 ${level}`}
                value={typeof current === "string" ? current : ""}
                disabled={disabled}
                onChange={(event) => {
                  const base = overrideMap
                    ? { ...cloneJson(globalMap), ...cloneJson(overrideMap) }
                    : cloneJson(globalMap);
                  if (event.target.value) {
                    base[level] = {
                      ...(asRecord(base[level]) ?? {}),
                      default: event.target.value,
                    };
                  } else {
                    delete base[level];
                  }
                  onChange(base);
                }}
              >
                <option value="">
                  {inherited ? `继承（${inherited}）` : "继承"}
                </option>
                {capabilities.map((effort) => (
                  <option value={effort} key={effort}>
                    {effort}
                  </option>
                ))}
              </select>
            </label>
          );
        })}
      </div>
      <div className="sp-effort__foot">
        <span className="sp-row__effective">
          生效：{formatInline(effectiveValue)}
        </span>
        <button
          type="button"
          className="ov-btn ov-btn--sm"
          disabled={disabled || !overrideMap}
          onClick={onClear}
        >
          恢复继承
        </button>
      </div>
    </div>
  );
}
