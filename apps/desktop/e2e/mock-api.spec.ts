/**
 * 真实 API 模式（Mock）的 e2e：浏览器 → MockIpcAdapter → Vite 代理 → tools/mock-api。
 * 场景控制经 scripts/mock-harness.mjs 的控制端口；所有用例共享一个 Mock 实例，
 * 因此串行执行、每例前重置，并只在 desktop 项目里跑（窄屏布局由场景模式覆盖）。
 */
import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

export const MOCK_APP = 'http://127.0.0.1:4177';
export const MOCK_CONTROL = 'http://127.0.0.1:8798';

const WEB = 'prj_01JB7K2M9Q4X8V3N5R6T1W0YZA';
const SELF = 'dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM';
const APPROVAL_INBOX = 'inb_01JB7P2K6R9T3X5N8Q1V4W7YZJ';

test.describe.configure({ mode: 'serial' });
test.setTimeout(60_000);

test.beforeEach(async ({ request }, info) => {
  test.skip(info.project.name !== 'desktop', 'Mock 实例共享，只在 desktop 项目串行运行');
  const reset = await request.post(`${MOCK_CONTROL}/instances/default/reset`);
  expect(reset.ok()).toBe(true);
});

async function control(page: Page, path: string, body: unknown = {}) {
  const response = await page.request.post(`${MOCK_CONTROL}/instances/default${path}`, { data: body });
  expect(response.ok()).toBe(true);
}

async function operations(page: Page): Promise<string[]> {
  const response = await page.request.get(`${MOCK_CONTROL}/instances/default/requests`);
  return ((await response.json()) as Array<{ operationId: string }>).map((r) => r.operationId);
}

const DELIVERY = 'dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB';

/** 在 Mock 侧让 PR #12 的合并闸门全部满足，并注入 forge.synced 提示界面重新拉取。 */
async function satisfyGates(page: Page) {
  const response = await page.request.get(`http://127.0.0.1:8797/v1/deliveries/${DELIVERY}`);
  const delivery = (await response.json()) as {
    pr: Record<string, unknown> & { checks: Record<string, unknown>; synced_at: string };
  };
  const pr = {
    ...delivery.pr,
    checks: { ...delivery.pr.checks, required_ok: true, failed: 0, pending: 0 },
    mergeable_state: 'clean',
    review_state: 'approved',
    // 一次真实的 GitHub 同步会推进 synced_at；界面以较新的同步结果为准。
    synced_at: new Date(Date.parse(delivery.pr.synced_at) + 60_000).toISOString(),
  };
  await control(page, '/mutate', { collection: 'deliveries', id: DELIVERY, patch: { pr } });
  await control(page, '/inject', {
    type: 'forge.synced',
    subject: DELIVERY,
    project_id: WEB,
    data: { delivery_id: DELIVERY },
  });
}

async function openApprovalSheet(page: Page) {
  await page.getByRole('button', { name: '需要你 · 2', exact: true }).click();
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

test('E2E-18：上报项在 Mock 侧解决后，tab 徽标与顶栏计数同时减少', async ({ page }) => {
  await openWorkspace(page);
  await expect(page.getByRole('button', { name: '需要你 · 2', exact: true })).toBeVisible();
  await control(page, '/mutate', { collection: 'inbox', id: APPROVAL_INBOX, patch: { status: 'resolved' } });
  await control(page, '/inject', {
    type: 'inbox.resolved',
    subject: APPROVAL_INBOX,
    project_id: WEB,
    data: { inbox_id: APPROVAL_INBOX, kind: 'approval_needed' },
  });
  await expect(page.getByRole('button', { name: '需要你 · 1', exact: true })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('navigation', { name: '项目' }).getByLabel('需要你 1 项')).toBeVisible();
});

test('E2E-07：GitHub 闸门未满足时签名禁用并列出缺项；Mock 满足闸门后即时启用', async ({ page }) => {
  await openWorkspace(page);
  const sheet = await openApprovalSheet(page);
  const sign = sheet.getByRole('button', { name: '用 Touch ID 签名批准' });
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
  const sign = sheet.getByRole('button', { name: '用 Touch ID 签名批准' });
  await expect(sign).toBeEnabled({ timeout: 15_000 });
  await sign.click();
  await expect.poll(async () => (await operations(page)).includes('grantApproval')).toBe(true);
  // 只按 grant 响应里的 Approval.status 更新（api.md §6.5 规则 5），不做乐观显示。
  await expect(sheet.getByRole('status').filter({ hasText: '签名请求已返回：已签名 · 等待合并' })).toBeVisible();
  await expect(sheet.getByRole('button', { name: '用 Touch ID 签名批准' })).toBeDisabled();
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
  await expect(sheet.getByRole('button', { name: '用 Touch ID 签名批准' })).toBeEnabled({ timeout: 15_000 });
  await page.evaluate(() => window.__GANTRY_MOCK__?.disconnect());
  await expect(sheet.getByText('连接中断，暂不能签名。')).toBeVisible();
  await expect(sheet.getByRole('button', { name: '用 Touch ID 签名批准' })).toBeDisabled();
  expect(await operations(page)).not.toContain('grantApproval');
  await page.evaluate(() => window.__GANTRY_MOCK__?.reconnect());
  await expect(sheet.getByRole('button', { name: '用 Touch ID 签名批准' })).toBeEnabled({ timeout: 15_000 });
});

test('E2E-05：只读设备禁用输入并隐藏设置', async ({ page }) => {
  await control(page, '/mutate', { collection: 'devices', id: SELF, patch: { role: 'viewer' } });
  await openWorkspace(page);
  await expect(page.getByLabel('给 agent 发消息')).toBeDisabled();
  await expect(page.getByRole('button', { name: '设置', exact: true })).toHaveCount(0);
});

test('E2E-14：吊销本机后出现阻塞的配对向导，之后不再发请求', async ({ page }) => {
  await openWorkspace(page);
  await page.evaluate(() => window.__GANTRY_MOCK__?.revoke());
  await expect(page.getByRole('main', { name: '配对设备' })).toBeVisible();
  await expect(page.getByRole('navigation', { name: '项目' })).toHaveCount(0);
  const before = (await operations(page)).length;
  await page.waitForTimeout(500);
  expect((await operations(page)).length).toBe(before);
});

test('配对向导：未配对启动 → 确认码来自 Mock → 一致 → 进入工作区', async ({ page }) => {
  await page.goto(`${MOCK_APP}/?paired=0`);
  await expect(page.getByRole('main', { name: '配对设备' })).toBeVisible();
  await page.getByLabel('配对串').fill('gantry+pair://mock-token');
  await page.getByLabel('设备名').fill('e2e Mac');
  await page.getByRole('button', { name: '开始配对' }).click();
  await expect(page.getByText(/^\d{3} \d{3}$/)).toBeVisible();
  await page.getByRole('button', { name: '一致', exact: true }).click();
  await expect(page.getByRole('navigation', { name: '项目' })).toBeVisible({ timeout: 15_000 });
  expect(await operations(page)).toContain('pairDevice');
});

test('检视器 ⋯ 暂停经 HTTP 调用 pauseDelivery', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await page.getByRole('complementary', { name: '项目信息' }).getByRole('button', { name: /^PR #12 / }).click();
  const inspector = page.getByRole('complementary', { name: '检视器' });
  await inspector.getByRole('button', { name: '更多操作' }).click();
  await inspector.getByRole('menuitem', { name: '暂停' }).click();
  await expect.poll(async () => (await operations(page)).includes('pauseDelivery')).toBe(true);
});

test('设置 · 设备与证书：列表来自 Mock', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '设置', exact: true }).click();
  await page.getByRole('button', { name: /设备与证书/ }).click();
  await expect(page.getByRole('article', { name: 'Susan 的 MacBook' })).toBeVisible();
  await expect(page.getByRole('article', { name: 'Susan 的 iPhone' })).toBeVisible();
});

const APPROVAL = 'apr_01JB7P4M8T2X6N9R3Q5K1V7WZK';
const CONVERSATION = 'cnv_01JB7K5Q1M3X8V2N6R9T4W0YZC';

test('E2E-13：应用关闭期间 Mock 继续产生事件；重新打开后重新水合，界面与 Mock 真值一致', async ({ page }) => {
  await openWorkspace(page);
  await openWeb(page);
  await expect(page.getByRole('button', { name: '需要你 · 2', exact: true })).toBeVisible();
  await page.goto('about:blank');

  // 关闭期间：agent 回复了一条消息，审批上报项被处理。
  const id = 'msg_01JB7PZZZZZZZZZZZZZZZZZZZZ';
  await control(page, '/mutate', {
    collection: 'messages',
    push: {
      id,
      conversation_id: CONVERSATION,
      role: 'agent',
      agent_kind: 'planner',
      tone: null,
      text: '关闭期间产生的回复',
      level_cap: null,
      refs: [],
      cards: [],
      delivery_id: null,
      created_at: '2026-10-24T12:45:00.000Z',
      updated_at: '2026-10-24T12:45:00.000Z',
    },
  });
  await control(page, '/inject', {
    type: 'message.posted',
    subject: id,
    project_id: WEB,
    data: { conversation_id: CONVERSATION, message_id: id },
  });
  await control(page, '/mutate', { collection: 'inbox', id: APPROVAL_INBOX, patch: { status: 'resolved' } });
  await control(page, '/inject', {
    type: 'inbox.resolved',
    subject: APPROVAL_INBOX,
    project_id: WEB,
    data: { inbox_id: APPROVAL_INBOX, kind: 'approval_needed' },
  });

  await openWorkspace(page);
  await openWeb(page);
  await expect(page.getByRole('main').getByText('关闭期间产生的回复')).toBeVisible();
  await expect(page.getByRole('button', { name: '需要你 · 1', exact: true })).toBeVisible();
  await expect(page.getByRole('navigation', { name: '项目' }).getByLabel('需要你 1 项')).toBeVisible();
});

test('恢复连接后从游标续流，提示“已补齐 n 件事件”', async ({ page }) => {
  await openWorkspace(page);
  await page.evaluate(() => window.__GANTRY_MOCK__?.disconnect());
  await expect(page.getByText('连接中断 · 重连中').first()).toBeVisible();
  for (const subject of ['inb_01JB7P3M7S1T4X6N9Q2V5W8YZK', 'inb_01JB7P4N8T2X5V7P9Q3W6X0YZM']) {
    await control(page, '/inject', { type: 'inbox.raised', subject, project_id: WEB, data: { inbox_id: subject } });
  }
  await page.evaluate(() => window.__GANTRY_MOCK__?.reconnect());
  await expect(page.getByText('已补齐 2 件事件')).toBeVisible({ timeout: 15_000 });
  const acked = await page.evaluate(() => window.__GANTRY_MOCK__?.ackedSeq());
  expect(acked).toBe(18242);
});

test('E2E-03：审批单打开时新推送使审批失效，签名禁用并丢弃 prepared 句柄', async ({ page }) => {
  await openWorkspace(page);
  await satisfyGates(page);
  const sheet = await openApprovalSheet(page);
  await expect(sheet.getByRole('button', { name: '用 Touch ID 签名批准' })).toBeEnabled({ timeout: 15_000 });

  const response = await page.request.get(`http://127.0.0.1:8797/v1/deliveries/${DELIVERY}`);
  const delivery = (await response.json()) as { pr: Record<string, unknown> & { synced_at: string } };
  const newHead = 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678';
  await control(page, '/mutate', {
    collection: 'deliveries',
    id: DELIVERY,
    patch: {
      pr: {
        ...delivery.pr,
        head_sha: newHead,
        synced_at: new Date(Date.parse(delivery.pr.synced_at) + 60_000).toISOString(),
      },
    },
  });
  // 新推送：forge 同步带来新 head，随后审批因 head 变化失效。
  await control(page, '/inject', {
    type: 'forge.synced',
    subject: DELIVERY,
    project_id: WEB,
    data: { delivery_id: DELIVERY },
  });
  await control(page, '/mutate', { collection: 'approvals', id: APPROVAL, patch: { status: 'invalidated' } });
  await control(page, '/inject', {
    type: 'approval.invalidated',
    subject: APPROVAL,
    project_id: WEB,
    correlation_id: DELIVERY,
    data: { approval_id: APPROVAL },
  });

  await expect(sheet.getByText(/已失效（9f2c4e7 → a1b2c3d），重新评审中/).first()).toBeVisible({ timeout: 15_000 });
  await expect(sheet.getByRole('button', { name: '用 Touch ID 签名批准' })).toHaveCount(0);
  const calls = await page.evaluate(() => window.__GANTRY_MOCK__?.calls() ?? []);
  expect(calls.some((c) => c.command === 'approval_discard')).toBe(true);
  expect(await operations(page)).not.toContain('grantApproval');
});
