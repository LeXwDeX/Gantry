import type { ProjectUiState, Selection } from "@/api/shell";

export type { ProjectUiState, Selection };

export function defaultProjectUi(): ProjectUiState {
  return {
    view: "chat",
    sel: null,
    draft: "",
    ctx: null,
    levelOverride: null,
    needsOnly: false,
    showAll: { issues: false, prs: false, worktrees: false },
    expandedGroups: [],
    lastViewed: null,
    chatScroll: { atBottom: true, anchor_message_id: null, offset: 0 },
    outbox: [],
  };
}

/** 合并界面状态；嵌套的 showAll / chatScroll 保持默认键。 */
export function mergeProjectUi(
  current: ProjectUiState,
  patch: Partial<ProjectUiState>,
): ProjectUiState {
  return {
    ...current,
    ...patch,
    showAll: { ...current.showAll, ...(patch.showAll ?? {}) },
    chatScroll: { ...current.chatScroll, ...(patch.chatScroll ?? {}) },
    expandedGroups: patch.expandedGroups ?? current.expandedGroups,
    outbox: patch.outbox ?? current.outbox,
  };
}

export function sameUiSelection(
  a: Selection | null,
  b: Selection | null,
): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  if (a.kind !== b.kind) return false;
  if (a.kind === "base") return true;
  if (a.kind === "worktree") return b.kind === "worktree" && a.id === b.id;
  return b.kind !== "worktree" && b.kind !== "base" && a.repo === b.repo && a.number === b.number;
}
