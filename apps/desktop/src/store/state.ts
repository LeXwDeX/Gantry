import type {
  Attempt,
  BudgetLedger,
  Conversation,
  Delivery,
  InboxItem,
  InboxSummary,
  Level,
  Me,
  Message,
  Project,
  Approval,
  Workflow,
  Worktree,
} from "@/api/types";
import type {
  ConnectionState,
  IdentityStatus,
  ProjectUiState,
} from "@/api/shell";

export interface ProjectAsOf {
  conversation: number | null;
  messages: number | null;
  deliveries: number | null;
  worktrees: number | null;
  /** 仅当前项目用来区分默认上限设置的水位线。 */
  settings: number | null;
}

export interface ProjectData {
  conversation: Conversation | null;
  messages: Message[];
  messageCursor: string | null;
  /** 含活动与最近完成的交付。 */
  deliveries: Delivery[];
  worktrees: Worktree[];
  defaultLevel: Level | null;
  asOf: ProjectAsOf;
}

export interface DesktopState {
  status: "loading" | "ready" | "error" | "unpaired";
  error: string | null;
  identity: IdentityStatus | null;
  connection: ConnectionState | null;
  me: Me | null;
  projects: Project[];
  currentProjectId: string | null;
  projectData: Record<string, ProjectData>;
  ui: Record<string, ProjectUiState>;
  inbox: InboxItem[];
  inboxSummary: InboxSummary | null;
  approvals: Record<string, Approval>;
  workflows: Record<string, Workflow>;
  /** 已读取的节点尝试；按节点聚合，旧尝试不覆盖。 */
  attemptsByNode: Record<string, Attempt[]>;
  /** 已读取的交付预算台账。 */
  budgetsByDelivery: Record<string, BudgetLedger>;
  notice: string | null;
  /** `settings.updated` 每来一次 +1，供设置表单感知外部修改。 */
  settingsRevision: number;
  resyncing: boolean;
  lastResyncReason: string | null;
}

export function emptyProjectData(): ProjectData {
  return {
    conversation: null,
    messages: [],
    messageCursor: null,
    deliveries: [],
    worktrees: [],
    defaultLevel: null,
    asOf: {
      conversation: null,
      messages: null,
      deliveries: null,
      worktrees: null,
      settings: null,
    },
  };
}

export function initialState(): DesktopState {
  return {
    status: "loading",
    error: null,
    identity: null,
    connection: null,
    me: null,
    projects: [],
    currentProjectId: null,
    projectData: {},
    ui: {},
    inbox: [],
    inboxSummary: null,
    approvals: {},
    workflows: {},
    attemptsByNode: {},
    budgetsByDelivery: {},
    notice: null,
    settingsRevision: 0,
    resyncing: false,
    lastResyncReason: null,
  };
}
