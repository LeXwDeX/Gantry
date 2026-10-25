import { useState, type ReactNode } from "react";
import type { InboxItem, Ref, UntrustedText as UntrustedValue } from "@/api/types";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { Modal } from "@/a11y/Modal";
import { approvalActionLabel, inboxKindLabel } from "@/i18n";
import { UntrustedText } from "@/render";
import { ScenarioMark } from "./ScenarioMark";
import { asRecord, asString, ipcErrorText, projectLabel } from "./settingsData";
import { approvalTargetLabel, shortSha } from "@/store/sign";

export interface InboxOverlayProps {
  controller: DesktopController;
  state: DesktopState;
  onApproval: (approvalId: string) => void;
  onClose: () => void;
}

interface Option {
  id: string;
  label: string;
  primary: boolean;
}

interface Group {
  key: string;
  label: string;
  projectId: string | null;
  items: InboxItem[];
}

export function InboxOverlay({
  controller,
  state,
  onApproval,
  onClose,
}: InboxOverlayProps): ReactNode {
  const [busyId, setBusyId] = useState<string | null>(null);
  const [confirmDismissId, setConfirmDismissId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const openItems = state.inbox.filter((item) => item.status === "open");
  const groups: Group[] = state.projects
    .map((project) => {
      const items = openItems.filter((item) => item.project_id === project.id);
      return {
        key: project.id,
        label: projectLabel(project.repo, project.id),
        projectId: project.id,
        items,
      };
    })
    .filter((group) => group.items.length > 0);
  const globalItems = openItems.filter((item) => item.project_id === null);
  if (globalItems.length > 0) {
    groups.push({
      key: "global",
      label: "全局",
      projectId: null,
      items: globalItems,
    });
  }

  const canChoose = controller.can("choose");
  const canDismiss = controller.can("dismiss");
  const canReply = controller.can("send");
  const readonlyReason = "此设备为只读";

  const selectItem = (item: InboxItem): void => {
    if (item.project_id) controller.selectProject(item.project_id);
    const delivery = item.delivery_id
      ? controller.deliveryById(item.delivery_id)
      : null;
    if (delivery && item.project_id) {
      const sel = delivery.pr
        ? { kind: "pr" as const, repo: delivery.pr.repo, number: delivery.pr.number }
        : delivery.issues[0]
          ? {
              kind: "issue" as const,
              repo: delivery.issues[0].repo,
              number: delivery.issues[0].number,
            }
          : delivery.worktree_id
            ? { kind: "worktree" as const, id: delivery.worktree_id }
            : null;
      controller.updateUi({ sel }, item.project_id);
    }
    onClose();
  };

  const choose = async (item: InboxItem, option: Option): Promise<void> => {
    if (!canChoose || busyId) return;
    setBusyId(item.id);
    setError(null);
    try {
      await controller.choose(item.id, option.id);
    } catch (failure) {
      setError(ipcErrorText(failure));
    } finally {
      setBusyId(null);
    }
  };

  const dismiss = async (item: InboxItem): Promise<void> => {
    if (!canDismiss || busyId) return;
    if (item.kind === "security_finding" && confirmDismissId !== item.id) {
      setConfirmDismissId(item.id);
      return;
    }
    setBusyId(item.id);
    setError(null);
    try {
      await controller.dismiss(item.id);
      setConfirmDismissId(null);
    } catch (failure) {
      setError(ipcErrorText(failure));
    } finally {
      setBusyId(null);
    }
  };

  const replyToSpec = (item: InboxItem): void => {
    if (!canReply) return;
    if (item.project_id) controller.selectProject(item.project_id);
    if (item.delivery_id && item.project_id) {
      controller.updateUi(
        {
          view: "chat",
          ctx: { kind: "delivery", id: item.delivery_id },
        },
        item.project_id,
      );
    }
    onClose();
  };

  const selectSource = (source: Ref): void => {
    let projectId: string | null = null;
    if (source.kind === "issue" || source.kind === "pr") {
      projectId =
        state.projects.find((project) => project.repo === source.repo)?.id ?? null;
    } else if (source.kind === "delivery") {
      projectId = controller.projectForDelivery(source.id);
    } else if (source.kind === "worktree") {
      projectId =
        state.projects.find((project) =>
          (state.projectData[project.id]?.worktrees ?? []).some(
            (worktree) => worktree.id === source.id,
          ),
        )?.id ?? null;
    }
    if (projectId) controller.selectProject(projectId);
    if (projectId) {
      if (source.kind === "issue" || source.kind === "pr") {
        controller.updateUi({ sel: source }, projectId);
      } else if (source.kind === "delivery") {
        const delivery = controller.deliveryById(source.id);
        if (delivery?.pr) {
          controller.updateUi(
            {
              sel: {
                kind: "pr",
                repo: delivery.pr.repo,
                number: delivery.pr.number,
              },
            },
            projectId,
          );
        } else if (delivery?.issues[0]) {
          controller.updateUi(
            {
              sel: {
                kind: "issue",
                repo: delivery.issues[0].repo,
                number: delivery.issues[0].number,
              },
            },
            projectId,
          );
        }
      } else {
        controller.updateUi({ sel: source }, projectId);
      }
    }
    onClose();
  };

  const renderItem = (item: InboxItem): ReactNode => {
    const approval =
      item.kind === "approval_needed"
        ? state.approvals[approvalIdOf(item) ?? ""] ?? null
        : null;
    const options = optionsOf(item);
    const busy = busyId === item.id;
    return (
      <article
        className="ov-inbox__item"
        key={item.id}
        aria-label={inboxKindLabel(item.kind)}
      >
        <div className="ov-inbox__head">
          <span className="ov-inbox__kind">{inboxKindLabel(item.kind)}</span>
          <button
            type="button"
            className="ov-inbox__summary"
            onClick={() => selectItem(item)}
          >
            <UntrustedText value={item.summary} />
          </button>
        </div>

        {item.kind === "approval_needed" ? (
          <div className="ov-inbox__approval">
            <span>
              {approval
                ? `${approvalActionLabel(approval.action)} · ${approvalTargetLabel(
                    approval.target,
                  )} · ${shortSha(approval.subject_digest)}`
                : "审批详情尚未载入。"}
            </span>
            <button
              type="button"
              className="ov-btn ov-btn--primary ov-btn--sm"
              disabled={!approvalIdOf(item)}
              onClick={() => {
                const approvalId = approvalIdOf(item);
                if (!approvalId) return;
                onApproval(approvalId);
                onClose();
              }}
            >
              审阅并签名…
            </button>
          </div>
        ) : null}

        {item.kind === "spec_needed" ? (
          <div className="ov-inbox__detail">
            {missingOf(item).map((missing) => (
              <p className="ov-inbox__missing" key={`${missing.aspect}-${missing.question}`}>
                <span className="ov-settings__meta">{missing.aspect}：</span>
                <UntrustedText value={missing.question} />
              </p>
            ))}
            <button
              type="button"
              className="ov-btn ov-btn--sm"
              disabled={!canReply}
              title={!canReply ? readonlyReason : undefined}
              onClick={() => replyToSpec(item)}
            >
              回复补充…
            </button>
          </div>
        ) : null}

        {item.kind === "security_finding" ? (
          <SecurityFinding
            item={item}
            confirm={confirmDismissId === item.id}
            canDismiss={canDismiss}
            busy={busy}
            onView={selectSource}
            onDismiss={() => void dismiss(item)}
            onCancel={() => setConfirmDismissId(null)}
          />
        ) : null}

        {item.kind !== "approval_needed" &&
        item.kind !== "spec_needed" &&
        item.kind !== "security_finding" &&
        options.length > 0 ? (
          <div className="ov-inbox__actions">
            {options.map((option) => (
              <button
                type="button"
                key={option.id}
                className={`ov-btn ov-btn--sm ${
                  option.primary ? "ov-btn--primary" : ""
                }`}
                disabled={!canChoose || busy}
                title={!canChoose ? readonlyReason : undefined}
                onClick={() => void choose(item, option)}
              >
                {option.label}
              </button>
            ))}
            {needsDismiss(item) ? (
              <DismissButton
                item={item}
                canDismiss={canDismiss}
                busy={busy}
                confirm={confirmDismissId === item.id}
                onDismiss={() => void dismiss(item)}
                onCancel={() => setConfirmDismissId(null)}
              />
            ) : null}
          </div>
        ) : null}

        {item.kind !== "approval_needed" &&
        item.kind !== "spec_needed" &&
        item.kind !== "security_finding" &&
        options.length === 0 ? (
          <div className="ov-inbox__actions">
            <DismissButton
              item={item}
              canDismiss={canDismiss}
              busy={busy}
              confirm={confirmDismissId === item.id}
              onDismiss={() => void dismiss(item)}
              onCancel={() => setConfirmDismissId(null)}
            />
          </div>
        ) : null}

        {item.kind === "escalation" && options.length === 0 ? (
          <span className="ov-settings__meta">
            忽略不会继续任务。
          </span>
        ) : null}
        {busy ? (
          <span role="status" className="ov-settings__meta">
            正在提交…
          </span>
        ) : null}
      </article>
    );
  };

  return (
    <Modal
      open
      label="需要你处理的事项"
      onClose={onClose}
      className="ov-inbox-dialog"
    >
      <div className="ov-inbox">
        <header className="ov-inbox__header">
          <span className="ov-inbox__eyebrow">
            需要你 · 全部项目 · {openItems.length} 项
          </span>
          <ScenarioMark ipc={controller.ipc} />
          <button
            type="button"
            className="ov-icon-btn"
            aria-label="关闭"
            onClick={onClose}
          >
            ×
          </button>
        </header>

        {error ? (
          <div role="alert" className="ov-alert">
            {error}
          </div>
        ) : null}

        {groups.length === 0 ? (
          <p className="ov-inbox__empty">没有需要你处理的事项。</p>
        ) : (
          groups.map((group) => (
            <section
              className="ov-inbox__group"
              key={group.key}
              aria-label={`${group.label} 项目`}
            >
              <div className="ov-inbox__group-head">
                <span className="ov-mono">{group.label}</span>
                <span className="ov-settings__meta">{group.items.length} 项</span>
              </div>
              {group.items.map(renderItem)}
            </section>
          ))
        )}
        <p className="ov-settings__meta">
          所有“需要你”表面只读同一份 open 列表；处理结果以服务端返回为准。
        </p>
      </div>
    </Modal>
  );
}

function DismissButton({
  item,
  canDismiss,
  busy,
  confirm,
  onDismiss,
  onCancel,
}: {
  item: InboxItem;
  canDismiss: boolean;
  busy: boolean;
  confirm: boolean;
  onDismiss: () => void;
  onCancel: () => void;
}): ReactNode {
  if (confirm) {
    return (
      <>
        <button
          type="button"
          className="ov-btn ov-btn--sm ov-btn--danger"
          disabled={!canDismiss || busy}
          onClick={onDismiss}
        >
          确认忽略
        </button>
        <button type="button" className="ov-btn ov-btn--sm" onClick={onCancel}>
          取消
        </button>
      </>
    );
  }
  return (
    <button
      type="button"
      className="ov-btn ov-btn--sm"
      disabled={!canDismiss || busy}
      title={!canDismiss ? "此设备为只读" : undefined}
      onClick={onDismiss}
    >
      {item.kind === "security_finding" ? "忽略…" : "忽略"}
    </button>
  );
}

function SecurityFinding({
  item,
  confirm,
  canDismiss,
  busy,
  onView,
  onDismiss,
  onCancel,
}: {
  item: InboxItem;
  confirm: boolean;
  canDismiss: boolean;
  busy: boolean;
  onView: (source: Ref) => void;
  onDismiss: () => void;
  onCancel: () => void;
}): ReactNode {
  const security = securityOf(item);
  if (!security) {
    return (
      <div className="ov-inbox__detail">
        <span className="ov-settings__meta">安全发现详情不可用。</span>
        <DismissButton
          item={item}
          canDismiss={canDismiss}
          busy={busy}
          confirm={confirm}
          onDismiss={onDismiss}
          onCancel={onCancel}
        />
      </div>
    );
  }
  return (
    <div className="ov-inbox__detail">
      <p>
        <span className="ov-settings__meta">
          {security.finding.severity} · {security.finding.category}：
        </span>
        <UntrustedText value={security.finding.title} />
      </p>
      <div className="ov-inbox__actions">
        <button
          type="button"
          className="ov-btn ov-btn--sm"
          onClick={() => onView(security.source)}
        >
          查看
        </button>
        <DismissButton
          item={item}
          canDismiss={canDismiss}
          busy={busy}
          confirm={confirm}
          onDismiss={onDismiss}
          onCancel={onCancel}
        />
      </div>
      <span className="ov-settings__meta">
        忽略安全发现需要二次确认；忽略不会继续被阻塞的任务。
      </span>
    </div>
  );
}

function optionsOf(item: InboxItem): Option[] {
  if (
    item.kind !== "decision_needed" &&
    item.kind !== "escalation" &&
    item.kind !== "budget_exhausted"
  ) {
    return [];
  }
  const payload = asRecord(item.payload);
  const options = payload?.options;
  if (!Array.isArray(options)) return [];
  return options
    .map((raw): Option | null => {
      const entry = asRecord(raw);
      const id = asString(entry?.id);
      const label = asString(entry?.label);
      if (!id || !label) return null;
      return { id, label, primary: entry?.primary === true };
    })
    .filter((option): option is Option => option !== null);
}

function needsDismiss(item: InboxItem): boolean {
  return item.kind === "escalation" || item.kind === "budget_exhausted";
}

function approvalIdOf(item: InboxItem): string | null {
  const payload = asRecord(item.payload);
  return payload ? asString(payload.approval_id) || null : null;
}

function missingOf(
  item: InboxItem,
): Array<{ aspect: string; question: UntrustedValue | string }> {
  const payload = asRecord(item.payload);
  const missing = payload?.missing;
  if (!Array.isArray(missing)) return [];
  const result: Array<{ aspect: string; question: UntrustedValue | string }> = [];
  for (const raw of missing) {
    const entry = asRecord(raw);
    const aspect = asString(entry?.aspect);
    if (!aspect) continue;
    const question = entry?.question;
    if (typeof question === "string") {
      result.push({ aspect, question });
      continue;
    }
    const untrusted = asRecord(question);
    if (untrusted && typeof untrusted.text === "string") {
      result.push({
        aspect,
        question: {
          text: untrusted.text,
          source: asString(untrusted.source) || "inbox.payload",
          truncated: untrusted.truncated === true,
        },
      });
    }
  }
  return result;
}

function securityOf(item: InboxItem): {
  source: Ref;
  finding: { severity: string; category: string; title: UntrustedValue };
} | null {
  const payload = asRecord(item.payload);
  const source = payload?.source;
  const finding = asRecord(payload?.finding);
  const title = asRecord(finding?.title);
  if (
    !source ||
    typeof source !== "object" ||
    !finding ||
    !title ||
    typeof title.text !== "string"
  ) {
    return null;
  }
  return {
    source: source as Ref,
    finding: {
      severity: asString(finding.severity) || "unknown",
      category: asString(finding.category) || "unknown",
      title: {
        text: title.text,
        source: asString(title.source) || "inbox.payload",
        truncated: title.truncated === true,
      },
    },
  };
}
