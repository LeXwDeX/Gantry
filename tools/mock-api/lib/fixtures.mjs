import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

export const fixtureRoot = fileURLToPath(new URL("../../../fixtures/api/", import.meta.url));

const fixtureNames = [
  "server",
  "me",
  "devices",
  "pairing",
  "projects",
  "deliveries",
  "budgets",
  "conversations",
  "messages",
  "worktrees",
  "workflows",
  "workflows-list",
  "node",
  "device-renew",
  "attempts",
  "inbox",
  "inbox-summary",
  "approvals",
  "challenge",
  "settings",
  "catalog",
  "settings-status",
  "events",
];

export async function loadFixtureState() {
  const entries = await Promise.all(
    fixtureNames.map(async (name) => {
      const raw = await readFile(`${fixtureRoot}${name}.json`, "utf8");
      return [name, JSON.parse(raw)];
    }),
  );
  return structuredClone(Object.fromEntries(entries));
}

export function createApplicationState(fixtures) {
  const settings = structuredClone(fixtures.settings);
  return {
    server: fixtures.server,
    logEpoch: fixtures.events.log_epoch,
    head: fixtures.events.head,
    events: structuredClone(fixtures.events.events),
    me: fixtures.me,
    devices: structuredClone(fixtures.devices),
    pairing: structuredClone(fixtures.pairing),
    projects: structuredClone(fixtures.projects),
    deliveries: structuredClone(fixtures.deliveries),
    budgets: structuredClone(fixtures.budgets),
    conversations: structuredClone(fixtures.conversations),
    messages: structuredClone(fixtures.messages),
    worktrees: structuredClone(fixtures.worktrees),
    workflows: structuredClone(fixtures.workflows),
    "workflows-list": structuredClone(fixtures["workflows-list"]),
    node: structuredClone(fixtures.node),
    "device-renew": structuredClone(fixtures["device-renew"]),
    attempts: structuredClone(fixtures.attempts),
    inbox: structuredClone(fixtures.inbox),
    inboxSummary: structuredClone(fixtures["inbox-summary"]),
    approvals: structuredClone(fixtures.approvals),
    challenge: structuredClone(fixtures.challenge),
    settings,
    globalSettings: settings.global,
    projectSettings: settings.project,
    catalog: structuredClone(fixtures.catalog),
    settingsStatus: structuredClone(fixtures["settings-status"]),
    pushTokens: new Map(),
  };
}
