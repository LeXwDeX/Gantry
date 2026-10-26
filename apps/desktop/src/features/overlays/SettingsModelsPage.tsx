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
  Project,
  SettingsCatalog,
  SettingsCatalogModel,
} from "@/api/types";
import type { GuardedWriteArgs, GuardedWritePreview } from "@/api/shell";
import { isScenarioAdapter } from "@/ipc";
import { useGuardedWrite } from "./useGuardedWrite";
import {
  TIERS,
  buildModelsValue,
  catalogModels,
  coversTier,
  draftFromModels,
  effortChoices,
  effortLabel,
  etagFrom,
  ipcErrorText,
  issueText,
  issuesAt,
  modelKey,
  modelsFingerprint,
  parseApiFailure,
  parseModels,
  projectLabel,
  splitModelKey,
  type LoadedModels,
  type ModelTier,
  type ModelsDraft,
  type ProjectModelsCoverage,
  type TierDraft,
} from "./settingsData";

export interface SettingsModelsPageProps {
  controller: DesktopController;
  state: DesktopState;
  onDirtyChange: (dirty: boolean) => void;
}

interface LoadState {
  status: "loading" | "ready" | "error";
  catalog: SettingsCatalog | null;
  models: LoadedModels | null;
  coverage: ProjectModelsCoverage[];
  error: string | null;
}

const LOADING: LoadState = {
  status: "loading",
  catalog: null,
  models: null,
  coverage: [],
  error: null,
};

const LEVEL_ROWS = [
  ["None", "理解意图", "只读沙盒中回答涉及代码的问题", "回答状态类问题"],
  ["Low", "理解意图", "单节点实现", "提交说明、状态摘要"],
  ["Medium", "理解意图、规划", "build / verify、单 reviewer", "状态清单"],
  ["High", "+ 需求准入、汇总 review", "实现 + 多角色 review", "事件摘要"],
  ["Max", "+ reasoner 预演、arbiter 裁决", "实现 + 对抗 review", "事件摘要"],
  [
    "Ultimate",
    "+ 拆 Milestone、每个 PR 裁决",
    "多交付并行，每个按 Max",
    "事件摘要",
  ],
] as const;

export function SettingsModelsPage({
  controller,
  state,
  onDirtyChange,
}: SettingsModelsPageProps): ReactNode {
  const [load, setLoad] = useState<LoadState>(LOADING);
  const [draft, setDraft] = useState<ModelsDraft | null>(null);
  const [baseline, setBaseline] = useState("");
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [secretSpent, setSecretSpent] = useState(false);
  const [needsReentry, setNeedsReentry] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [saved, setSaved] = useState<string | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  const scenario = isScenarioAdapter(controller.ipc);
  const baselineRevision = useRef(state.settingsRevision);
  const projectsRef = useRef<Project[]>(state.projects);
  const editVersionRef = useRef(0);
  const write = useGuardedWrite(controller.ipc);
  const { preview, phase, error, prepare, submit, discard, clear } = write;

  useEffect(() => {
    projectsRef.current = state.projects;
  }, [state.projects]);

  const reload = useCallback(
    async (projects: Project[]): Promise<void> => {
      setLoad((prev) => ({ ...prev, status: "loading", error: null }));
      setConflict(false);
      setSaved(null);
      setLocalError(null);
      try {
        const [catalogResult, modelsResult] = await Promise.all([
          controller.ipc.api("settings.catalog", {}),
          controller.ipc.api("settings.get", { resource: "models" }),
        ]);
        if (!catalogResult.ok) {
          setLoad({
            status: "error",
            catalog: null,
            models: null,
            coverage: [],
            error: parseApiFailure(catalogResult),
          });
          return;
        }
        if (!modelsResult.ok || !modelsResult.body) {
          setLoad({
            status: "error",
            catalog: catalogResult.body,
            models: null,
            coverage: [],
            error: modelsResult.ok
              ? "服务端没有返回模型设置。"
              : parseApiFailure(modelsResult),
          });
          return;
        }

        const coverage = await Promise.all(
          projects.map(async (project): Promise<ProjectModelsCoverage> => {
            try {
              const result = await controller.ipc.api("project_settings.get", {
                projectId: project.id,
                resource: "models",
              });
              if (!result.ok || !result.body) {
                return {
                  projectId: project.id,
                  repo: project.repo,
                  overrides: [],
                  failed: true,
                };
              }
              return {
                projectId: project.id,
                repo: project.repo,
                overrides: TIERS.map((tier) => tier.id).filter((tier) =>
                  coversTier(result.body, tier),
                ),
                failed: false,
              };
            } catch {
              return {
                projectId: project.id,
                repo: project.repo,
                overrides: [],
                failed: true,
              };
            }
          }),
        );

        const value = parseModels(modelsResult.body.value);
        const nextDraft = draftFromModels(value);
        setLoad({
          status: "ready",
          catalog: catalogResult.body,
          models: {
            doc: modelsResult.body,
            etag: etagFrom(modelsResult),
            value,
          },
          coverage,
          error: null,
        });
        setDraft(nextDraft);
        setBaseline(modelsFingerprint(nextDraft));
        setSecrets({});
        setSecretSpent(false);
        setNeedsReentry(false);
        baselineRevision.current = controller.settingsRevision;
      } catch (failure) {
        setLoad({
          status: "error",
          catalog: null,
          models: null,
          coverage: [],
          error: ipcErrorText(failure),
        });
      }
    },
    [controller],
  );

  useEffect(() => {
    void reload(projectsRef.current);
  }, [reload]);

  const revision = state.settingsRevision;
  useEffect(() => {
    if (load.status === "ready" && revision !== baselineRevision.current) {
      setConflict(true);
      clear();
    }
  }, [clear, revision, load.status]);

  const modelDirty =
    draft !== null && baseline !== "" && modelsFingerprint(draft) !== baseline;
  const hasSecrets = Object.values(secrets).some(Boolean);
  const dirty = modelDirty || hasSecrets || preview !== null || needsReentry;

  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);

  const editTier = useCallback(
    (tier: ModelTier, patch: Partial<TierDraft>) => {
      if (phase === "submitting") return;
      editVersionRef.current += 1;
      discard();
      if (secretSpent) setNeedsReentry(true);
      setSaved(null);
      setLocalError(null);
      setDraft((prev) =>
        prev ? { ...prev, [tier]: { ...prev[tier], ...patch } } : prev,
      );
    },
    [discard, phase, secretSpent],
  );

  const chooseModel = useCallback(
    (tier: ModelTier, key: string) => {
      const catalog = load.catalog;
      if (!draft || !catalog) return;
      const { provider, model } = splitModelKey(key);
      const capability = catalog.models.capabilities[key];
      const current = draft[tier];
      const allowByLevel = tier !== "utility";
      const options = effortChoices(catalog, key, "", allowByLevel);
      const keep =
        current.effort === "by_level"
          ? allowByLevel
          : Boolean(capability?.reasoning_efforts.includes(current.effort));
      editTier(tier, {
        provider,
        model,
        effort: keep ? current.effort : (options[0] ?? ""),
      });
    },
    [draft, editTier, load.catalog],
  );

  const editSecret = useCallback(
    (providerId: string, value: string) => {
      if (phase === "submitting") return;
      editVersionRef.current += 1;
      discard();
      setNeedsReentry(false);
      setSaved(null);
      setLocalError(null);
      setSecrets((prev) => ({ ...prev, [providerId]: value }));
    },
    [discard, phase],
  );

  const handleSave = useCallback(async () => {
    if (!load.models || !draft) return;
    if (conflict) {
      setLocalError("此页已被其他设备修改；请先载入最新值，不能自动合并。");
      return;
    }
    if (needsReentry) {
      setLocalError("凭据值已在准备时清除，请重新输入凭据后再保存。");
      return;
    }
    const args: GuardedWriteArgs = {
      kind: "settings_put",
      scope: "global",
      resource: "models",
      value: buildModelsValue(load.models.value.root, draft, secrets),
      ifMatch: load.models.etag,
    };
    const hadSecret = Object.values(secrets).some(Boolean);
    setLocalError(null);
    setSaved(null);
    const version = editVersionRef.current;
    const request = prepare(args);
    setSecrets({});
    if (hadSecret) setSecretSpent(true);
    const result = await request;
    if (editVersionRef.current !== version) {
      discard();
      if (hadSecret) setNeedsReentry(true);
      setLocalError("准备期间草稿已变化，旧预览已丢弃；请重新保存。");
      return;
    }
    if (hadSecret && (!result || !result.valid)) {
      setNeedsReentry(true);
      setLocalError("凭据值已从界面清除，请重新输入凭据后再保存。");
    }
  }, [conflict, discard, draft, load.models, needsReentry, prepare, secrets]);

  const handleSubmit = useCallback(async () => {
    if (!preview || !preview.can_submit.ok) return;
    if (conflict) {
      setLocalError("此页已被其他设备修改；不能提交旧预览，请先载入最新值。");
      return;
    }
    const result = await submit();
    if (!result) {
      if (secretSpent) setNeedsReentry(true);
      return;
    }
    if (result.ok) {
      const rev = result.headers.x_event_seq;
      await reload(projectsRef.current);
      setSaved(
        scenario
          ? rev
            ? `模拟设置已应用（rev ${rev}），已重新读取。仅写入开发场景内存。新配置从运行中节点的下一个节点开始生效。`
            : "模拟设置已应用，已重新读取。仅写入开发场景内存。新配置从运行中节点的下一个节点开始生效。"
          : rev
            ? `已保存（rev ${rev}）。新配置从运行中节点的下一个节点开始生效。`
            : "已保存。新配置从运行中节点的下一个节点开始生效。",
      );
      setSecretSpent(false);
      setNeedsReentry(false);
      return;
    }
    if (
      "status" in result &&
      (result.status === 412 || result.problem.code === "precondition_failed")
    ) {
      setConflict(true);
      setNeedsReentry(secretSpent);
      return;
    }
    if (secretSpent) setNeedsReentry(true);
  }, [conflict, preview, reload, scenario, secretSpent, submit]);

  const handleReload = useCallback(async () => {
    clear();
    await reload(projectsRef.current);
  }, [clear, reload]);

  if (load.status === "loading") {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">模型</h1>
        <p role="status" className="ov-settings__muted">
          正在载入模型设置…
        </p>
      </section>
    );
  }

  if (load.status === "error" || !load.models || !load.catalog || !draft) {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">模型</h1>
        <div role="alert" className="ov-alert">
          {load.error ?? "模型设置不可用。"}
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => void reload(projectsRef.current)}
          >
            重试
          </button>
        </div>
      </section>
    );
  }

  const errors = preview?.errors ?? [];
  const warnings = preview?.warnings ?? [];
  const relaxes = preview?.relaxes ?? [];
  const submitBlocked =
    preview && !preview.can_submit.ok
      ? submitBlockReason(preview.can_submit.reason)
      : null;

  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">模型</h1>
        <p className="ov-settings__muted">
          三类模型分工，保存在服务端，对所有项目生效；项目可在左侧「项目」里覆盖。提供方凭据只存在服务端，桌面端不保存。
        </p>
        {scenario ? (
          <p className="ov-notice ov-notice--info" role="status">
            开发场景：保存预览与提交只修改内存模型设置并立即回读；不产生真实签名、凭据或网络写入。
          </p>
        ) : null}
      </div>

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

      <div className="ov-models__grid">
        {TIERS.map((meta) => {
          const tier = draft[meta.id];
          const key = modelKey(tier);
          const choices = catalogModels(load.catalog, meta.id);
          if (key && !choices.some((choice) => choice.key === key)) {
            choices.unshift({
              key,
              capability: {
                tools: meta.id !== "utility",
                reasoning_efforts: tier.effort ? [tier.effort] : [],
              } as SettingsCatalogModel,
            });
          }
          const allowByLevel = meta.id !== "utility";
          const efforts = effortChoices(
            load.catalog,
            key,
            tier.effort,
            allowByLevel,
          );
          const covered = load.coverage.filter((project) =>
            project.overrides.includes(meta.id),
          );
          return (
            <section
              className={`ov-tier ov-tier--${meta.id}`}
              key={meta.id}
              aria-label={meta.name}
            >
              <div className="ov-tier__head">
                <span className="ov-tier__badge">{meta.short}</span>
                <span className="ov-tier__name">{meta.name}</span>
              </div>
              <span className="ov-tier__desc">{meta.description}</span>

              <label className="ov-field">
                <span>模型</span>
                <select
                  className="ov-select ov-select--mono"
                  aria-label={`${meta.name} 模型`}
                  value={key}
                  onChange={(event) => chooseModel(meta.id, event.target.value)}
                >
                  {key ? null : <option value="">未配置</option>}
                  {choices.map((choice) => (
                    <option value={choice.key} key={choice.key}>
                      {choice.key}
                      {load.catalog?.models.capabilities[choice.key]
                        ? ""
                        : "（目录未列出）"}
                    </option>
                  ))}
                </select>
              </label>
              <FieldIssues
                issues={[
                  ...issuesAt(errors, `${meta.id}/provider`),
                  ...issuesAt(errors, `${meta.id}/model`),
                ]}
              />

              <label className="ov-field">
                <span>推理深度</span>
                <select
                  className="ov-select"
                  aria-label={`${meta.name} 推理深度`}
                  value={tier.effort}
                  disabled={efforts.length === 0}
                  onChange={(event) =>
                    editTier(meta.id, { effort: event.target.value })
                  }
                >
                  {tier.effort ? null : <option value="">未配置</option>}
                  {efforts.map((effort) => (
                    <option value={effort} key={effort}>
                      {effortLabel(effort)}
                      {modelSupports(
                        load.catalog,
                        key,
                        effort,
                        allowByLevel,
                      )
                        ? ""
                        : "（目录未列出）"}
                    </option>
                  ))}
                </select>
              </label>
              <FieldIssues
                issues={issuesAt(errors, `${meta.id}/reasoning_effort`)}
              />
              {tier.effort === "by_level" && !tier.hasByLevel ? (
                <span className="ov-field-error" role="status">
                  没有已保存的档位映射；本页不生成默认值，提交可能被服务端拒绝。
                </span>
              ) : null}

              <span className="ov-tier__coverage">
                {covered.length > 0
                  ? `被 ${covered.length} 个项目覆盖：${covered
                      .map((project) =>
                        projectLabel(project.repo, project.projectId),
                      )
                      .join("、")}`
                  : "未被项目覆盖"}
              </span>
              {load.coverage.some((project) => project.failed) ? (
                <span className="ov-tier__coverage">
                  {
                    load.coverage.filter((project) => project.failed).length
                  }{" "}
                  个项目的覆盖读取失败。
                </span>
              ) : null}
            </section>
          );
        })}
      </div>

      <section className="ov-settings__section">
        <h2 className="ov-settings__h2">提供方凭据（仅服务端）</h2>
        <p className="ov-settings__muted">
          凭据值只写入不读取。输入值仅存在于本组件的局部状态；保存预览发出后立即清空，不进入界面状态存储、日志或持久化。已设置的值永远不显示。
          {scenario
            ? " 开发场景只接受测试占位值（含 test / example 等字样），请勿输入真实凭据。"
            : ""}
        </p>
        {load.models.value.providers.length === 0 ? (
          <p className="ov-settings__muted">服务端未配置模型提供方。</p>
        ) : (
          <div className="ov-credentials">
            {load.models.value.providers.map((provider) => (
              <div className="ov-credential" key={provider.id}>
                <div className="ov-credential__head">
                  <span className="ov-mono">{provider.id}</span>
                  <span className="ov-settings__meta">
                    {provider.kind || "未知类型"}
                    {provider.baseUrl ? ` · ${provider.baseUrl}` : ""}
                  </span>
                </div>
                <span className="ov-settings__meta">
                  {provider.credential
                    ? `引用 ${provider.secretRef || "未命名"} · ${
                        provider.present ? "已设置 ●●●●" : "未设置"
                      }`
                    : "未配置凭据引用；本页不创建提供方。"}
                </span>
                <label className="ov-field">
                  <span>API key（只写）</span>
                  <input
                    type="password"
                    className="ov-input"
                    aria-label={`${provider.id} API key`}
                    autoComplete="new-password"
                    spellCheck={false}
                    disabled={!provider.credential}
                    value={secrets[provider.id] ?? ""}
                    placeholder={
                      provider.present
                        ? "已设置 ●●●●，输入新值以轮换"
                        : "输入 API key"
                    }
                    onChange={(event) =>
                      editSecret(provider.id, event.target.value)
                    }
                  />
                </label>
                <span className="ov-settings__meta">
                  {scenario
                    ? "仅测试占位值，勿输入真实凭据；准备后立即从界面清除。"
                    : "值只写不读；准备后立即从界面清除，不进入日志或持久化。"}
                </span>
                <FieldIssues
                  issues={issuesAt(
                    errors,
                    `providers/${provider.id}/credential/value`,
                  )}
                />
              </div>
            ))}
          </div>
        )}
      </section>

      <section className="ov-settings__section">
        <h2 className="ov-settings__h2">
          每个档位里三类模型各做什么
        </h2>
        <div className="ov-table-wrap" tabIndex={0} role="region" aria-label="档位与模型分工">
          <table className="ov-table">
            <thead>
              <tr>
                <th>档位</th>
                <th>大脑</th>
                <th>干活</th>
                <th>打杂</th>
              </tr>
            </thead>
            <tbody>
              {LEVEL_ROWS.map((row) => (
                <tr key={row[0]}>
                  <td className="ov-mono">{row[0]}</td>
                  <td>{row[1]}</td>
                  <td>{row[2]}</td>
                  <td>{row[3]}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      <section className="ov-save">
        <p className="ov-settings__muted">
          涉及凭据或放宽安全的改动，保存时需要审批设备签名；新配置从下一个节点开始生效，运行中的节点不受影响。
        </p>
        {scenario ? (
          <p className="ov-settings__meta">
            开发场景：保存与提交只写入内存并立即回读；不产生真实签名或凭据。生产仍走外壳的
            dry-run 与审批签名。
          </p>
        ) : null}

        {localError ? (
          <div role="alert" className="ov-alert">
            {localError}
          </div>
        ) : null}
        {error && !conflict ? (
          <div role="alert" className="ov-alert">
            {error}
          </div>
        ) : null}
        {errors.length > 0 ? (
          <div role="alert" className="ov-alert">
            预览未通过校验：{errors.length} 项。
            {errors
              .filter((issue) => !isKnownPointer(issue.pointer))
              .map((issue) => (
                <span className="ov-issues__item" key={`${issue.pointer}-${issue.code}`}>
                  {issue.pointer || "（整体）"}：{issueText(issue)}
                </span>
              ))}
          </div>
        ) : null}
        {warnings.length > 0 ? (
          <div role="status" className="ov-notice ov-notice--warn">
            {warnings.map((issue) => (
              <span className="ov-issues__item" key={`${issue.pointer}-${issue.code}`}>
                {issueText(issue)}
              </span>
            ))}
          </div>
        ) : null}
        {relaxes.length > 0 ? (
          <div role="status" className="ov-notice ov-notice--warn">
            此改动放宽了安全限制：{relaxes.join("、")}
          </div>
        ) : null}
        {preview?.requires_signature ? (
          <div role="status" className="ov-notice ov-notice--info">
            此改动需要审批设备签名。提交时由外壳发起系统确认；本机不是审批设备时无法完成。
          </div>
        ) : null}
        {needsReentry ? (
          <div role="status" className="ov-notice ov-notice--warn">
            凭据值已在准备时清除；请重新输入凭据后再保存。
          </div>
        ) : null}
        {submitBlocked ? (
          <div role="status" className="ov-notice ov-notice--warn">
            {submitBlocked}
          </div>
        ) : null}
        {saved ? (
          <div role="status" className="ov-notice ov-notice--ok">
            {saved}
          </div>
        ) : null}

        <div className="ov-save__actions">
          {preview ? (
            <button
              type="button"
              className="ov-btn ov-btn--primary"
              disabled={!preview.can_submit.ok || phase !== "idle" || conflict}
              onClick={() => void handleSubmit()}
            >
              {scenario
                ? "模拟应用设置"
                : preview.requires_signature
                  ? "签名并提交…"
                  : "确认提交"}
            </button>
          ) : (
            <button
              type="button"
              className="ov-btn ov-btn--primary"
              disabled={
                dirty === false ||
                phase !== "idle" ||
                conflict ||
                needsReentry
              }
              onClick={() => void handleSave()}
            >
              保存
            </button>
          )}
          {preview ? (
            <button
              type="button"
              className="ov-btn"
              disabled={phase !== "idle"}
              onClick={clear}
            >
              取消预览
            </button>
          ) : null}
          {preview && preview.errors.length === 0 ? (
            <span className="ov-settings__meta">
              外壳持有本次请求体（含凭据值）；界面只保存 pending_id。
            </span>
          ) : null}
        </div>
        {load.models.doc.rev ? (
          <span className="ov-settings__meta">
            当前版本 rev {load.models.doc.rev} · ETag {load.models.etag}
            {load.models.doc.warnings && load.models.doc.warnings.length > 0
              ? ` · 载入时有 ${load.models.doc.warnings.length} 条服务端警告`
              : ""}
          </span>
        ) : null}
      </section>
    </section>
  );
}

function FieldIssues({
  issues,
}: {
  issues: import("@/api/types").SettingsIssue[];
}): ReactNode {
  if (issues.length === 0) return null;
  return (
    <span className="ov-field-error" role="alert">
      {issues.map((issue) => (
        <span key={`${issue.pointer}-${issue.code}`}>{issueText(issue)}</span>
      ))}
    </span>
  );
}

function modelSupports(
  catalog: SettingsCatalog | null,
  key: string,
  effort: string,
  allowByLevel: boolean,
): boolean {
  if (effort === "by_level") return allowByLevel;
  if (!catalog) return true;
  const capability = catalog.models.capabilities[key];
  if (!capability) return true;
  return capability.reasoning_efforts.includes(effort);
}

function isKnownPointer(pointer: string): boolean {
  return (
    /^\/(planner|worker|utility)\/(provider|model|reasoning_effort)$/.test(
      pointer,
    ) || /^\/providers\/[^/]+\/credential\/value$/.test(pointer)
  );
}

function submitBlockReason(
  reason: "not_connected" | "not_approver" | "invalid",
): string {
  switch (reason) {
    case "not_connected":
      return "无法提交：外壳未连接；真实写入不会发生。";
    case "not_approver":
      return "无法提交：此改动需要审批设备签名，而本机不是审批设备。";
    case "invalid":
      return "无法提交：预览未通过校验。";
  }
}
