import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import type { Prefs } from "@/api/shell";
import { Modal } from "@/a11y/Modal";
import { phaseLabel } from "@/i18n";
import { isScenarioAdapter } from "@/ipc";
import { ScenarioMark } from "./ScenarioMark";
import { projectLabel } from "./settingsData";
import { SettingsModelsPage } from "./SettingsModelsPage";
import { SettingsProjectPage } from "./SettingsProjectPage";
import { SettingsDevicesPage } from "./SettingsDevicesPage";
import { SettingsResourcePage } from "./SettingsResourcePage";
import { SettingsAppearancePage } from "./SettingsAppearancePage";
import { SettingsPlaceholder } from "./SettingsPlaceholder";

export interface SettingsOverlayProps {
  controller: DesktopController;
  state: DesktopState;
  initialPage?: string;
  /** 本机偏好保存后回传给工作区，立即应用布局。 */
  onPrefsChanged?: (prefs: Prefs) => void;
  onClose: () => void;
}

interface NavItem {
  page: string;
  label: string;
}

interface NavGroup {
  title: string;
  items: NavItem[];
}

const RESOURCE_PAGES = [
  "levels",
  "github",
  "sandbox",
  "policies",
  "guardian",
  "notify",
] as const;

type ResourcePage = (typeof RESOURCE_PAGES)[number];

function isResourcePage(page: string): page is ResourcePage {
  return (RESOURCE_PAGES as readonly string[]).includes(page);
}

export function SettingsOverlay({
  controller,
  state,
  initialPage,
  onPrefsChanged,
  onClose,
}: SettingsOverlayProps): ReactNode {
  const [page, setPage] = useState(initialPage || "models");
  const [dirty, setDirty] = useState(false);
  const [confirmTarget, setConfirmTarget] = useState<
    | { kind: "close" }
    | { kind: "navigate"; page: string }
    | null
  >(null);
  const backRef = useRef<HTMLButtonElement>(null);
  const confirmRef = useRef<HTMLDivElement>(null);
  const stayRef = useRef<HTMLButtonElement>(null);
  const confirmTrigger = useRef<HTMLElement | null>(null);
  const restoreFocus = useRef(true);

  const projectPages = useMemo(
    () =>
      state.projects.map((project) => ({
        page: `project:${project.id}`,
        label: projectLabel(project.repo, project.id),
      })),
    [state.projects],
  );

  const navGroups: NavGroup[] = useMemo(
    () => [
      {
        title: "服务端",
        items: [
          { page: "models", label: "模型" },
          { page: "levels", label: "档位与预算" },
          { page: "github", label: "GitHub App" },
          { page: "sandbox", label: "沙盒与出网" },
          { page: "policies", label: "危险动作策略" },
          { page: "guardian", label: "后台守护" },
          { page: "notify", label: "通知" },
        ],
      },
      { title: "项目", items: projectPages },
      {
        title: "本机",
        items: [
          { page: "devices", label: "设备与证书" },
          { page: "appearance", label: "外观" },
        ],
      },
    ],
    [projectPages],
  );

  const onDirtyChange = useCallback((value: boolean) => {
    setDirty(value);
  }, []);

  const openConfirm = useCallback(
    (
      target: { kind: "close" } | { kind: "navigate"; page: string },
      trigger: HTMLElement | null,
    ) => {
      confirmTrigger.current = trigger;
      restoreFocus.current = true;
      setConfirmTarget(target);
    },
    [],
  );

  const requestClose = useCallback(() => {
    if (dirty) openConfirm({ kind: "close" }, backRef.current);
    else onClose();
  }, [dirty, onClose, openConfirm]);

  const requestPage = useCallback(
    (next: string, trigger: HTMLElement | null) => {
      if (next === page) return;
      if (dirty) openConfirm({ kind: "navigate", page: next }, trigger);
      else setPage(next);
    },
    [dirty, page, openConfirm],
  );

  const stay = useCallback(() => {
    restoreFocus.current = true;
    setConfirmTarget(null);
  }, []);

  const leave = useCallback(() => {
    const target = confirmTarget;
    restoreFocus.current = false;
    confirmTrigger.current = null;
    setConfirmTarget(null);
    setDirty(false);
    if (!target || target.kind === "close") {
      onClose();
      return;
    }
    setPage(target.page);
  }, [confirmTarget, onClose]);

  useEffect(() => {
    if (confirmTarget) {
      stayRef.current?.focus();
      return;
    }
    if (!restoreFocus.current) {
      restoreFocus.current = true;
      return;
    }
    const trigger = confirmTrigger.current;
    confirmTrigger.current = null;
    if (trigger?.isConnected) trigger.focus();
  }, [confirmTarget]);

  useEffect(() => {
    if (!confirmTarget) return;
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        stay();
        return;
      }
      if (event.key !== "Tab" || !confirmRef.current) return;
      const focusable = Array.from(
        confirmRef.current.querySelectorAll<HTMLElement>(
          "button:not([disabled])",
        ),
      );
      if (focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;
      if (!confirmRef.current.contains(active)) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      } else if (event.shiftKey && active === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && active === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown, true);
    return () => document.removeEventListener("keydown", onKeyDown, true);
  }, [confirmTarget, stay]);

  const connection = state.connection;
  const scenario = isScenarioAdapter(controller.ipc);
  const host = state.identity?.server_host ?? "服务端";
  const connectionLabel = scenario
    ? "开发场景 · 未连接真实服务端"
    : connection?.phase === "live"
      ? `${host} · 已连接`
      : phaseLabel(connection?.phase);
  const connectionOk = !scenario && connection?.phase === "live";

  return (
    <Modal
      open
      variant="fullscreen"
      label="设置"
      onClose={requestClose}
      className="ov-settings-dialog"
      initialFocusRef={backRef}
    >
      <div className="ov-settings">
        <header className="ov-settings__header">
          <button
            type="button"
            className="ov-settings__back"
            onClick={requestClose}
            ref={backRef}
          >
            <span aria-hidden="true">‹</span> 返回工作区
          </button>
          <span className="ov-settings__title">设置</span>
          <ScenarioMark ipc={controller.ipc} />
          {dirty ? (
            <span className="ov-settings__dirty" role="status">
              有未保存的改动
            </span>
          ) : null}
          <span className="ov-settings__connection">
            <span
              className={`tone-dot ${connectionOk ? "tone-ok" : "tone-idle"}`}
              aria-hidden="true"
            />
            {connectionLabel}
          </span>
        </header>

        {confirmTarget ? (
          <div
            ref={confirmRef}
            role="alertdialog"
            aria-label="放弃未保存的改动"
            aria-describedby="ov-settings-leave-desc"
            className="ov-settings__leave"
          >
            <span id="ov-settings-leave-desc">有未保存的改动，离开会丢失。</span>
            <button
              type="button"
              className="ov-btn ov-btn--warn"
              onClick={leave}
            >
              {confirmTarget.kind === "close" ? "放弃改动并返回" : "放弃改动并切换"}
            </button>
            <button type="button" className="ov-btn" ref={stayRef} onClick={stay}>
              继续编辑
            </button>
          </div>
        ) : null}

        <div className="ov-settings__body">
          <nav aria-label="设置分类" className="ov-settings__nav">
            {navGroups.map((group) => (
              <div className="ov-settings__nav-group" key={group.title}>
                <span className="ov-settings__nav-title">{group.title}</span>
                {group.items.length === 0 ? (
                  <span className="ov-settings__nav-empty">暂无项目</span>
                ) : (
                  group.items.map((item) => (
                    <button
                      type="button"
                      key={item.page}
                      className="ov-settings__nav-item"
                      aria-current={page === item.page ? "page" : undefined}
                      onClick={(event) =>
                        requestPage(item.page, event.currentTarget)
                      }
                    >
                      {item.label}
                    </button>
                  ))
                )}
              </div>
            ))}
          </nav>

          <main className="ov-settings__main">
            {page === "models" ? (
              <SettingsModelsPage
                controller={controller}
                state={state}
                onDirtyChange={onDirtyChange}
              />
            ) : null}
            {page.startsWith("project:") ? (
              <SettingsProjectPage
                key={page}
                controller={controller}
                state={state}
                projectId={page.slice("project:".length)}
                onDirtyChange={onDirtyChange}
              />
            ) : null}
            {page === "devices" ? (
              <SettingsDevicesPage
                controller={controller}
                state={state}
                onDirtyChange={onDirtyChange}
              />
            ) : null}
            {page === "appearance" ? (
              <SettingsAppearancePage
                controller={controller}
                state={state}
                onDirtyChange={onDirtyChange}
                onPrefsChanged={onPrefsChanged}
              />
            ) : null}
            {isResourcePage(page) ? (
              <SettingsResourcePage
                key={page}
                controller={controller}
                state={state}
                page={page}
                onDirtyChange={onDirtyChange}
              />
            ) : null}
            {page !== "models" &&
            page !== "devices" &&
            page !== "appearance" &&
            !isResourcePage(page) &&
            !page.startsWith("project:") ? (
              <SettingsPlaceholder page="unknown" />
            ) : null}
          </main>
        </div>
      </div>
    </Modal>
  );
}
