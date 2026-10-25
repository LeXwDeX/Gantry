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
  Level,
  SettingsCatalog,
  SettingsDocument,
} from "@/api/types";
import type { GuardedWriteArgs, GuardedWritePreview } from "@/api/shell";
import { useGuardedWrite } from "./useGuardedWrite";
import {
  asRecord,
  asString,
  catalogModels,
  etagFrom,
  ipcErrorText,
  issueText,
  issuesAt,
  modelKey,
  parseApiFailure,
  parseModels,
  projectLabel,
  readTier,
  splitModelKey,
} from "./settingsData";

export interface SettingsProjectPageProps {
  controller: DesktopController;
  state: DesktopState;
  projectId: string;
  onDirtyChange: (dirty: boolean) => void;
}

interface ProjectSnapshot {
  levelsGlobal: SettingsDocument;
  levelsOverride: SettingsDocument;
  levelsEffective: SettingsDocument;
  modelsGlobal: SettingsDocument;
  modelsOverride: SettingsDocument;
  modelsEffective: SettingsDocument;
  levelsEtag: string;
  modelsEtag: string;
  effectiveLevel: string;
  effectiveWorker: string;
  clamped: string[];
}

interface Load {
  status: "loading" | "ready" | "error";
  snapshot: ProjectSnapshot | null;
  catalog: SettingsCatalog | null;
  error: string | null;
}

const LEVELS: Level[] = ["None", "Low", "Medium", "High", "Max", "Ultimate"];

export function SettingsProjectPage({
  controller,
  state,
  projectId,
  onDirtyChange,
}: SettingsProjectPageProps): ReactNode {
  const project = state.projects.find((item) => item.id === projectId) ?? null;
  const [load, setLoad] = useState<Load>({
    status: "loading",
    snapshot: null,
    catalog: null,
    error: null,
  });
  const [levelDraft, setLevelDraft] = useState("");
  const [workerDraft, setWorkerDraft] = useState("");
  const [levelBaseline, setLevelBaseline] = useState("");
  const [workerBaseline, setWorkerBaseline] = useState("");
  const [saved, setSaved] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const baselineRevision = useRef(state.settingsRevision);
  const editVersionRef = useRef(0);
  const levelWrite = useGuardedWrite(controller.ipc);
  const modelWrite = useGuardedWrite(controller.ipc);

  const reload = useCallback(async (): Promise<void> => {
    setLoad((prev) => ({ ...prev, status: "loading", error: null }));
    setConflict(false);
    setSaved(null);
    setLocalError(null);
    try {
      const [
        catalogResult,
        globalLevels,
        globalModels,
        overrideLevels,
        effectiveLevels,
        overrideModels,
        effectiveModels,
      ] = await Promise.all([
        controller.ipc.api("settings.catalog", {}),
        controller.ipc.api("settings.get", { resource: "levels" }),
        controller.ipc.api("settings.get", { resource: "models" }),
        controller.ipc.api("project_settings.get", {
          projectId,
          resource: "levels",
        }),
        controller.ipc.api("project_settings.get", {
          projectId,
          resource: "levels",
          view: "effective",
        }),
        controller.ipc.api("project_settings.get", {
          projectId,
          resource: "models",
        }),
        controller.ipc.api("project_settings.get", {
          projectId,
          resource: "models",
          view: "effective",
        }),
      ]);
      const fail = (message: string): void => {
        setLoad({
          status: "error",
          snapshot: null,
          catalog: null,
          error: message,
        });
      };
      if (!catalogResult.ok) return fail(parseApiFailure(catalogResult));
      if (!globalLevels.ok) return fail(parseApiFailure(globalLevels));
      if (!globalModels.ok) return fail(parseApiFailure(globalModels));
      if (!overrideLevels.ok) return fail(parseApiFailure(overrideLevels));
      if (!effectiveLevels.ok) return fail(parseApiFailure(effectiveLevels));
      if (!overrideModels.ok) return fail(parseApiFailure(overrideModels));
      if (!effectiveModels.ok) return fail(parseApiFailure(effectiveModels));
      if (
        !catalogResult.body ||
        !globalLevels.body ||
        !globalModels.body ||
        !overrideLevels.body ||
        !effectiveLevels.body ||
        !overrideModels.body ||
        !effectiveModels.body
      ) {
        return fail("项目设置响应不完整。");
      }

      const overrideLevelsValue = asRecord(overrideLevels.body.value) ?? {};
      const overrideModelsValue = asRecord(overrideModels.body.value) ?? {};
      const effectiveLevelValue = asRecord(effectiveLevels.body.value) ?? {};
      const effectiveModelsValue = parseModels(effectiveModels.body.value);
      const worker = readTier(overrideModelsValue, "worker");
      const nextLevelDraft = asString(overrideLevelsValue.default_level);
      const nextWorkerDraft = worker.model ? modelKey(worker) : "";
      setLoad({
        status: "ready",
        snapshot: {
          levelsGlobal: globalLevels.body,
          levelsOverride: overrideLevels.body,
          levelsEffective: effectiveLevels.body,
          modelsGlobal: globalModels.body,
          modelsOverride: overrideModels.body,
          modelsEffective: effectiveModels.body,
          levelsEtag: etagFrom(overrideLevels),
          modelsEtag: etagFrom(overrideModels),
          effectiveLevel: asString(effectiveLevelValue.default_level),
          effectiveWorker: modelKey(effectiveModelsValue.tiers.worker),
          clamped: [
            ...(effectiveLevels.body.clamped ?? []),
            ...(effectiveModels.body.clamped ?? []),
          ],
        },
        catalog: catalogResult.body,
        error: null,
      });
      setLevelDraft(nextLevelDraft);
      setWorkerDraft(nextWorkerDraft);
      setLevelBaseline(nextLevelDraft);
      setWorkerBaseline(nextWorkerDraft);
      baselineRevision.current = controller.settingsRevision;
    } catch (failure) {
      setLoad({
        status: "error",
        snapshot: null,
        catalog: null,
        error: ipcErrorText(failure),
      });
    }
  }, [controller, projectId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const revision = state.settingsRevision;
  useEffect(() => {
    if (load.status === "ready" && revision !== baselineRevision.current) {
      setConflict(true);
      levelWrite.clear();
      modelWrite.clear();
    }
  }, [levelWrite, modelWrite, revision, load.status]);

  const levelDirty = levelDraft !== levelBaseline;
  const workerDirty = workerDraft !== workerBaseline;
  const dirty =
    levelDirty ||
    workerDirty ||
    levelWrite.preview !== null ||
    modelWrite.preview !== null;
  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);

  const editLevel = useCallback(
    (value: string) => {
      if (levelWrite.phase === "submitting" || modelWrite.phase === "submitting") return;
      editVersionRef.current += 1;
      levelWrite.discard();
      setSaved(null);
      setLocalError(null);
      setLevelDraft(value);
    },
    [levelWrite, modelWrite.phase],
  );

  const editWorker = useCallback(
    (value: string) => {
      if (levelWrite.phase === "submitting" || modelWrite.phase === "submitting") return;
      editVersionRef.current += 1;
      modelWrite.discard();
      setSaved(null);
      setLocalError(null);
      setWorkerDraft(value);
    },
    [levelWrite.phase, modelWrite],
  );

  const handleLevelSave = useCallback(async (): Promise<void> => {
    if (!load.snapshot) return;
    if (workerDirty) {
      setLocalError("干活模型有未保存改动；请先保存或放弃它，再保存档位。");
      return;
    }
    const override = asRecord(load.snapshot.levelsOverride.value) ?? {};
    const value = { ...override };
    if (levelDraft) value.default_level = levelDraft;
    else delete value.default_level;
    const args: GuardedWriteArgs =
      Object.keys(value).length === 0
        ? {
            kind: "settings_delete",
            projectId,
            resource: "levels",
            ifMatch: load.snapshot.levelsEtag,
          }
        : {
            kind: "settings_put",
            scope: "project",
            projectId,
            resource: "levels",
            value,
            ifMatch: load.snapshot.levelsEtag,
          };
    const version = editVersionRef.current;
    const result = await levelWrite.prepare(args);
    if (editVersionRef.current !== version) {
      levelWrite.clear();
      setLocalError("准备期间草稿已变化，旧预览已丢弃；请重新保存。");
      return;
    }
    if (result) setLocalError(null);
  }, [levelDraft, levelWrite, load.snapshot, projectId, workerDirty]);

  const handleWorkerSave = useCallback(async (): Promise<void> => {
    if (!load.snapshot) return;
    if (levelDirty) {
      setLocalError("档位上限有未保存改动；请先保存或放弃它，再保存干活模型。");
      return;
    }
    const override = asRecord(load.snapshot.modelsOverride.value) ?? {};
    const value: Record<string, unknown> = { ...override };
    const worker = { ...(asRecord(value.worker) ?? {}) };
    if (workerDraft) {
      const { provider, model } = splitModelKey(workerDraft);
      worker.provider = provider;
      worker.model = model;
      value.worker = worker;
    } else {
      delete worker.provider;
      delete worker.model;
      if (Object.keys(worker).length === 0) delete value.worker;
      else value.worker = worker;
    }
    const args: GuardedWriteArgs =
      Object.keys(value).length === 0
        ? {
            kind: "settings_delete",
            projectId,
            resource: "models",
            ifMatch: load.snapshot.modelsEtag,
          }
        : {
            kind: "settings_put",
            scope: "project",
            projectId,
            resource: "models",
            value,
            ifMatch: load.snapshot.modelsEtag,
          };
    const version = editVersionRef.current;
    const result = await modelWrite.prepare(args);
    if (editVersionRef.current !== version) {
      modelWrite.clear();
      setLocalError("准备期间草稿已变化，旧预览已丢弃；请重新保存。");
      return;
    }
    if (result) setLocalError(null);
  }, [levelDirty, load.snapshot, modelWrite, projectId, workerDraft]);

  const submitLevel = useCallback(async () => {
    if (workerDirty) {
      setLocalError("干活模型有未保存改动；已取消档位预览，请先处理。");
      levelWrite.clear();
      return;
    }
    const result = await levelWrite.submit();
    if (!result) return;
    if (result.ok) {
      const rev = result.headers.x_event_seq;
      await reload();
      setSaved(rev ? `已保存（rev ${rev}）。` : "已保存。");
    } else if (
      "status" in result &&
      (result.status === 412 || result.problem.code === "precondition_failed")
    ) {
      setConflict(true);
    }
  }, [levelWrite, reload, workerDirty]);

  const submitWorker = useCallback(async () => {
    if (levelDirty) {
      setLocalError("档位上限有未保存改动；已取消模型预览，请先处理。");
      modelWrite.clear();
      return;
    }
    const result = await modelWrite.submit();
    if (!result) return;
    if (result.ok) {
      const rev = result.headers.x_event_seq;
      await reload();
      setSaved(rev ? `已保存（rev ${rev}）。` : "已保存。");
    } else if (
      "status" in result &&
      (result.status === 412 || result.problem.code === "precondition_failed")
    ) {
      setConflict(true);
    }
  }, [levelDirty, modelWrite, reload]);

  const handleReload = useCallback(async () => {
    levelWrite.clear();
    modelWrite.clear();
    await reload();
  }, [levelWrite, modelWrite, reload]);

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
          正在载入项目设置…
        </p>
      </section>
    );
  }

  if (load.status === "error" || !load.snapshot || !load.catalog) {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">
          项目 · <span className="ov-mono">{projectLabel(project.repo, project.id)}</span>
        </h1>
        <div role="alert" className="ov-alert">
          {load.error ?? "项目设置不可用。"}
          <button type="button" className="ov-btn ov-btn--sm" onClick={() => void reload()}>
            重试
          </button>
        </div>
      </section>
    );
  }

  const snapshot = load.snapshot;
  const globalLevel = readLevel(snapshot.levelsGlobal);
  const globalWorker = readTier(
    asRecord(snapshot.modelsGlobal.value) ?? {},
    "worker",
  );
  const workerOptions = catalogModels(load.catalog, "worker");
  const currentWorker = readTier(
    asRecord(snapshot.modelsOverride.value) ?? {},
    "worker",
  );
  const workerChoices = [...workerOptions];
  if (
    workerDraft &&
    !workerChoices.some((choice) => choice.key === workerDraft)
  ) {
    workerChoices.unshift({
      key: workerDraft,
      capability: {
        tools: true,
        reasoning_efforts: currentWorker.effort
          ? [currentWorker.effort]
          : [],
      },
    });
  }

  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">
          项目 · <span className="ov-mono">{projectLabel(project.repo, project.id)}</span>
        </h1>
        <p className="ov-settings__muted">
          这里只列项目可以覆盖的键。未覆盖的继承全局；安全相关的键只能收紧，不能放宽。预算模板与受保护路径不在本页提供（config.md §2.2；本版本不做伪配置）。
        </p>
      </div>

      {localError ? (
        <div role="alert" className="ov-alert">
          {localError}
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
      {snapshot.clamped.length > 0 ? (
        <div role="status" className="ov-notice ov-notice--warn">
          生效值被全局约束收紧：{snapshot.clamped.join("、")}
        </div>
      ) : null}
      {saved ? (
        <div role="status" className="ov-notice ov-notice--ok">
          {saved}
        </div>
      ) : null}

      <div
        className="ov-table-wrap"
        tabIndex={0}
        role="region"
        aria-label="项目档位与模型覆盖"
      >
        <table className="ov-table">
          <thead>
            <tr>
              <th>设置</th>
              <th>全局</th>
              <th>本项目</th>
              <th>生效值</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td>默认档位上限</td>
              <td className="ov-mono">{globalLevel}</td>
              <td>
                <select
                  className="ov-select ov-select--sm"
                  aria-label="默认档位上限 项目覆盖"
                  value={levelDraft}
                  onChange={(event) => editLevel(event.target.value)}
                >
                  <option value="">继承全局</option>
                  {LEVELS.map((level) => (
                    <option value={level} key={level}>
                      {level}
                    </option>
                  ))}
                </select>
                <FieldIssues
                  issues={issuesAt(
                    levelWrite.preview?.errors ?? [],
                    "default_level",
                  )}
                />
              </td>
              <td className="ov-mono">
                {snapshot.effectiveLevel || "未配置"}
              </td>
            </tr>
            <tr>
              <td>干活模型</td>
              <td className="ov-mono">{modelKey(globalWorker) || "未配置"}</td>
              <td>
                <select
                  className="ov-select ov-select--sm"
                  aria-label="干活模型 项目覆盖"
                  value={workerDraft}
                  onChange={(event) => editWorker(event.target.value)}
                >
                  <option value="">继承全局</option>
                  {workerChoices.map((choice) => (
                    <option value={choice.key} key={choice.key}>
                      {choice.key}
                      {load.catalog?.models.capabilities[choice.key]
                        ? ""
                        : "（目录未列出）"}
                    </option>
                  ))}
                </select>
                <FieldIssues
                  issues={issuesAt(
                    modelWrite.preview?.errors ?? [],
                    "worker/model",
                  )}
                />
              </td>
              <td className="ov-mono">
                {snapshot.effectiveWorker || "未配置"}
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <div className="ov-settings__section">
        <div className="ov-save__actions">
          <button
            type="button"
            className="ov-btn ov-btn--primary"
            disabled={
              !levelDirty ||
              levelWrite.phase !== "idle" ||
              conflict ||
              levelWrite.preview !== null
            }
            onClick={() => void handleLevelSave()}
          >
            保存档位
          </button>
          <button
            type="button"
            className="ov-btn ov-btn--primary"
            disabled={
              !workerDirty ||
              modelWrite.phase !== "idle" ||
              conflict ||
              modelWrite.preview !== null
            }
            onClick={() => void handleWorkerSave()}
          >
            保存模型
          </button>
        </div>

        <WritePreview
          label="档位"
          write={levelWrite}
          onDiscard={() => levelWrite.clear()}
          onReload={handleReload}
          submitLabel="确认提交档位"
          onSubmit={submitLevel}
        />
        <WritePreview
          label="干活模型"
          write={modelWrite}
          onDiscard={() => modelWrite.clear()}
          onReload={handleReload}
          submitLabel="确认提交模型"
          onSubmit={submitWorker}
        />
        <p className="ov-settings__muted">
          项目覆盖以服务端 `clamped[]` 为准；继承值只读。恢复继承会删除对应覆盖键。
        </p>
      </div>
    </section>
  );
}

interface PreviewProps {
  label: string;
  write: ReturnType<typeof useGuardedWrite>;
  submitLabel: string;
  onDiscard: () => void;
  onReload: () => Promise<void>;
  onSubmit: () => Promise<void>;
}

function WritePreview({
  label,
  write,
  submitLabel,
  onDiscard,
  onReload,
  onSubmit,
}: PreviewProps): ReactNode {
  const preview: GuardedWritePreview | null = write.preview;
  if (!preview && !write.error) return null;
  const blocked = preview && !preview.can_submit.ok;
  return (
    <div className="ov-write-preview">
      <h3 className="ov-settings__h3">{label}：保存预览</h3>
      {write.error ? (
        <div role="alert" className="ov-alert">
          {write.error}
        </div>
      ) : null}
      {preview?.errors.map((issue) => (
        <div role="alert" className="ov-alert" key={`${issue.pointer}-${issue.code}`}>
          {issue.pointer || "（整体）"}：{issueText(issue)}
        </div>
      ))}
      {preview?.warnings.map((issue) => (
        <div role="status" className="ov-notice ov-notice--warn" key={`${issue.pointer}-${issue.code}`}>
          {issueText(issue)}
        </div>
      ))}
      {preview?.relaxes.length ? (
        <div role="status" className="ov-notice ov-notice--warn">
          此改动放宽了安全限制：{preview.relaxes.join("、")}
        </div>
      ) : null}
      {preview?.requires_signature ? (
        <div role="status" className="ov-notice ov-notice--info">
          此改动需要审批设备签名。
        </div>
      ) : null}
      {blocked && preview ? (
        <div role="status" className="ov-notice ov-notice--warn">
          无法提交：
          {preview.can_submit.ok
            ? ""
            : preview.can_submit.reason === "not_connected"
              ? "外壳未连接。示例模式不会提交或保存。"
              : preview.can_submit.reason === "not_approver"
                ? "此改动需要审批设备签名，而本机不是审批设备。"
                : "预览未通过校验。"}
        </div>
      ) : null}
      <div className="ov-save__actions">
        <button
          type="button"
          className="ov-btn ov-btn--primary"
          disabled={!preview?.can_submit.ok || write.phase !== "idle"}
          onClick={() => void onSubmit()}
        >
          {submitLabel}
        </button>
        <button
          type="button"
          className="ov-btn"
          onClick={onDiscard}
          disabled={write.phase !== "idle"}
        >
          取消预览
        </button>
        <button
          type="button"
          className="ov-btn"
          onClick={() => void onReload()}
          disabled={write.phase !== "idle"}
        >
          重新载入
        </button>
      </div>
    </div>
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

function readLevel(doc: SettingsDocument): string {
  const value = asRecord(doc.value);
  return value ? asString(value.default_level) : "";
}
