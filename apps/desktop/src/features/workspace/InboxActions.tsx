import type { ReactNode } from "react";
import { useState } from "react";
import type { InboxItem } from "@/api/types";
import type { Selection } from "@/api/shell";
import type { ProjectData } from "@/store/state";
import {
  approvalIdOfItem,
  inboxOptions,
  inboxSource,
  selectionForRef,
} from "./model";

export interface InboxActionsProps {
  item: InboxItem;
  data: ProjectData;
  isViewer: boolean;
  onChoose: (itemId: string, optionId: string) => void;
  onDismiss: (itemId: string) => void;
  onReply: (item: InboxItem) => void;
  onSelect: (selection: Selection) => void;
  onOpenApproval: (approvalId: string) => void;
}

/**
 * 上报项的操作区。收件箱、检视器与对话卡片共用；
 * 只发 inbox.choose / inbox.dismiss 或转交审批单，不自行签名。
 */
export function InboxActions({
  item,
  data,
  isViewer,
  onChoose,
  onDismiss,
  onReply,
  onSelect,
  onOpenApproval,
}: InboxActionsProps): ReactNode {
  const [confirming, setConfirming] = useState(false);
  const disabled = isViewer;
  const disabledTitle = isViewer ? "此设备为只读" : undefined;
  const options = inboxOptions(item);

  if (item.kind === "approval_needed") {
    const approvalId = approvalIdOfItem(item);
    if (!approvalId) return null;
    return (
      <button
        type="button"
        className="btn btn-primary btn-sm"
        disabled={disabled}
        title={disabledTitle}
        onClick={() => onOpenApproval(approvalId)}
      >
        审阅并签名…
      </button>
    );
  }

  if (item.kind === "spec_needed") {
    return (
      <button
        type="button"
        className="btn btn-primary btn-sm"
        disabled={disabled}
        title={disabledTitle}
        onClick={() => onReply(item)}
      >
        回复补充…
      </button>
    );
  }

  if (item.kind === "security_finding") {
    const source = inboxSource(item);
    const selection = source ? selectionForRef(source, data) : null;
    return (
      <>
        {selection ? (
          <button
            type="button"
            className="btn btn-sm"
            onClick={() => onSelect(selection)}
          >
            查看
          </button>
        ) : null}
        {confirming ? (
          <span
            className="ws-inline-confirm"
            role="alertdialog"
            aria-label="确认忽略安全发现"
          >
            <span>忽略不会关闭发现本身；审计仍保留。</span>
            <button
              type="button"
              className="btn btn-sm"
              disabled={disabled}
              onClick={() => {
                setConfirming(false);
                onDismiss(item.id);
              }}
            >
              确认忽略
            </button>
            <button
              type="button"
              className="btn btn-sm"
              autoFocus
              onClick={() => setConfirming(false)}
            >
              取消
            </button>
          </span>
        ) : (
          <button
            type="button"
            className="btn btn-sm"
            disabled={disabled}
            title={
              disabled
                ? disabledTitle
                : "忽略不会关闭安全发现本身，审计仍保留"
            }
            onClick={() => setConfirming(true)}
          >
            忽略…
          </button>
        )}
      </>
    );
  }

  if (options.length > 0) {
    return (
      <>
        {options.map((option) => (
          <button
            key={option.id}
            type="button"
            className={`btn btn-sm${option.primary ? " btn-primary" : ""}`}
            disabled={disabled}
            title={disabledTitle}
            onClick={() => onChoose(item.id, option.id)}
          >
            {option.label}
          </button>
        ))}
      </>
    );
  }

  return (
    <>
      <button
        type="button"
        className="btn btn-sm"
        disabled={disabled}
        title={disabled ? disabledTitle : undefined}
        onClick={() => onDismiss(item.id)}
      >
        忽略
      </button>
      {item.kind === "escalation" ? (
        <span className="ws-muted ws-inline-note">忽略不会继续任务。</span>
      ) : null}
    </>
  );
}
