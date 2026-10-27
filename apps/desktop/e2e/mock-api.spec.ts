/**
 * 真实 API 模式（Mock）的 e2e：浏览器 → MockIpcAdapter → Vite 代理 → tools/mock-api。
 *
 * 场景控制用 tools/mock-api 自带的 /__mock/*（经 scripts/mock-harness.mjs 转发）。写请求由 Mock 自己产生事件；
 * 用例只为“服务端其他部分”（引擎、forge、policy）的进展注入事件，data 一律合法（Mock 按 schema 校验）。
 * 所有用例共享一个 Mock 实例，因此串行执行、每例前重置，并只在 desktop 项目里跑（窄屏由场景模式覆盖）。
 */
import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';
import type { InboxItem, Message } from '../src/api/types';
import {
  APPROVAL,
  APPROVAL_INBOX,
  DELIVERY,
  ESCALATION_INBOX,
  PR_HEAD,
  SELF,
  SPEC_INBOX,
  WEB,
  events,
  mockMessage,
  type InjectedEvent,
} from '../tests/unit/mockEvents';

export const MOCK_APP = 'http://127.0.0.1:4177';
const MOCK_API = 'http://127.0.0.1:8797';
const MOCK_CONTROL = 'http://127.0.0.1:8798/instances/default';
const SIGN = '用 Touch ID 签名批准';

test.describe.configure({ mode: 'serial' });
test.setTimeout(90_000);

test.beforeEach(async ({ request }, info) => {
  test.skip(info.project.name !== 'desktop', 'Mock 实例共享，只在 desktop 项目串行运行');
  const reset = await request.post(`${MOCK_CONTROL}/reset`);
  expect(reset.ok()).toBe(true);
});

async function control(page: Page, path: string, body: unknown = {}) {
  const response = await page.request.post(`${MOCK_CONTROL}${path}`, { data: body });
  expect(response.ok(), await response.text()).toBe(true);
}

async function emit(page: Page, event: InjectedEvent) {
  await control(page, '/events', event);
}

async function mutate(page: Page, collection: string, id: string, patch: Record<string, unknown>) {
  await control(page, '/mutate', { collection, id, patch });
}

async function getJson<T>(page: Page, path: string): Promise<T> {
  return (await (await page.request.get(`${MOCK_API}${path}`)).json()) as T;
}

async function operations(page: Page): Promise<string[]> {
  const response = await page.request.get(`${MOCK_CONTROL}/requests`);
  return ((await response.json()) as Array<{ operationId: string }>).map((r) => r.operationId);
}

type Pr = Record<string, unknown> & { checks: Record<string, unknown>; synced_at: string; head_sha: string };

async function currentPr(page: Page): Promise<Pr> {
  return (await getJson<{ pr: Pr }>(page, `/v1/deliveries/${DELIVERY}`)).pr;
}

function later(timestamp: string, seconds = 60): string {
  return new Date(Date.parse(timestamp) + seconds * 1000).toISOString();
}

/** forge 同步：PR #12 的合并闸门全部满足（GitHub 状态变化 + forge.synced）。 */
async function satisfyGates(page: Page) {
  const pr = await currentPr(page);
  await mutate(page, 'deliveries', DELIVERY, {
    pr: {
      ...pr,
      checks: { ...pr.checks, required_ok: true, failed: 0, pending: 0 },
      mergeable_state: 'clean',
      review_state: 'approved',
      synced_at: later(pr.synced_at),
    },
  });
  await emit(
    page,
    events.forgeSynced([
      { field: 'mergeable_state', from: pr.mergeable_state, to: 'clean' },
      { field: 'review_state', from: pr.review_state, to: 'approved' },
    ]),
  );
}

async function openApprovalSheet(page: Page, count = 2) {
  await page.getByRole('button', { name: `需要你 · ${count}`, exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: /审阅审批/ }).first().click();
  const sheet = page.getByRole('dialog');
  await expect(sheet.getByText(/^[a-f0-9]{40}$/).first()).toBeVisible();
  return sheet;
}

async function openWorkspace(page: Page, query = '') {
  await page.goto(`${MOCK_APP}/${query}`);
  await expect(page.getByRole('navigation', { name: '项目' })).toBeVisible();
  await expect(page.getByText('已连接', { exact: false }).first()).toBeVisible();
}

async function openWeb(page: Page) {
  await page.getByRole('navigation', { name: '项目' }).getByRole('button', { name: /web/ }).click();
  await expect(page.getByRole('complementary', { name: '项目信息' }).getByRole('button', { name: /^PR #12 / })).toBeVisible();
}

/** 服务端（engine / conversation）发出的一条消息：写入 Mock 并发 message.posted。 */
async function postServerMessage(page: Page, message: Message, projectId = WEB) {
  await control(page, '/mutate', { collection: 'messages', push: message });
  await emit(page, events.messagePosted(message, projectId));
}

/** 连续 ≥ 2 条事件行会折叠成“后台处理了 n 件事”（desktop-ui §12）；展开后再找具体的事件行。 */
async function expandEventGroup(page: Page, count: number) {
  await page.getByRole('main').getByRole('button', { name: `后台处理了 ${count} 件事` }).click({ timeout: 15_000 });
}

async function raiseInbox(page: Page, item: InboxItem) {
  await control(page, '/mutate', { collection: 'inbox', push: item });
  await emit(page, events.inboxRaised(item as unknown as Record<string, unknown> & { id: string }));
}

test('水合：项目 tab、徽标与顶栏计数来自 Mock，且不显示示例数据标记', async ({ page }) => {
  const external: string[] = [];
  page.on('request', (request) => {
    if (!request.url().startsWith(MOCK_APP) && !request.url().startsWith('data:')) external.push(request.url());
  });
  await openWorkspace(page);
  const nav = page.getByRole('navigation', { name: '项目' });
  await expect(nav.getByRole('button', { name: /web/ })).toBeVisible();
  await expect(nav.getByRole('button', { name: /api/ })).toBeVisible();
  await expect(nav.getByLabel('需要你 2 项')).toBeVisible();
  await expect(page.getByRole('button', { name: '需要你 · 2', exact: true })).toBeVisible();
  await expect(page.getByText('示例数据')).toHaveCount(0);
  // 所有请求都走同源代理，没有任何外部地址（I5 / CSP 前提）。
  expect(external).toEqual([]);
  const ops = await operations(page);
  expect(ops[0]).toBe('getMe');
  expect(ops).toContain('streamEvents');
  const { violations } = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
  expect(violations.filter(({ impact }) => impact === 'serious' || impact === 'critical')).toEqual([]);
});

test('空项目：对话视图显示空态卡，输入区可用', async ({ page }) => {
  await openWorkspace(page);
  // 夹具中 acme/api 没有对话也没有交付，是首屏项目。
  await expect(page.getByRole('navigation', { name: '项目' }).getByRole('button', { name: /api/ })).toHaveAttribute(
    'aria-current',
    'page',
  );
  const main = page.getByRole('main');
  await expect(main.getByRole('button', { name: '描述一个任务' })).toBeVisible();
  await expect(main.getByRole('button', { name: '从已有 Issue 开始' })).toBeVisible();
  await expect(main.getByRole('button', { name: '新建 Issue' })).toBeVisible();
  await expect(page.getByLabel('给 agent 发消息')).toBeEnabled();
});

test('E2E-01：选中不改变上下文；发送经 HTTP 落到 Mock，气泡出现', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await page.getByRole('complementary', { name: '项目信息' }).getByRole('button', { name: /^PR #12 / }).click();
  await expect(page.getByTitle('选择这条消息的上下文')).toHaveText(/@ 全项目/);
  await page.getByLabel('给 agent 发消息').fill('Mock 模式 e2e 消息');
  await page.getByRole('button', { name: '发送', exact: true }).click();
  await expect(page.getByRole('main').getByText('Mock 模式 e2e 消息')).toBeVisible();
  await expect.poll(async () => (await operations(page)).filter((op) => op === 'postMessage').length).toBe(1);
});

test('E2E-11：经 Mock 注入的不可信内容不生成元素、不发外部请求，链接旁显示目标主机', async ({ page }) => {
  const external: string[] = [];
  page.on('request', (request) => {
    if (!request.url().startsWith(MOCK_APP) && !request.url().startsWith('data:')) external.push(request.url());
  });
  await openWorkspace(page);
  await openWeb(page);
  const text = [
    '<script>window.__pwned = 1</script>',
    '<img src=x onerror="window.__pwned = 2">',
    '<iframe src="https://evil.example/"></iframe>',
    '<svg onload="window.__pwned = 3"></svg>',
    '[看起来是 GitHub](https://evil.example/login)',
    '![追踪像素](https://evil.example/p.png)',
    '[脚本](javascript:alert(1))',
    '‮反向文字‬ 零​宽',
  ].join('\n\n');
  await postServerMessage(page, mockMessage(50, { text }));
  const main = page.getByRole('main');
  await expect(main.getByText(/追踪像素/).first()).toBeVisible({ timeout: 15_000 });
  expect(await page.evaluate(() => (window as unknown as { __pwned?: number }).__pwned)).toBeUndefined();
  expect(await main.locator('script, iframe, svg[onload], img[src*="evil"], img[onerror]').count()).toBe(0);
  await expect(main.getByText('evil.example', { exact: false }).first()).toBeVisible();
  expect(external).toEqual([]);
});

test('E2E-05：Mock 对发消息返回 403 forbidden_role 时只显示失败，不崩溃', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await control(page, '/fail-next', { operation_id: 'postMessage', status: 403, code: 'forbidden_role' });
  await page.getByLabel('给 agent 发消息').fill('会被拒绝的消息');
  await page.getByRole('button', { name: '发送', exact: true }).click();
  await expect(page.getByRole('region', { name: '待发送消息' }).getByText('会被拒绝的消息')).toBeVisible({
    timeout: 15_000,
  });
  await expect(page.getByRole('navigation', { name: '项目' })).toBeVisible();
});

test('E2E-21：设置页显示“尚未应用最新 rev”，模块追上后变为已应用', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '设置', exact: true }).click();
  await page.getByRole('button', { name: /^档位与预算/ }).first().click();
  await expect(page.getByText(/尚未应用最新 rev 18238/).first()).toBeVisible({ timeout: 15_000 });
  const status = await getJson<{ latest_rev: number; modules: Record<string, Record<string, unknown>> }>(
    page,
    '/v1/settings/status',
  );
  const caughtUp = Object.fromEntries(
    Object.entries(status.modules).map(([name, entry]) => [name, { ...entry, applied_rev: status.latest_rev }]),
  );
  await control(page, '/mutate', { set: 'settingsStatus', value: { ...status, modules: caughtUp } });
  await page.getByRole('button', { name: /^外观/ }).first().click();
  await page.getByRole('button', { name: /^档位与预算/ }).first().click();
  await expect(page.getByText('已应用最新 rev').first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText(/尚未应用最新 rev/)).toHaveCount(0);
});

test('E2E-05：只读设备禁用输入并隐藏设置', async ({ page }) => {
  await mutate(page, 'devices', SELF, { role: 'viewer' });
  await openWorkspace(page);
  await expect(page.getByLabel('给 agent 发消息')).toBeDisabled();
  await expect(page.getByRole('button', { name: '设置', exact: true })).toHaveCount(0);
});

test('E2E-18：上报项解决后，tab 徽标、顶栏计数、左栏药丸与检视器同时变化，没有残留', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  const left = page.getByRole('complementary', { name: '项目信息' });
  const inspector = page.getByRole('complementary', { name: '检视器' });
  await left.getByRole('button', { name: /^PR #12 / }).click();
  await expect(page.getByRole('button', { name: '需要你 · 2', exact: true })).toBeVisible();
  await expect(left.getByText(/! 需要你/).first()).toBeVisible();
  await expect(inspector.getByText('! 需要你').first()).toBeVisible();

  await mutate(page, 'inbox', ESCALATION_INBOX, { status: 'resolved' });
  await emit(page, events.inboxResolved(ESCALATION_INBOX, 'chosen', { option_id: 'narrow' }));
  await expect(page.getByRole('button', { name: '需要你 · 1', exact: true })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('navigation', { name: '项目' }).getByLabel('需要你 1 项')).toBeVisible();

  // 审批也处理完：所有表面同时消失。
  await mutate(page, 'inbox', APPROVAL_INBOX, { status: 'resolved' });
  await emit(page, events.inboxResolved(APPROVAL_INBOX, 'approval_settled', { approval_id: APPROVAL }));
  await expect(page.getByRole('button', { name: '需要你 · 0', exact: true })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('navigation', { name: '项目' }).getByLabel(/需要你 \d+ 项/)).toHaveCount(0);
  await expect(left.getByText(/! 需要你/)).toHaveCount(0);
  await expect(inspector.getByText('! 需要你')).toHaveCount(0);
});

test('E2E-07：GitHub 闸门未满足时签名禁用并列出缺项；闸门满足后即时启用', async ({ page }) => {
  await openWorkspace(page);
  const sheet = await openApprovalSheet(page);
  const sign = sheet.getByRole('button', { name: SIGN });
  await expect(sign).toBeDisabled();
  await expect(sign).toHaveAttribute('title', /必需 checks 未通过/);
  await expect(sheet.getByRole('button', { name: /去 GitHub 上 approve/ })).toBeVisible();
  await satisfyGates(page);
  await expect(sign).toBeEnabled({ timeout: 15_000 });
});

test('E2E-08 / E2E-09：审批单签名经外壳替身 grant，IPC 只传 preparedId', async ({ page }) => {
  await openWorkspace(page);
  await satisfyGates(page);
  const sheet = await openApprovalSheet(page);
  await expect(sheet.getByText(/未签名信息/)).toBeVisible();
  const sign = sheet.getByRole('button', { name: SIGN });
  await expect(sign).toBeEnabled({ timeout: 15_000 });
  await sign.click();
  await expect.poll(async () => (await operations(page)).includes('grantApproval')).toBe(true);
  // 只按 grant 响应里的 Approval.status 更新（api.md §6.5 规则 5），不做乐观显示。
  await expect(sheet.getByRole('status').filter({ hasText: '签名请求已返回：已签名 · 等待合并' })).toBeVisible();
  await expect(sheet.getByRole('button', { name: SIGN })).toBeDisabled();
  const calls = await page.evaluate(() => window.__GANTRY_MOCK__?.calls() ?? []);
  const signCalls = calls.filter((c) => c.command === 'approval_sign');
  expect(signCalls).toHaveLength(1);
  expect(Object.keys(signCalls[0].args)).toEqual(['preparedId']);
  for (const call of calls) {
    for (const key of ['signature', 'nonce', 'payload', 'subject_digest']) {
      expect(Object.keys(call.args)).not.toContain(key);
    }
  }
});

test('E2E-04：断线时签名禁用且不产生 grant；恢复后可签', async ({ page }) => {
  await openWorkspace(page);
  await satisfyGates(page);
  const sheet = await openApprovalSheet(page);
  await expect(sheet.getByRole('button', { name: SIGN })).toBeEnabled({ timeout: 15_000 });
  await page.evaluate(() => window.__GANTRY_MOCK__?.disconnect());
  await expect(sheet.getByText('连接中断，暂不能签名。')).toBeVisible();
  await expect(sheet.getByRole('button', { name: SIGN })).toBeDisabled();
  expect(await operations(page)).not.toContain('grantApproval');
  await page.evaluate(() => window.__GANTRY_MOCK__?.reconnect());
  await expect(sheet.getByRole('button', { name: SIGN })).toBeEnabled({ timeout: 15_000 });
});

test('E2E-03：审批单打开时新推送使审批失效，签名禁用并丢弃 prepared 句柄', async ({ page }) => {
  await openWorkspace(page);
  await satisfyGates(page);
  const sheet = await openApprovalSheet(page);
  await expect(sheet.getByRole('button', { name: SIGN })).toBeEnabled({ timeout: 15_000 });

  const pr = await currentPr(page);
  const newHead = 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678';
  // 新推送：forge 同步带来新 head，随后审批因 head 变化失效。
  await mutate(page, 'deliveries', DELIVERY, { pr: { ...pr, head_sha: newHead, synced_at: later(pr.synced_at, 120) } });
  await emit(page, events.forgeSynced([{ field: 'head_sha', from: pr.head_sha, to: newHead }]));
  await mutate(page, 'approvals', APPROVAL, { status: 'invalidated' });
  await emit(page, events.approvalInvalidated(pr.head_sha, newHead));

  await expect(sheet.getByText(/已失效（9f2c4e7 → a1b2c3d），重新评审中/).first()).toBeVisible({ timeout: 15_000 });
  await expect(sheet.getByRole('button', { name: SIGN })).toHaveCount(0);
  const calls = await page.evaluate(() => window.__GANTRY_MOCK__?.calls() ?? []);
  expect(calls.some((c) => c.command === 'approval_discard')).toBe(true);
  expect(await operations(page)).not.toContain('grantApproval');
  // 之后直接 grant 也会失败：审批已非 pending。
  const late = await page.request.post(`${MOCK_API}/v1/approvals/${APPROVAL}/grant`, {
    headers: { 'Idempotency-Key': 'late-grant-key-00000001' },
    data: { nonce: 'A'.repeat(43), signature: 'A'.repeat(86) },
  });
  expect(late.status()).toBe(409);
});

test('challenge 过期：审批单提示过期并可重新载入', async ({ page }) => {
  await page.clock.install();
  await openWorkspace(page);
  await satisfyGates(page);
  const sheet = await openApprovalSheet(page);
  await expect(sheet.getByRole('button', { name: SIGN })).toBeEnabled({ timeout: 15_000 });
  // challenge 有效期 120 秒（C27）；浏览器时钟快进 3 分钟。
  await page.clock.fastForward(180_000);
  await expect(sheet.getByText(/已过期/).first()).toBeVisible({ timeout: 15_000 });
  await expect(sheet.getByRole('button', { name: SIGN })).toBeDisabled();
  await sheet.getByRole('button', { name: /重新载入/ }).first().click();
  await expect(sheet.getByText(/^[a-f0-9]{40}$/).first()).toBeVisible();
  expect((await operations(page)).filter((op) => op === 'getApprovalChallenge').length).toBeGreaterThanOrEqual(2);
});

// E2E-00 在空项目 acme/api 上从零开始：没有对话、交付、PR、审批，全部由任务消息之后的事件逐步产生。
const API = 'prj_01JB7K2N0R5X9V4P6S7T2W1YZB';
const E0 = {
  conversation: 'cnv_01JB7R00000000000000000001',
  delivery: 'dlv_01JB7R00000000000000000002',
  workflow: 'wfl_01JB7R00000000000000000003',
  approval: 'apr_01JB7R00000000000000000004',
  inbox: 'inb_01JB7R00000000000000000005',
  head: 'c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00',
  merge: '1405df66cbe219b0bf6355bc3d60361a8376b6b4',
};
const engine = { kind: 'engine', id: 'engine' };
const forgeActor = { login: 'gantry-bot', github_id: 9988776, type: 'bot', association: 'NONE' };

function forgeData(kind: string, data: Record<string, unknown>, target: { type: string; number: number }) {
  return {
    schema: 'forge.event.v1',
    kind,
    dedup_key: `dedup:${kind}:e2e00`,
    source: 'webhook',
    delivery_ids: [E0.delivery],
    project_id: API,
    repo: 'acme/api',
    installation_id: 1,
    delivery_id: E0.delivery,
    target: { ...target, node_id: 'PR_e2e00' },
    actor: forgeActor,
    origin: 'self_unattributed',
    caused_by_action: null,
    occurred_at: '2026-10-24T12:50:00.000Z',
    observed_at: '2026-10-24T12:50:00.000Z',
    stale: false,
    data,
  };
}

async function emitFor(page: Page, type: string, subject: string, data: unknown, actor = engine) {
  await emit(page, { type, subject, data, project_id: API, correlation_id: E0.delivery, actor });
}

test('E2E-00：从空项目发起 Low 任务 → 新交付与计划卡 → 阶段推进 → PR → CI → 审批签名 → 合并 → Issue 关闭', async ({ page }) => {
  const ts = '2026-10-24T12:45:00.000Z';
  await control(page, '/mutate', {
    collection: 'conversations',
    push: { id: E0.conversation, project_id: API, created_at: ts, updated_at: ts },
  });
  await openWorkspace(page);
  const nav = page.getByRole('navigation', { name: '项目' });
  await expect(nav.getByRole('button', { name: /api/ })).toHaveAttribute('aria-current', 'page');
  const left = page.getByRole('complementary', { name: '项目信息' });
  const main = page.getByRole('main');
  await expect(left.getByRole('button', { name: /^进行中 Issue 0/ })).toBeVisible();
  await expect(nav.getByRole('button', { name: /api/ }).getByLabel(/需要你/)).toHaveCount(0);

  // 1. 用户选 Low 上限并发出任务：请求体带 level_cap = Low。
  await page.getByRole('radio', { name: 'Low' }).click();
  await page.getByLabel('给 agent 发消息').fill('给健康检查接口加上版本号');
  await page.getByRole('button', { name: '发送', exact: true }).click();
  await expect(main.getByText('给健康检查接口加上版本号')).toBeVisible();
  const posted = await getJson<{ items: Message[] }>(page, `/v1/conversations/${E0.conversation}/messages?limit=10`);
  const userMessage = posted.items.find((m) => m.role === 'user');
  expect(userMessage?.level_cap).toBe('Low');

  // 2. conversation 建交付（Low：跳过计划，直接实现），agent 回复计划卡。
  const delivery = {
    id: E0.delivery,
    project_id: API,
    title: '健康检查接口加版本号',
    level_cap: 'Low',
    level_effective: 'Low',
    stage: 'implement',
    stage_status: 'running',
    issues: [
      {
        repo: 'acme/api',
        number: 7,
        node_id: 'I_e2e00',
        title: { text: 'Health check should report version', source: 'github:issue.title', truncated: false },
        state: 'open',
        state_reason: null,
        synced_at: ts,
      },
    ],
    pr: null,
    worktree_id: null,
    parent_id: null,
    depends_on: [],
    origin: 'conversation',
    created_at: ts,
    updated_at: ts,
  };
  await control(page, '/mutate', { collection: 'deliveries', push: delivery });
  await emitFor(page, 'delivery.created', E0.delivery, {
    title: delivery.title,
    level_cap: 'Low',
    level_effective: 'Low',
    origin: 'conversation',
    source_message_id: userMessage?.id,
    user_text: '给健康检查接口加上版本号',
    issues: [],
    config_rev: 18238,
    budget: {
      template: 'low.single',
      limits: {
        tokens_by_tier: { utility: 100000, planner: 50000, worker: 1000000 },
        tokens: null,
        nodes: 4,
        rounds: 1,
        wall_time: 3600,
        concurrency: 1,
        forge_writes: 20,
      },
    },
  });
  await postServerMessage(
    page,
    mockMessage(60, {
      conversation_id: E0.conversation,
      text: 'Low 档：单节点实现后开 PR。',
      delivery_id: E0.delivery,
      cards: [
        {
          kind: 'plan',
          delivery_id: E0.delivery,
          workflow_id: E0.workflow,
          summary: { text: '实现 → 开 PR → CI → 合并', source: 'agent:planner', truncated: false },
        },
      ],
    } as Partial<Message>),
    API,
  );
  await expect(main.getByText('Low 档：单节点实现后开 PR。')).toBeVisible({ timeout: 15_000 });
  await expect(main.getByText('实现 → 开 PR → CI → 合并')).toBeVisible();
  await expect(left.getByRole('button', { name: /^#7 / })).toBeVisible();

  // 3. 实现完成，开 PR，进入评审。
  const pr = {
    repo: 'acme/api',
    number: 13,
    node_id: 'PR_e2e00',
    title: { text: 'feat: report version in health check', source: 'github:pr.title', truncated: false },
    state: 'open',
    head_sha: E0.head,
    base_ref: 'main',
    base_sha: E0.merge,
    draft: false,
    mergeable_state: 'blocked',
    in_merge_queue: false,
    checks: { passed: 0, failed: 0, pending: 3, required_ok: false },
    review_state: 'none',
    synced_at: later(ts, 60),
  };
  await mutate(page, 'deliveries', E0.delivery, { stage: 'review', pr });
  await emitFor(
    page,
    'forge.pr_opened',
    E0.delivery,
    forgeData(
      'pr_opened',
      {
        number: 13,
        node_id: 'PR_e2e00',
        title: pr.title.text,
        body: 'Closes #7',
        head: { ref: 'gantry/7-health-version', sha: E0.head, repo: 'acme/api' },
        base: { ref: 'main' },
        draft: false,
        labels: [],
        author: forgeActor,
        closing_issues: [7],
        created_by_gantry: true,
      },
      { type: 'pr', number: 13 },
    ),
    { kind: 'forge', id: 'forge' },
  );
  await emitFor(page, 'delivery.stage_changed', E0.delivery, { from: 'implement', to: 'review', stage_status: 'running' });
  await expect(left.getByRole('button', { name: /^PR #13 / })).toBeVisible({ timeout: 15_000 });

  // 4. CI 通过、review 通过；policy 发起 merge 审批，上报“需要你”，agent 发审批卡。
  await mutate(page, 'deliveries', E0.delivery, {
    stage: 'merge',
    pr: {
      ...pr,
      checks: { passed: 3, failed: 0, pending: 0, required_ok: true },
      mergeable_state: 'clean',
      review_state: 'approved',
      synced_at: later(ts, 120),
    },
  });
  await emitFor(
    page,
    'forge.synced',
    E0.delivery,
    {
      target: { type: 'pr', number: 13, node_id: 'PR_e2e00' },
      source: 'webhook',
      changes: [{ field: 'mergeable_state', from: 'blocked', to: 'clean' }],
      synced_at: later(ts, 120),
    },
    { kind: 'forge', id: 'forge' },
  );
  await emitFor(page, 'delivery.stage_changed', E0.delivery, { from: 'review', to: 'merge', stage_status: 'waiting_human' });
  const approval = {
    id: E0.approval,
    delivery_id: E0.delivery,
    action: 'merge_pr',
    target: { kind: 'pr', repo: 'acme/api', number: 13 },
    subject_digest: E0.head,
    device_id: null,
    signature: null,
    status: 'pending',
    expires_at: later(ts, 86_400),
    consumed_by: null,
    created_at: later(ts, 130),
    updated_at: later(ts, 130),
  };
  await control(page, '/mutate', { collection: 'approvals', push: approval });
  await emitFor(page, 'approval.requested', E0.approval, {
    action: 'merge_pr',
    target: approval.target,
    subject_digest: E0.head,
    expires_at: approval.expires_at,
    config_rev: 18238,
    requested_by: engine,
  });
  const item = {
    id: E0.inbox,
    project_id: API,
    delivery_id: E0.delivery,
    kind: 'approval_needed',
    severity: 'blocker',
    summary: 'PR #13 可以合并，需要你的审批。',
    payload: { approval_id: E0.approval },
    status: 'open',
    dedup_key: `approval:${E0.approval}`,
    created_at: later(ts, 131),
    updated_at: later(ts, 131),
  } as unknown as InboxItem;
  await raiseInbox(page, item);
  await postServerMessage(
    page,
    mockMessage(61, {
      conversation_id: E0.conversation,
      text: 'PR #13 已就绪，合并前需要你的审批。',
      delivery_id: E0.delivery,
      cards: [{ kind: 'approval', approval_id: E0.approval }],
    } as Partial<Message>),
    API,
  );
  await expect(nav.getByRole('button', { name: /api/ }).getByLabel('需要你 1 项')).toBeVisible({ timeout: 15_000 });
  await expect(left.getByText(/! 需要你/).first()).toBeVisible();

  // 5. 在审批单签名。
  await main.getByRole('button', { name: '审阅审批…' }).last().click();
  const sheet = page.getByRole('dialog');
  await expect(sheet.getByText(E0.head)).toBeVisible();
  await sheet.getByRole('button', { name: SIGN }).click({ timeout: 15_000 });
  await expect(sheet.getByRole('status').filter({ hasText: '已签名 · 等待合并' })).toBeVisible();
  await sheet.getByRole('button', { name: '关闭' }).click();
  await expect(main.getByText('已签名 · 等待合并', { exact: true }).first()).toBeVisible();

  // 6. Outbox 执行 merge：审批被消费、PR 合并、上报项随审批了结。
  await mutate(page, 'approvals', E0.approval, { status: 'consumed', consumed_by: 'act_merge' });
  await emitFor(page, 'approval.consumed', E0.approval, {
    approval_id: E0.approval,
    consumed_by: 'act_merge',
    subject_digest: E0.head,
  });
  await mutate(page, 'deliveries', E0.delivery, {
    pr: { ...pr, state: 'merged', checks: { passed: 3, failed: 0, pending: 0, required_ok: true }, synced_at: later(ts, 300) },
  });
  await emitFor(
    page,
    'forge.pr_merged',
    E0.delivery,
    forgeData(
      'pr_merged',
      {
        merge_commit_sha: E0.merge,
        head_sha: E0.head,
        base_ref: 'main',
        merged_by: forgeActor,
        via_merge_queue: false,
        merge_method: 'squash',
      },
      { type: 'pr', number: 13 },
    ),
    { kind: 'forge', id: 'forge' },
  );
  await mutate(page, 'inbox', E0.inbox, { status: 'resolved' });
  await emitFor(page, 'inbox.resolved', E0.inbox, {
    inbox_id: E0.inbox,
    resolution: 'approval_settled',
    approval_id: E0.approval,
  });
  await expect(main.getByText(`已合并 ${E0.head.slice(0, 7)}`, { exact: true }).first()).toBeVisible({ timeout: 15_000 });
  await expect(nav.getByRole('button', { name: /api/ }).getByLabel(/需要你/)).toHaveCount(0);

  // 7. Issue 关闭，交付完成并移入“最近完成”。
  await mutate(page, 'deliveries', E0.delivery, { stage: 'closed', stage_status: 'done' });
  await emitFor(
    page,
    'forge.issue_closed',
    E0.delivery,
    forgeData('issue_closed', { number: 7, state_reason: 'completed', closed_by: forgeActor, closed_via_pr: 13 }, { type: 'issue', number: 7 }),
    { kind: 'forge', id: 'forge' },
  );
  await emitFor(page, 'delivery.stage_changed', E0.delivery, { from: 'merge', to: 'closed', stage_status: 'done' });
  await expect(main.getByText('Issue 已关闭 · 交付完成', { exact: true }).first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('region', { name: '最近完成' }).getByRole('button', { name: /PR #13|#7/ }).first()).toBeVisible();
  await expect(page.getByRole('region', { name: '进行中 PR' }).getByRole('button', { name: /^PR #13 / })).toHaveCount(0);
});

test('E2E-19：GitHub 上直接合并 → 事件行出现，上报项消失，审批卡显示最终状态', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  const pr = await currentPr(page);
  await mutate(page, 'approvals', APPROVAL, { status: 'consumed', consumed_by: 'external' });
  await emit(page, events.approvalConsumed('external'));
  await mutate(page, 'deliveries', DELIVERY, { pr: { ...pr, state: 'merged', synced_at: later(pr.synced_at, 300) } });
  await emit(page, events.prMerged('1405df66cbe219b0bf6355bc3d60361a8376b6b4'));
  await mutate(page, 'inbox', APPROVAL_INBOX, { status: 'resolved' });
  await emit(page, events.inboxResolved(APPROVAL_INBOX, 'external', { approval_id: APPROVAL }));
  await postServerMessage(
    page,
    mockMessage(2, {
      role: 'event',
      agent_kind: null,
      tone: 'info',
      text: 'PR #12 已在 GitHub 上被合并',
      delivery_id: DELIVERY,
      refs: [{ kind: 'pr', repo: 'acme/web', number: 12 }],
    }),
  );
  const main = page.getByRole('main');
  await expandEventGroup(page, 2);
  await expect(main.getByRole('button', { name: /PR #12 已在 GitHub 上被合并/ })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('button', { name: '需要你 · 1', exact: true })).toBeVisible();
  await expect(main.getByText(`已合并 ${PR_HEAD.slice(0, 7)}`, { exact: true }).first()).toBeVisible();
  await expect(main.getByRole('button', { name: '审阅审批…' })).toHaveCount(0);
});

test('策略拒绝上报：policy.denied、事件行与 decision_needed 上报项一起呈现；上报项的未知 reason_code 不崩溃', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await emit(page, events.policyDenied('policy.patch_denied'));
  await postServerMessage(
    page,
    mockMessage(3, {
      role: 'event',
      agent_kind: null,
      tone: 'warn',
      text: '补丁改动 .github/workflows/ci.yml，被策略拒绝',
      delivery_id: DELIVERY,
      refs: [{ kind: 'pr', repo: 'acme/web', number: 12 }],
    }),
  );
  const item = {
    id: 'inb_01JB7Q00000000000000000001',
    project_id: WEB,
    delivery_id: DELIVERY,
    kind: 'decision_needed',
    severity: 'blocker',
    summary: '补丁改动了 workflow 文件，需要人工处理',
    payload: {
      kind: 'manual_action',
      reason_code: 'policy.workflow_path_future_unknown',
      options: [
        { id: 'replan', label: '重新规划', primary: true },
        { id: 'abandon', label: '放弃' },
      ],
    },
    status: 'open',
    dedup_key: 'decision:dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB:policy.workflow_path',
    created_at: '2026-10-24T12:46:00.000Z',
    updated_at: '2026-10-24T12:46:00.000Z',
  } as unknown as InboxItem;
  await raiseInbox(page, item);
  await expandEventGroup(page, 2);
  await expect(page.getByRole('main').getByRole('button', { name: /补丁改动 \.github\/workflows\/ci\.yml，被策略拒绝/ })).toBeVisible({
    timeout: 15_000,
  });
  await expect(page.getByRole('button', { name: '需要你 · 3', exact: true })).toBeVisible();
  await page.getByRole('button', { name: '需要你 · 3', exact: true }).click();
  const inbox = page.getByRole('dialog');
  await expect(inbox.getByText('补丁改动了 workflow 文件，需要人工处理')).toBeVisible();
  await expect(inbox.getByRole('button', { name: '重新规划' })).toBeVisible();
});

test('E2E-15：写入模型凭据走签名链；未签名的直接 PUT 被 403；凭据值不回显、不进 IPC 录制', async ({ page }) => {
  const secret = 'sk-test-e2e-credential-0000';
  await openWorkspace(page);
  await page.getByRole('button', { name: '设置', exact: true }).click();
  await page.getByRole('button', { name: /^模型/ }).first().click();
  const input = page.getByLabel('anthropic API key');
  await input.fill(secret);
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await expect(page.getByText('此改动需要审批设备签名', { exact: false })).toBeVisible({ timeout: 15_000 });
  await expect(input).toHaveValue('');
  await page.getByRole('button', { name: '签名并提交…' }).click();
  await expect(page.getByText(/已保存（rev \d+）/)).toBeVisible({ timeout: 15_000 });
  // rev = 本次 settings.updated 的 seq（config.md §2.7），与服务端读回的 rev 一致。
  const rev = (await getJson<{ rev: number }>(page, '/v1/settings/models')).rev;
  await expect(page.getByText(`已保存（rev ${rev}）`, { exact: false })).toBeVisible();
  await expect(page.getByText(`当前版本 rev ${rev}`, { exact: false })).toBeVisible();
  const chain = new Set(['createApproval', 'getApprovalChallenge', 'grantApproval', 'putSettings']);
  expect((await operations(page)).filter((op) => chain.has(op))).toEqual([
    'putSettings',
    'createApproval',
    'getApprovalChallenge',
    'grantApproval',
    'putSettings',
  ]);
  // 无签名的直接写入被拒绝。
  const etag = (await page.request.get(`${MOCK_API}/v1/settings/models`)).headers().etag;
  const direct = await page.request.put(`${MOCK_API}/v1/settings/models`, {
    headers: { 'If-Match': etag },
    data: {
      value: {
        providers: {
          anthropic: {
            kind: 'anthropic',
            base_url: 'https://api.anthropic.com',
            credential: { type: 'api_key', secret_ref: 'model.anthropic.main', value: secret },
          },
        },
      },
    },
  });
  expect(direct.status()).toBe(403);
  expect((await direct.json()).code).toBe('signature_required');
  // 值不出现在界面、读取结果和 IPC 录制里。
  expect(await page.content()).not.toContain(secret);
  expect(JSON.stringify(await getJson(page, '/v1/settings/models'))).not.toContain(secret);
  const calls = await page.evaluate(() => window.__GANTRY_MOCK__?.calls() ?? []);
  expect(JSON.stringify(calls)).not.toContain(secret);
});

test('证书将过期：顶栏提示；到期后转 cert_expired 阻塞配对向导', async ({ page }) => {
  const me = await getJson<Record<string, unknown> & { server: { time: string }; certificate: Record<string, string> }>(
    page,
    '/v1/me',
  );
  await control(page, '/mutate', {
    set: 'me',
    value: { ...me, certificate: { ...me.certificate, not_after: later(me.server.time, 2 * 86_400) } },
  });
  await page.goto(`${MOCK_APP}/`);
  await expect(page.getByText('证书将过期').first()).toBeVisible({ timeout: 15_000 });
  await page.evaluate(() => window.__GANTRY_MOCK__?.expireCertificate());
  await expect(page.getByRole('main', { name: '配对设备' })).toBeVisible();
});

test('E2E-14：吊销本机后出现阻塞的配对向导，之后不再发请求', async ({ page }) => {
  await openWorkspace(page);
  await page.evaluate(() => window.__GANTRY_MOCK__?.revoke());
  await expect(page.getByRole('main', { name: '配对设备' })).toBeVisible();
  await expect(page.getByRole('navigation', { name: '项目' })).toHaveCount(0);
  const before = (await operations(page)).length;
  await page.waitForTimeout(500);
  expect((await operations(page)).length).toBe(before);

  // 重置本机身份后重新配对，界面正常水合。
  await page.getByRole('button', { name: '重置本机身份…' }).click();
  await page.getByRole('button', { name: '确认重置' }).click();
  await page.getByLabel('配对串').fill('gantry+pair://mock-token');
  await page.getByLabel('设备名').fill('e2e Mac');
  await page.getByRole('button', { name: '开始配对' }).click();
  await page.getByRole('button', { name: '一致', exact: true }).click();
  await expect(page.getByRole('navigation', { name: '项目' })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('button', { name: '需要你 · 2', exact: true })).toBeVisible();
});

test('自动滚动：离开底部时新消息出现“新消息 ↓”，点击回到底部', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  for (let index = 10; index < 40; index += 1) {
    await postServerMessage(page, mockMessage(index, { text: `填充消息 ${index}` }));
  }
  const list = page.getByRole('main').locator('.ws-chat__scroll');
  await expect(page.getByRole('main').getByText('填充消息 39')).toBeVisible({ timeout: 15_000 });
  await list.evaluate((element) => {
    element.scrollTop = 0;
    element.dispatchEvent(new Event('scroll'));
  });
  await postServerMessage(page, mockMessage(40, { text: '离开底部后的新消息' }));
  const jump = page.getByRole('button', { name: '新消息 ↓' });
  await expect(jump).toBeVisible({ timeout: 15_000 });
  await jump.click();
  await expect(page.getByRole('main').getByText('离开底部后的新消息')).toBeInViewport();
  await expect(jump).toHaveCount(0);
});

test('配对向导：未配对启动 → 确认码来自 Mock → 一致 → 进入工作区；不一致回到起始', async ({ page }) => {
  await page.goto(`${MOCK_APP}/?paired=0`);
  await expect(page.getByRole('main', { name: '配对设备' })).toBeVisible();
  await page.getByLabel('配对串').fill('gantry+pair://mock-token');
  await page.getByLabel('设备名').fill('e2e Mac');
  await page.getByRole('button', { name: '开始配对' }).click();
  await expect(page.getByText(/^\d{3} \d{3}$/)).toBeVisible();
  await page.getByRole('button', { name: '不一致' }).click();
  await expect(page.getByRole('button', { name: '开始配对' })).toBeVisible();

  await page.getByLabel('配对串').fill('gantry+pair://mock-token');
  await page.getByLabel('设备名').fill('e2e Mac');
  await page.getByRole('button', { name: '开始配对' }).click();
  await expect(page.getByText(/^\d{3} \d{3}$/)).toBeVisible();
  await page.getByRole('button', { name: '一致', exact: true }).click();
  await expect(page.getByRole('navigation', { name: '项目' })).toBeVisible({ timeout: 15_000 });
  expect((await operations(page)).filter((op) => op === 'pairDevice')).toHaveLength(2);
});

test('检视器 ⋯ 暂停经 HTTP 调用 pauseDelivery，界面随 delivery.paused 更新', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await page.getByRole('complementary', { name: '项目信息' }).getByRole('button', { name: /^PR #12 / }).click();
  const inspector = page.getByRole('complementary', { name: '检视器' });
  await inspector.getByRole('button', { name: '更多操作' }).click();
  await inspector.getByRole('menuitem', { name: '暂停' }).click();
  await expect.poll(async () => (await operations(page)).includes('pauseDelivery')).toBe(true);
  await inspector.getByRole('button', { name: '更多操作' }).click();
  await expect(inspector.getByRole('menuitem', { name: '继续' })).toBeVisible({ timeout: 15_000 });
});

test('设置 · 设备与证书：列表来自 Mock', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '设置', exact: true }).click();
  await page.getByRole('button', { name: /设备与证书/ }).click();
  await expect(page.getByRole('article', { name: 'Susan 的 MacBook' })).toBeVisible();
  await expect(page.getByRole('article', { name: 'Susan 的 iPhone' })).toBeVisible();
});

test('E2E-13：应用关闭期间 Mock 继续产生事件；重新打开后重新水合，界面与 Mock 真值一致', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await expect(page.getByRole('button', { name: '需要你 · 2', exact: true })).toBeVisible();
  await page.goto('about:blank');

  // 关闭期间：agent 回复了一条消息，交付推进到合并阶段，规格缺失的上报项被处理。
  await postServerMessage(page, mockMessage(4, { text: '关闭期间产生的回复' }));
  await mutate(page, 'deliveries', DELIVERY, { stage: 'merge' });
  await emit(page, events.stageChanged('review', 'merge'));
  await mutate(page, 'inbox', ESCALATION_INBOX, { status: 'resolved' });
  await emit(page, events.inboxResolved(ESCALATION_INBOX, 'chosen', { option_id: 'narrow' }));

  await openWorkspace(page);
  await openWeb(page);
  await expect(page.getByRole('main').getByText('关闭期间产生的回复')).toBeVisible();
  await expect(page.getByRole('button', { name: '需要你 · 1', exact: true })).toBeVisible();
  await expect(page.getByRole('navigation', { name: '项目' }).getByLabel('需要你 1 项')).toBeVisible();
  await page.getByRole('complementary', { name: '项目信息' }).getByRole('button', { name: /^PR #12 / }).click();
  await expect(page.getByRole('complementary', { name: '检视器' }).getByText(/合并闸门/).first()).toBeVisible();
});

test('恢复连接后从游标续流，提示“已补齐 n 件事件”', async ({ page }) => {
  await openWorkspace(page);
  await page.evaluate(() => window.__GANTRY_MOCK__?.disconnect());
  await expect(page.getByText('连接中断 · 重连中').first()).toBeVisible();
  await emit(page, events.stageChanged('review', 'merge'));
  await emit(page, events.inboxResolved(SPEC_INBOX, 'answered'));
  await page.evaluate(() => window.__GANTRY_MOCK__?.reconnect());
  await expect(page.getByText('已补齐 2 件事件')).toBeVisible({ timeout: 15_000 });
  expect(await page.evaluate(() => window.__GANTRY_MOCK__?.ackedSeq())).toBe(18242);
});

test('事件写回：发送消息后 Mock 推送 message.posted，界面不重复显示', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await page.getByLabel('给 agent 发消息').fill('只出现一次');
  await page.getByRole('button', { name: '发送', exact: true }).click();
  await expect(page.getByRole('main').getByText('只出现一次')).toHaveCount(1);
  await page.waitForTimeout(500);
  await expect(page.getByRole('main').getByText('只出现一次')).toHaveCount(1);
});
