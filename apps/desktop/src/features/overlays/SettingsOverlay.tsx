import { useCallback, useMemo, useRef, useState, type ReactNode } from "react";
import type { DesktopState } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { Modal } from "@/a11y/Modal";
import { phaseLabel } from "@/i18n";
import { isScenarioAdapter } from "@/ipc";
import { ScenarioMark } from "./ScenarioMark";
import { projectLabel } from "./settingsData";
import { SettingsModelsPage } from "./SettingsModelsPage";
import { SettingsProjectPage } from "./SettingsProjectPage";
import { SettingsDevicesPage } from "./SettingsDevicesPage";
import { SettingsPlaceholder, type PlaceholderKey } from "./SettingsPlaceholder";

export interface SettingsOverlayProps {
  controller: DesktopController;
  state: DesktopState;
  initialPage?: string;
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

const PLACEHOLDER_PAGES: Record<string, PlaceholderKey> = {
  levels: "levels",
  github: "github",
  sandbox: "sandbox",
  policies: "policies",
  guardian: "guardian",
  notify: "notify",
  appearance: "appearance",
};

export function SettingsOverlay({
  controller,
  state,
  initialPage,
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

  const requestClose = useCallback(() => {
    if (dirty) setConfirmTarget({ kind: "close" });
    else onClose();
  }, [dirty, onClose]);

  const requestPage = useCallback(
    (next: string) => {
      if (next === page) return;
      if (dirty) setConfirmTarget({ kind: "navigate", page: next });
      else setPage(next);
    },
    [dirty, page],
  );

  const stay = useCallback(() => setConfirmTarget(null), []);

  const leave = useCallback(() => {
    const target = confirmTarget;
    setConfirmTarget(null);
    setDirty(false);
    if (!target || target.kind === "close") {
      onClose();
      return;
    }
    setPage(target.page);
  }, [confirmTarget, onClose]);

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
            role="alertdialog"
            aria-label="放弃未保存的改动"
            className="ov-settings__leave"
          >
            <span>有未保存的改动，离开会丢失。</span>
            <button
              type="button"
              className="ov-btn ov-btn--warn"
              onClick={leave}
            >
              {confirmTarget.kind === "close" ? "放弃改动并返回" : "放弃改动并切换"}
            </button>
            <button type="button" className="ov-btn" onClick={stay}>
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
                      onClick={() => requestPage(item.page)}
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
            {PLACEHOLDER_PAGES[page] ? (
              <SettingsPlaceholder page={PLACEHOLDER_PAGES[page]} />
            ) : null}
            {!PLACEHOLDER_PAGES[page] &&
            page !== "models" &&
            page !== "devices" &&
            !page.startsWith("project:") ? (
              <SettingsPlaceholder page="unknown" />
            ) : null}
          </main>
        </div>
      </div>
    </Modal>
  );
}
