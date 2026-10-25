import type {
  KeyboardEvent as ReactKeyboardEvent,
  ReactNode,
  RefObject,
} from "react";
import { useEffect, useRef, useState } from "react";
import type { ProjectUiState } from "@/api/shell";
import type { ProjectData } from "@/store/state";
import type { DesktopController } from "@/store/controller";
import { errorMessage } from "@/i18n";
import { UntrustedText } from "@/render";
import {
  LEVELS,
  LEVEL_HINTS,
  contextOptions,
  ctxLabelFor,
  deliveryForCtx,
} from "./model";

export interface ComposerProps {
  projectId: string | null;
  controller: DesktopController;
  data: ProjectData;
  ui: ProjectUiState;
  isViewer: boolean;
  platform: "windows" | "macos" | "ios" | null;
  connectionLive: boolean;
  focusRequest: number;
  composerRef: RefObject<HTMLTextAreaElement | null>;
  onUpdate: (patch: Partial<ProjectUiState>) => void;
}

/**
 * 输入区：多行输入、IME 保护、上下文、档位上限、离线队列。
 * 档位默认值只用于显示；`level_cap` 由控制器在显式 override 时才发送。
 */
export function Composer({
  projectId,
  controller,
  data,
  ui,
  isViewer,
  platform,
  connectionLive,
  focusRequest,
  composerRef,
  onUpdate,
}: ComposerProps): ReactNode {
  const [contextOpen, setContextOpen] = useState(false);
  const pickerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const optionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const levelRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const isMac = platform === "macos";
  const shortcut = isMac ? "⌘↵ 发送" : "Ctrl+Enter 发送";
  const ariaShortcut = isMac ? "Meta+Enter" : "Control+Enter";

  const ctxDelivery = deliveryForCtx(ui.ctx, data);
  const defaultLevel = data.defaultLevel;
  const displayLevel =
    ui.levelOverride ?? ctxDelivery?.level_cap ?? defaultLevel ?? null;
  const options = contextOptions(data);
  const currentIndex = displayLevel ? LEVELS.indexOf(displayLevel) : -1;
  const stopIndex = currentIndex >= 0 ? currentIndex : 0;
  const conflict =
    ui.levelOverride !== null &&
    ctxDelivery !== null &&
    ui.levelOverride !== ctxDelivery.level_cap;
  const draftReady = ui.draft.trim().length > 0;
  const canSend = !isViewer && draftReady;

  useEffect(() => {
    if (focusRequest <= 0) return;
    const element = composerRef.current;
    if (!element) return;
    element.focus();
    const length = element.value.length;
    element.setSelectionRange?.(length, length);
  }, [focusRequest, composerRef]);

  useEffect(() => {
    setContextOpen(false);
  }, [projectId]);

  useEffect(() => {
    if (!contextOpen) return;
    const handlePointer = (event: PointerEvent): void => {
      if (!pickerRef.current?.contains(event.target as Node)) {
        setContextOpen(false);
      }
    };
    document.addEventListener("pointerdown", handlePointer);
    return () => document.removeEventListener("pointerdown", handlePointer);
  }, [contextOpen]);

  useEffect(() => {
    setContextOpen(false);
  }, [projectId]);

  useEffect(() => {
    if (!contextOpen) return;
    const selected = options.findIndex((option) =>
      option.ref === null ? ui.ctx === null : sameRef(option.ref, ui.ctx),
    );
    optionRefs.current[Math.max(selected, 0)]?.focus();
    // 打开时只定位一次，避免输入或选择过程中抢焦点。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [contextOpen]);

  function focusComposer(): void {
    const element = composerRef.current;
    if (!element) return;
    element.focus();
    const length = element.value.length;
    element.setSelectionRange?.(length, length);
  }

  function chooseContext(ref: ProjectUiState["ctx"]): void {
    onUpdate({ ctx: ref, levelOverride: null });
    setContextOpen(false);
    focusComposer();
  }

  function moveLevel(delta: number | "home" | "end"): void {
    if (isViewer) return;
    const next =
      delta === "home"
        ? 0
        : delta === "end"
          ? LEVELS.length - 1
          : Math.max(0, Math.min(LEVELS.length - 1, stopIndex + delta));
    const level = LEVELS[next];
    onUpdate({ levelOverride: level });
    window.setTimeout(() => levelRefs.current[next]?.focus(), 0);
  }

  function handleLevelKey(
    event: ReactKeyboardEvent<HTMLDivElement>,
  ): void {
    if (event.key === "ArrowRight" || event.key === "ArrowDown") {
      event.preventDefault();
      moveLevel(1);
    } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
      event.preventDefault();
      moveLevel(-1);
    } else if (event.key === "Home") {
      event.preventDefault();
      moveLevel("home");
    } else if (event.key === "End") {
      event.preventDefault();
      moveLevel("end");
    }
  }

  function send(): void {
    if (!canSend) return;
    controller.send();
    focusComposer();
  }

  function handleKeyDown(
    event: ReactKeyboardEvent<HTMLTextAreaElement>,
  ): void {
    const composing =
      event.nativeEvent.isComposing || event.keyCode === 229;
    const modifier = isMac ? event.metaKey : event.ctrlKey;
    if (event.key === "Enter" && modifier && !composing) {
      event.preventDefault();
      send();
    }
  }

  return (
    <div className="ws-composer-wrap">
      {ui.outbox.length > 0 ? (
        <section className="ws-outbox" aria-label="待发送消息">
          {ui.outbox.map((item) => (
            <div key={item.local_id} className="ws-outbox__item">
              <div className="ws-outbox__body">
                <UntrustedText value={item.text} />
                <span className="ws-outbox__state">
                  {item.state === "queued"
                    ? "待发送"
                    : item.state === "sending"
                      ? "发送中"
                      : `发送失败：${errorMessage(item.error_code)}`}
                </span>
              </div>
              {item.state === "failed" ? (
                <div className="ws-outbox__actions">
                  <button
                    type="button"
                    className="btn btn-sm"
                    onClick={() => {
                      controller.editOutbox(item.local_id);
                      focusComposer();
                    }}
                  >
                    编辑
                  </button>
                  {!isViewer ? (
                    <button
                      type="button"
                      className="btn btn-sm"
                      onClick={() => controller.retryOutbox(item.local_id)}
                    >
                      重试
                    </button>
                  ) : null}
                  <button
                    type="button"
                    className="btn btn-sm"
                    onClick={() => controller.removeOutbox(item.local_id)}
                  >
                    删除
                  </button>
                </div>
              ) : null}
            </div>
          ))}
        </section>
      ) : null}

      <div className={`ws-composer${conflict ? " has-warning" : ""}`}>
        {conflict && ctxDelivery ? (
          <div className="ws-composer__warning" role="status">
            {ctxLabelFor(ui.ctx, data)} 上限{" "}
            {ctxDelivery.level_cap} → {ui.levelOverride}；已触发的闸门不撤销
          </div>
        ) : null}

        <label className="visually-hidden" htmlFor="composer">
          给 agent 发消息
        </label>
        <textarea
          id="composer"
          ref={composerRef}
          className="ws-composer__input"
          rows={2}
          value={ui.draft}
          disabled={isViewer}
          placeholder={
            isViewer
              ? "此设备为只读，不能发消息"
              : "描述任务，或针对某个 Issue / PR 提问…（用 @ 选择上下文）"
          }
          onChange={(event) => onUpdate({ draft: event.target.value })}
          onKeyDown={handleKeyDown}
        />

        <div className="ws-composer__bar">
          <div
            className="ws-context"
            ref={pickerRef}
            onKeyDown={(event) => {
              if (event.key !== "Escape" || !contextOpen) return;
              event.preventDefault();
              setContextOpen(false);
              triggerRef.current?.focus();
            }}
          >
            <button
              ref={triggerRef}
              type="button"
              className="ws-context__trigger"
              aria-haspopup="listbox"
              aria-expanded={contextOpen}
              title="选择这条消息的上下文"
              disabled={isViewer}
              onClick={() => setContextOpen((value) => !value)}
            >
              @ {ctxLabelFor(ui.ctx, data)} ▾
            </button>
            {ui.ctx ? (
              <button
                type="button"
                className="ws-context__clear"
                aria-label="清除上下文"
                disabled={isViewer}
                onClick={() => chooseContext(null)}
              >
                ×
              </button>
            ) : null}
            {contextOpen ? (
              <div
                role="listbox"
                aria-label="选择上下文"
                className="ws-context__menu"
                onKeyDown={(event) => {
                  const activeIndex = optionRefs.current.findIndex(
                    (element) => element === document.activeElement,
                  );
                  if (event.key === "ArrowDown") {
                    event.preventDefault();
                    const next = Math.min(
                      Math.max(activeIndex + 1, 0),
                      options.length - 1,
                    );
                    optionRefs.current[next]?.focus();
                  } else if (event.key === "ArrowUp") {
                    event.preventDefault();
                    const next = Math.max(activeIndex - 1, 0);
                    optionRefs.current[next]?.focus();
                  } else if (event.key === "Home") {
                    event.preventDefault();
                    optionRefs.current[0]?.focus();
                  } else if (event.key === "End") {
                    event.preventDefault();
                    optionRefs.current[options.length - 1]?.focus();
                  }
                }}
              >
                {options.map((option, index) => (
                  <button
                    key={option.key}
                    ref={(element) => {
                      optionRefs.current[index] = element;
                    }}
                    type="button"
                    role="option"
                    tabIndex={-1}
                    aria-selected={
                      option.ref === null
                        ? ui.ctx === null
                        : ui.ctx !== null && sameRef(option.ref, ui.ctx)
                    }
                    className="ws-context__option"
                    onClick={() => chooseContext(option.ref)}
                  >
                    {option.label}
                  </button>
                ))}
              </div>
            ) : null}
          </div>

          <span className="ws-composer__label">上限</span>
          <div
            role="radiogroup"
            aria-label="档位上限"
            className="ws-levels"
            onKeyDown={handleLevelKey}
          >
            {LEVELS.map((level, index) => {
              const checked = level === displayLevel;
              return (
                <button
                  key={level}
                  ref={(element) => {
                    levelRefs.current[index] = element;
                  }}
                  type="button"
                  role="radio"
                  aria-checked={checked}
                  tabIndex={index === stopIndex ? 0 : -1}
                  title={LEVEL_HINTS[level]}
                  className={`ws-levels__item${checked ? " is-checked" : ""}`}
                  disabled={isViewer}
                  onClick={() => onUpdate({ levelOverride: level })}
                >
                  {level}
                </button>
              );
            })}
          </div>
          <span className="ws-composer__hint">
            {displayLevel === "None"
              ? "None：只问答，不建交付"
              : displayLevel
                ? `≤ ${displayLevel}`
                : "默认上限未载入"}
          </span>

          <span className="ws-composer__shortcut">{shortcut}</span>
          <button
            type="button"
            className="btn btn-primary ws-composer__send"
            disabled={!canSend}
            aria-keyshortcuts={ariaShortcut}
            onClick={send}
          >
            {isViewer ? "发送" : connectionLive ? "发送" : "稍后发送"}
          </button>
        </div>
      </div>
    </div>
  );
}

function sameRef(
  a: ProjectUiState["ctx"],
  b: ProjectUiState["ctx"],
): boolean {
  if (!a || !b) return a === b;
  switch (a.kind) {
    case "issue":
      return b.kind === "issue" && a.repo === b.repo && a.number === b.number;
    case "pr":
      return b.kind === "pr" && a.repo === b.repo && a.number === b.number;
    case "worktree":
      return b.kind === "worktree" && a.id === b.id;
    case "delivery":
      return b.kind === "delivery" && a.id === b.id;
  }
}
