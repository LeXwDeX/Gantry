/**
 * 本机外观偏好页。
 *
 * 字段来自 docs/interfaces/config.md §13.2 与 api/shell.ts 的 Prefs；
 * 只支持已设计的深色主题与中文。偏好存本机，不进事件日志、不跨设备同步。
 * 开发场景只写入内存；保存后立即 prefsGet 回读，不触发系统通知。
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
import type { Prefs } from "@/api/shell";
import { isScenarioAdapter } from "@/ipc";
import { ipcErrorText } from "./settingsData";
import "./settings-resources.css";
import {
  FieldGrid,
  type FieldSpec,
  setAtPath,
  type Path,
} from "./settingsFields";

export interface SettingsAppearancePageProps {
  controller: DesktopController;
  state: DesktopState;
  onDirtyChange: (dirty: boolean) => void;
  /** 保存后立即应用到工作区布局；只传本机偏好。 */
  onPrefsChanged?: (prefs: Prefs) => void;
}

const APPEARANCE_FIELDS: FieldSpec[] = [
  {
    path: ["theme"],
    label: "主题",
    kind: "enum",
    options: ["dark"],
    required: true,
    help: "当前只实现深色主题；浅色与跟随系统在 config.md §13.2 有键名但未设计，不提供伪选项。",
  },
  {
    path: ["language"],
    label: "语言",
    kind: "enum",
    options: ["zh-CN"],
    required: true,
    help: "当前只提供中文（zh-CN）；其他语言未设计。",
  },
  {
    path: ["density"],
    label: "密度",
    kind: "enum",
    options: ["compact", "comfortable"],
    required: true,
    help: "compact 是原型的紧凑信息密度；comfortable 是已有字段值，原型未展开其具体观感。",
  },
  {
    path: ["systemNotifications"],
    label: "本机系统通知",
    kind: "boolean",
    help: "只决定本机是否弹窗；服务端 notify.channels.desktop 决定是否发出。模拟保存不会触发系统通知。",
  },
  {
    path: ["layout", "leftWidth"],
    label: "左栏宽度",
    kind: "integer",
    min: 180,
    max: 480,
    unit: "px",
    required: true,
  },
  {
    path: ["layout", "inspectorWidth"],
    label: "检视器宽度",
    kind: "integer",
    min: 220,
    max: 520,
    unit: "px",
    required: true,
  },
  {
    path: ["layout", "leftCollapsed"],
    label: "折叠左栏",
    kind: "boolean",
  },
  {
    path: ["layout", "inspectorCollapsed"],
    label: "折叠检视器",
    kind: "boolean",
  },
];

interface LoadState {
  status: "loading" | "ready" | "error";
  error: string | null;
}

interface PrefsDraft {
  value: Prefs;
  baseline: string;
}

function fingerprint(value: unknown): string {
  return JSON.stringify(value);
}

function prefsFieldsIssues(prefs: Prefs): import("@/api/types").SettingsIssue[] {
  const issues: import("@/api/types").SettingsIssue[] = [];
  if (prefs.theme !== "dark") {
    issues.push({
      pointer: "/theme",
      code: "enum",
      message: "当前只支持深色主题。",
    });
  }
  if (prefs.language !== "zh-CN") {
    issues.push({
      pointer: "/language",
      code: "enum",
      message: "当前只支持中文（zh-CN）。",
    });
  }
  if (!["compact", "comfortable"].includes(prefs.density)) {
    issues.push({
      pointer: "/density",
      code: "enum",
      message: "密度必须是 compact 或 comfortable。",
    });
  }
  for (const [key, label, min, max] of [
    ["leftWidth", "左栏宽度", 180, 480],
    ["inspectorWidth", "检视器宽度", 220, 520],
  ] as const) {
    const value = prefs.layout[key];
    if (!Number.isInteger(value) || value < min || value > max) {
      issues.push({
        pointer: `/layout/${key}`,
        code: "range",
        message: `${label}必须在 ${min}–${max} px。`,
      });
    }
  }
  return issues;
}

function diffPrefs(before: Prefs, after: Prefs): Partial<Prefs> {
  const patch: Record<string, unknown> = {};
  for (const key of ["theme", "language", "density", "systemNotifications"] as const) {
    if (before[key] !== after[key]) patch[key] = after[key];
  }
  const layout: Record<string, unknown> = {};
  for (const key of [
    "leftWidth",
    "inspectorWidth",
    "leftCollapsed",
    "inspectorCollapsed",
  ] as const) {
    if (before.layout[key] !== after.layout[key]) {
      layout[key] = after.layout[key];
    }
  }
  if (Object.keys(layout).length > 0) patch.layout = layout;
  return patch as Partial<Prefs>;
}

function describePrefsDiff(before: Prefs, after: Prefs): string[] {
  const lines: string[] = [];
  if (before.theme !== after.theme) lines.push(`主题 ${before.theme} → ${after.theme}`);
  if (before.language !== after.language) {
    lines.push(`语言 ${before.language} → ${after.language}`);
  }
  if (before.density !== after.density) {
    lines.push(`密度 ${before.density} → ${after.density}`);
  }
  if (before.systemNotifications !== after.systemNotifications) {
    lines.push(
      `本机系统通知 ${before.systemNotifications ? "开" : "关"} → ${after.systemNotifications ? "开" : "关"}`,
    );
  }
  for (const [key, label] of [
    ["leftWidth", "左栏宽度"],
    ["inspectorWidth", "检视器宽度"],
  ] as const) {
    if (before.layout[key] !== after.layout[key]) {
      lines.push(
        `${label} ${before.layout[key]} → ${after.layout[key]} px`,
      );
    }
  }
  for (const [key, label] of [
    ["leftCollapsed", "左栏折叠"],
    ["inspectorCollapsed", "检视器折叠"],
  ] as const) {
    if (before.layout[key] !== after.layout[key]) {
      lines.push(
        `${label} ${before.layout[key] ? "是" : "否"} → ${after.layout[key] ? "是" : "否"}`,
      );
    }
  }
  return lines;
}

export function SettingsAppearancePage({
  controller,
  state,
  onDirtyChange,
  onPrefsChanged,
}: SettingsAppearancePageProps): ReactNode {
  const [load, setLoad] = useState<LoadState>({
    status: "loading",
    error: null,
  });
  const [draft, setDraft] = useState<PrefsDraft | null>(null);
  const [preview, setPreview] = useState<Prefs | null>(null);
  const [previewBaseline, setPreviewBaseline] = useState<Prefs | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [confirmReload, setConfirmReload] = useState(false);
  const [saving, setSaving] = useState(false);
  const scenario = isScenarioAdapter(controller.ipc);
  const alive = useRef(true);

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const reload = useCallback(async (): Promise<void> => {
    setLoad({ status: "loading", error: null });
    setLocalError(null);
    setSaved(null);
    setConflict(false);
    setPreview(null);
    setPreviewBaseline(null);
    setConfirmReload(false);
    try {
      const prefs = await controller.ipc.prefsGet();
      if (!alive.current) return;
      setDraft({ value: prefs, baseline: fingerprint(prefs) });
      setLoad({ status: "ready", error: null });
    } catch (failure) {
      if (!alive.current) return;
      setLoad({ status: "error", error: ipcErrorText(failure) });
    }
  }, [controller]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const dirty =
    draft !== null &&
    (fingerprint(draft.value) !== draft.baseline || preview !== null);

  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange(false), [onDirtyChange]);

  const edit = useCallback(
    (path: Path, next: unknown): void => {
      if (saving) return;
      setDraft((prev) =>
        prev
          ? {
              ...prev,
              value: setAtPath(prev.value, path, next) as Prefs,
            }
          : prev,
      );
      setPreview(null);
      setPreviewBaseline(null);
      setSaved(null);
      setLocalError(null);
      setConflict(false);
    },
    [saving],
  );

  const handlePreview = useCallback((): void => {
    if (!draft) return;
    const issues = prefsFieldsIssues(draft.value);
    if (issues.length > 0) {
      setLocalError(
        `请先修正标出的 ${issues.length} 项字段错误：${issues
          .map((issue) => issue.message ?? issue.code)
          .join("；")}`,
      );
      setPreview(null);
      return;
    }
    const edited = draft.value;
    setPreview(structuredClone(edited));
    setPreviewBaseline(JSON.parse(draft.baseline) as Prefs);
    setLocalError(null);
    setSaved(null);
  }, [draft]);

  const handleSave = useCallback(async (): Promise<void> => {
    if (!preview || !draft) return;
    setSaving(true);
    setLocalError(null);
    setSaved(null);
    try {
      const latest = await controller.ipc.prefsGet();
      if (!alive.current) return;
      if (fingerprint(latest) !== draft.baseline) {
        setConflict(true);
        setPreview(null);
        setPreviewBaseline(null);
        setLocalError("本机偏好已在别处修改；请重新载入最新值，不能自动合并。");
        return;
      }
      const patch = diffPrefs(previewBaseline ?? latest, preview);
      await controller.ipc.prefsSet({ patch });
      const readBack = await controller.ipc.prefsGet();
      if (!alive.current) return;
      const equal = fingerprint(readBack) === fingerprint(preview);
      setDraft({ value: readBack, baseline: fingerprint(readBack) });
      setPreview(null);
      setPreviewBaseline(null);
      if (equal) onPrefsChanged?.(readBack);
      setSaved(
        equal
          ? scenario
            ? "已保存到本机偏好并回读一致，布局已应用到工作区。开发场景只写入内存；未触发系统通知。"
            : "已保存到本机偏好并回读一致，布局已应用到工作区。未触发系统通知。"
          : scenario
            ? "已保存到本机偏好，但回读值与提交值不同；请重新载入检查。开发场景只写入内存。"
            : "已保存到本机偏好，但回读值与提交值不同；请重新载入检查。",
      );
    } catch (failure) {
      if (!alive.current) return;
      setLocalError(`${ipcErrorText(failure)} 可修正后重试；预览仍保留。`);
    } finally {
      if (alive.current) setSaving(false);
    }
  }, [controller, draft, onPrefsChanged, preview, previewBaseline, scenario]);

  const requestReload = useCallback((): void => {
    if (dirty) setConfirmReload(true);
    else void reload();
  }, [dirty, reload]);

  if (load.status === "loading") {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">外观</h1>
        <p role="status" className="ov-settings__muted">
          正在载入本机偏好…
        </p>
      </section>
    );
  }

  if (load.status === "error" || !draft) {
    return (
      <section className="ov-settings__page">
        <h1 className="ov-settings__h1">外观</h1>
        <div role="alert" className="ov-alert">
          {load.error ?? "本机偏好不可用。"}
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

  const issues = prefsFieldsIssues(draft.value);
  const previewLines =
    preview && previewBaseline ? describePrefsDiff(previewBaseline, preview) : [];

  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">外观</h1>
        <p className="ov-settings__muted">
          本机偏好，只保存在本机应用配置，不进事件日志、不跨设备同步、丢失无业务后果。
          不存放凭据或配置键。
        </p>
        <p className="ov-settings__meta">
          来源：docs/interfaces/config.md §13.2、api/shell.ts 的 Prefs。字段值只开放已设计选项。
        </p>
      </div>

      {scenario ? (
        <div className="ov-notice ov-notice--info" role="status">
          开发场景：偏好只写入内存，保存后立即回读并应用到工作区布局；不触发系统通知。
        </div>
      ) : null}

      {conflict ? (
        <div className="ov-notice ov-notice--warn" role="alert">
          <span>本机偏好已被其他进程修改。不会自动合并。</span>
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => void reload()}
          >
            载入最新值（丢弃我的改动）
          </button>
        </div>
      ) : null}

      {confirmReload ? (
        <div className="ov-notice ov-notice--warn" role="alert">
          <span>有未保存的改动，重新载入会丢失。</span>
          <button
            type="button"
            className="ov-btn ov-btn--warn ov-btn--sm"
            onClick={() => {
              setConfirmReload(false);
              void reload();
            }}
          >
            放弃改动并重新载入
          </button>
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            onClick={() => setConfirmReload(false)}
          >
            继续编辑
          </button>
        </div>
      ) : null}

      {localError ? (
        <div role="alert" className="ov-alert">
          {localError}
        </div>
      ) : null}

      <section className="ov-settings__section">
        <h2 className="ov-settings__h2">主题、语言与密度</h2>
        <p className="ov-settings__muted">
          浅色主题、跟随系统与其他语言没有设计，不提供伪开关；当前字段值分别固定为 dark 与 zh-CN。
        </p>
        <FieldGrid
          fields={APPEARANCE_FIELDS.slice(0, 4)}
          value={draft.value}
          issues={issues}
          disabled={saving}
          onChange={edit}
        />
      </section>

      <section className="ov-settings__section">
        <h2 className="ov-settings__h2">布局记忆</h2>
        <p className="ov-settings__muted">
          这些字段记录面板宽度与折叠状态。保存只写本机偏好；工作区是否立即应用由工作区读取偏好的时机决定，本页不直接改布局。
        </p>
        <FieldGrid
          fields={APPEARANCE_FIELDS.slice(4)}
          value={draft.value}
          issues={issues}
          disabled={saving}
          onChange={edit}
        />
      </section>

      <section className="ov-save" aria-label="本机偏好保存">
        <h2 className="ov-settings__h2">保存</h2>
        <p className="ov-settings__muted">
          保存前先本地校验并生成预览；确认后调用 prefsSet，再立即 prefsGet 回读。
          本机偏好没有服务端版本或 ETag。
        </p>

        {issues.length > 0 ? (
          <div className="ov-notice ov-notice--warn" role="alert">
            {issues.map((issue) => (
              <span
                className="ov-issues__item"
                key={`${issue.pointer}-${issue.code}`}
              >
                {issue.pointer}：{issue.message ?? issue.code}
              </span>
            ))}
          </div>
        ) : null}

        {preview ? (
          <div className="sr-preview">
            <h3 className="ov-settings__h3">保存预览</h3>
            {previewLines.length === 0 ? (
              <p className="ov-settings__muted">没有变更。</p>
            ) : (
              <ul className="sr-preview__list">
                {previewLines.map((line) => (
                  <li key={line}>{line}</li>
                ))}
              </ul>
            )}
            <div className="ov-save__actions">
              <button
                type="button"
                className="ov-btn ov-btn--primary"
                disabled={saving}
                onClick={() => void handleSave()}
              >
                {scenario ? "模拟应用到本机偏好" : "保存到本机"}
              </button>
              <button
                type="button"
                className="ov-btn"
                disabled={saving}
                onClick={() => {
                  setPreview(null);
                  setPreviewBaseline(null);
                }}
              >
                取消预览
              </button>
              <button
                type="button"
                className="ov-btn"
                disabled={saving}
                onClick={requestReload}
              >
                重新载入
              </button>
            </div>
            <span className="ov-settings__meta">
              模拟保存不会触发系统通知；布局字段需工作区重新读取偏好后生效。
            </span>
          </div>
        ) : (
          <div className="ov-save__actions">
            <button
              type="button"
              className="ov-btn ov-btn--primary"
              disabled={!dirty || saving || issues.length > 0}
              onClick={handlePreview}
            >
              预览本机偏好保存
            </button>
            {scenario ? (
              <span className="ov-settings__meta">
                开发场景只写入内存；不会写入真实应用配置目录。
              </span>
            ) : null}
          </div>
        )}

        {saved ? (
          <div role="status" className="ov-notice ov-notice--ok">
            {saved}
          </div>
        ) : null}

        <span className="ov-settings__meta">
          当前基线：{draft.baseline === fingerprint(draft.value) ? "已同步" : "有未保存改动"}
          {" · "}本机偏好不产生服务端 rev。
        </span>
      </section>
    </section>
  );
}
