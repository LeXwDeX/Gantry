import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

async function openWorkspace(page: Page, scenario = 'default') {
  await page.goto(`/?scenario=${scenario}`);
  await expect(page.locator('#composer')).toBeVisible();
}

async function openTools(page: Page) {
  const tools = page.getByRole('region', { name: '开发场景工具' });
  const toggle = tools.getByRole('button', { name: /开发场景/ });
  if ((await toggle.getAttribute('aria-expanded')) !== 'true') {
    await toggle.click();
  }
  return tools;
}

async function sendWithScript(page: Page, script: string, text: string) {
  const tools = await openTools(page);
  await page.locator('#cs-script').selectOption(script);
  await page.locator('#composer').fill(text);
  await page.getByRole('button', { name: '发送', exact: true }).click();
  return tools;
}

async function openGraph(page: Page) {
  const trigger = page
    .locator('.ws-card')
    .filter({ hasText: '模拟计划：' })
    .last()
    .getByRole('button', { name: '查看执行图' });
  await expect(trigger).toBeVisible();
  await trigger.click();
  const graph = page.getByRole('dialog', { name: '执行图' });
  await expect(graph).toBeVisible();
  return { graph, trigger };
}

async function noOverflow(page: Page) {
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
}

async function accessible(page: Page) {
  const { violations } = await new AxeBuilder({ page })
    .withTags(['wcag2a', 'wcag2aa', 'wcag21aa'])
    .analyze();
  expect(
    violations.filter(({ impact }) => impact === 'serious' || impact === 'critical'),
  ).toEqual([]);
}

test('发送多节点任务后自动完成并通过执行图查看尝试与预算', async ({ page }) => {
  await openWorkspace(page);
  await sendWithScript(page, 'task', '请运行完整演示任务');

  await expect(
    page.getByText('任务回执：已建立一个模拟交付与计划。'),
  ).toBeVisible();
  await expect(page.getByText('模拟执行摘要').first()).toBeVisible();
  await expect(page.getByText('运行仓库测试')).toBeVisible();
  await expect(page.locator('.cs-tool[data-status="succeeded"]')).toHaveCount(2);
  await expect(
    page.getByText('模拟任务完成：计划、工具和验证均已成功。'),
  ).toBeVisible({ timeout: 15000 });

  const { graph, trigger } = await openGraph(page);
  await expect(graph.getByText(/修订 \d+/)).toBeVisible();
  await expect(
    graph.getByRole('group', { name: '执行图节点，可滚动' }).locator('[data-node-id]'),
  ).toHaveCount(3);
  await graph.getByRole('button', { name: /实现：完成/ }).click();
  await expect(graph.getByRole('region', { name: '尝试' })).toContainText(
    '尝试 #1',
  );
  await expect(graph.getByRole('region', { name: '只读预算' })).toContainText(
    'tokens',
  );
  // This test runs in both configured viewports and covers completed-state graph text.
  await accessible(page);
  await noOverflow(page);

  await graph.getByRole('button', { name: '← 返回' }).click();
  await expect(page.getByRole('dialog', { name: '执行图' })).toHaveCount(0);
  await expect(trigger).toBeFocused();
});

test('失败节点经执行图重试产生新尝试并完成', async ({ page }) => {
  await openWorkspace(page);
  await sendWithScript(page, 'failure_retry', '请演示失败与重试');
  await expect(page.getByText('模拟构建失败：工具返回非零退出码。可以重试该节点。')).toBeVisible({
    timeout: 15000,
  });

  const { graph } = await openGraph(page);
  await graph.getByRole('button', { name: /实现：失败/ }).click();
  await expect(graph.getByRole('region', { name: '尝试' })).toContainText(
    '尝试 #1',
  );
  await graph.getByRole('button', { name: '重试失败节点' }).click();
  await expect(
    page.getByText(
      '重试成功：新尝试已完成，旧失败尝试保持不变。模拟执行摘要已结算。',
    ),
  ).toBeVisible({ timeout: 15000 });
  await expect(graph.getByRole('region', { name: '尝试' })).toContainText(
    '尝试 #2',
  );
  const tools = graph.getByRole('region', { name: '模拟工具' });
  await expect(tools.locator('.eg-activity__item')).toHaveCount(4);
  await expect(
    tools.locator('.eg-activity__item').filter({ hasText: '运行仓库测试' }).filter({
      hasText: '失败',
    }),
  ).toHaveCount(1);
  await expect(
    tools.locator('.eg-activity__item').filter({ hasText: '运行仓库测试' }).filter({
      hasText: '完成',
    }),
  ).toHaveCount(1);
});

test('模拟批准只改内存，生产签名保持禁用且无凭据泄露', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '需要你 · 4', exact: true }).click();
  const inbox = page.getByRole('dialog', { name: '需要你处理的事项' });
  await inbox.getByRole('button', { name: '审阅审批…' }).first().click();
  const sheet = page.getByRole('dialog', { name: '审批单' });

  await expect(
    sheet.getByRole('button', { name: /签名批准/ }),
  ).toBeDisabled();
  await expect(sheet.getByText(/生产签名路径在示例模式始终拒绝/)).toBeVisible();
  await sheet.getByRole('button', { name: '模拟批准（不生成签名）' }).click();
  await expect(sheet.getByText(/模拟批准已应用：未生成 signature/)).toBeVisible();
  await expect(
    page.getByText(/模拟批准：未生成 signature，未调用真实签名或 GitHub/),
  ).toBeVisible();
  expect(await page.locator('body').textContent()).not.toContain(
    'test-only-not-a-real-key',
  );
});


test('选择后进入模拟审批，head 变化使审批失效并清理收件箱', async ({ page }) => {
  await openWorkspace(page);
  await sendWithScript(page, 'choice_approval', '请演示选择与审批失效');
  await expect(
    page.getByText('这项演示需要你先做选择；选择后可以模拟批准或拒绝，模拟批准不会生成 signature。'),
  ).toBeVisible({ timeout: 15000 });
  await page.getByRole('button', { name: '继续并模拟审批' }).first().click();
  await expect(
    page.getByText('选择已记录。现在可以模拟批准或拒绝；模拟批准不会生成 signature。'),
  ).toBeVisible();

  await page.getByRole('button', { name: /需要你 · \d+/ }).click();
  const inbox = page.getByRole('dialog', { name: '需要你处理的事项' });
  const item = inbox
    .locator('.ov-inbox__item')
    .filter({ hasText: '模拟审批：确认已选择的演示任务可以进入完成阶段。' });
  await expect(item).toBeVisible();
  await item.getByRole('button', { name: '审阅审批…' }).click();
  const sheet = page.getByRole('dialog', { name: '审批单' });
  await expect(
    sheet.getByRole('button', { name: '模拟批准（不生成签名）' }),
  ).toBeEnabled();
  await sheet.getByRole('button', { name: '关闭' }).click();
  await expect(page.getByRole('dialog')).toHaveCount(0);

  const tools = await openTools(page);
  await tools.getByRole('button', { name: '模拟 head 变化' }).click();
  await expect(page.getByText('PR head 已变化，审批已失效；本次演示任务停止。')).toBeVisible();
  await expect(page.getByText('PR head 已变化，审批失效。')).toBeVisible();

  await page.getByRole('button', { name: /需要你 · \d+/ }).click();
  const reopened = page.getByRole('dialog', { name: '需要你处理的事项' });
  await expect(
    reopened.locator('.ov-inbox__item').filter({
      hasText: '模拟审批：确认已选择的演示任务可以进入完成阶段。',
    }),
  ).toHaveCount(0);
});

test('选择停止演示后不进入审批，收件箱回到原计数', async ({ page }) => {
  await openWorkspace(page);
  await sendWithScript(page, 'choice_approval', '请停止这次演示');
  await page.getByRole('button', { name: '停止演示', exact: true }).first().click();
  await expect(
    page.getByText('已选择停止演示：本次模拟任务已取消，不会进入审批；未生成签名，也未调用真实 GitHub。'),
  ).toBeVisible();
  await expect(page.getByText('选择已记录。现在可以模拟批准或拒绝；模拟批准不会生成 signature。')).toHaveCount(0);
  await page.getByRole('button', { name: '需要你 · 4', exact: true }).click();
  const inbox = page.getByRole('dialog', { name: '需要你处理的事项' });
  await expect(
    inbox.locator('.ov-inbox__item').filter({
      hasText: '模拟审批：确认已选择的演示任务可以进入完成阶段。',
    }),
  ).toHaveCount(0);
  await accessible(page);
  await noOverflow(page);
});

test('模拟拒绝关闭交付且不产生签名', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '需要你 · 4', exact: true }).click();
  const inbox = page.getByRole('dialog', { name: '需要你处理的事项' });
  await inbox.getByRole('button', { name: '审阅审批…' }).first().click();
  const sheet = page.getByRole('dialog', { name: '审批单' });
  await sheet.getByRole('button', { name: '模拟拒绝' }).click();
  await expect(sheet.getByText(/模拟拒绝已应用：未生成 signature/)).toBeVisible();
  await expect(page.getByText(/审批被拒绝；本次演示任务已停止/)).toBeVisible();
  await expect(page.getByRole('button', { name: '需要你 · 3', exact: true })).toBeVisible();
});

for (const title of [
  '档位与预算',
  'GitHub App',
  '沙盒与出网',
  '危险动作策略',
  '后台守护',
  '通知',
  '外观',
]) {
  test(`设置菜单 ${title} 正常打开并加载`, async ({ page }) => {
    await openWorkspace(page);
    await page.getByRole('button', { name: '设置', exact: true }).click();
    const settings = page.getByRole('dialog', { name: '设置' });
    const nav = settings.getByRole('navigation', { name: '设置分类' });
    await nav.getByRole('button', { name: title, exact: true }).click();
    await expect(
      settings.getByRole('heading', { name: title, exact: true }).first(),
    ).toBeVisible();
    await noOverflow(page);
  });
}

test('外观错误值阻止保存，修正后模拟应用并改变工作区布局', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '设置', exact: true }).click();
  const settings = page.getByRole('dialog', { name: '设置' });
  const nav = settings.getByRole('navigation', { name: '设置分类' });
  await nav.getByRole('button', { name: '外观', exact: true }).click();
  await settings.getByLabel('左栏宽度').fill('20');
  await expect(settings.getByText(/左栏宽度必须在 180–480 px/).first()).toBeVisible();
  await expect(
    settings.getByRole('button', { name: '预览本机偏好保存' }),
  ).toBeDisabled();
  await settings.getByLabel('左栏宽度').fill('260');
  await settings.getByRole('button', { name: '预览本机偏好保存' }).click();
  await settings.getByRole('button', { name: '模拟应用到本机偏好' }).click();
  await expect(settings.getByText(/布局已应用到工作区/)).toBeVisible();
  expect(
    await page.locator('.ws-root').getAttribute('data-left-width'),
  ).toBe('260');
  await settings.getByRole('button', { name: '返回工作区' }).click();
  await expect(page.locator('.ws-root')).toHaveAttribute(
    'data-left-width',
    '260',
  );
});

test('设置冲突保留草稿，载入最新值后恢复编辑', async ({ page }) => {
  await openWorkspace(page);
  const tools = await openTools(page);
  await tools.getByRole('button', { name: '注入设置冲突' }).click();
  await page.getByRole('button', { name: '设置', exact: true }).click();
  const settings = page.getByRole('dialog', { name: '设置' });
  const nav = settings.getByRole('navigation', { name: '设置分类' });
  await nav.getByRole('button', { name: '档位与预算', exact: true }).click();
  await settings.getByLabel('low nodes').fill('7');
  await settings.getByRole('button', { name: '保存档位与预算' }).click();
  await settings.getByRole('button', { name: '模拟应用设置' }).click();
  await expect(
    settings.getByText(/提交冲突|此页已被其他设备修改/),
  ).toBeVisible();
  await settings.getByRole('button', { name: '载入最新值（丢弃我的改动）' }).click();
  await expect(
    settings.getByText(/提交冲突|此页已被其他设备修改/),
  ).toHaveCount(0);
});

test('项目六类覆盖可切换，档位覆盖可恢复继承并回读生效值', async ({ page }) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '设置', exact: true }).click();
  const settings = page.getByRole('dialog', { name: '设置' });
  const nav = settings.getByRole('navigation', { name: '设置分类' });
  await nav.getByRole('button', { name: 'specgit', exact: true }).click();
  await expect(
    settings.getByRole('heading', { name: /项目 · specgit/ }),
  ).toBeVisible();
  const resources = settings.getByRole('navigation', { name: '项目覆盖资源' });
  for (const name of ['模型', '档位与预算', '危险动作策略', '后台守护', '沙盒', '出网档案']) {
    await resources.getByRole('button', { name: `${name}覆盖` }).click();
    await expect(
      settings.getByRole('navigation', { name: '项目覆盖资源' }),
    ).toBeVisible();
  }
  await resources.getByRole('button', { name: '档位与预算覆盖' }).click();
  const levelRow = settings.getByRole('group', { name: '默认档位上限 覆盖行' });
  await expect(levelRow.getByLabel('默认档位上限 项目覆盖')).toHaveValue('High');
  await levelRow.getByRole('button', { name: '恢复继承' }).click();
  await settings.getByRole('button', { name: '保存档位与预算覆盖' }).click();
  await expect(settings.getByText(/恢复继承（DELETE）/)).toBeVisible();
  await settings.getByRole('button', { name: '模拟应用设置' }).click();
  await expect(settings.getByText(/模拟设置已应用/)).toBeVisible();
  await expect(
    settings.getByRole('note', { name: '默认档位上限 全局值' }),
  ).toContainText('Medium');
});

test('模拟配对失败可恢复，确认后进入工作区', async ({ page }) => {
  await page.goto('/?scenario=unpaired');
  await expect(page.getByRole('main', { name: '配对设备' })).toBeVisible();
  await page.getByLabel('配对串').fill('gantry+pair://local-test');
  await page.getByLabel('设备名').fill('演示 Mac');
  await page.getByRole('button', { name: '模拟下次配对失败' }).click();
  await page.getByRole('button', { name: '开始配对' }).click();
  await expect(page.getByText(/模拟配对预检失败/)).toBeVisible();
  await page.getByLabel('配对串').fill('gantry+pair://local-test-retry');
  await page.getByRole('button', { name: '开始配对' }).click();
  await expect(page.getByText('492 713')).toBeVisible();
  await page.getByRole('button', { name: '一致', exact: true }).click();
  await expect(page.locator('#composer')).toBeVisible();
  await expect(page.getByText('示例数据', { exact: true })).toBeVisible();
});

test('取消生成后无迟到回复，重置清空演示事实', async ({ page }) => {
  await openWorkspace(page);
  const tools = await sendWithScript(page, 'task', '请取消这次演示任务');
  await expect(
    page.getByText('任务回执：已建立一个模拟交付与计划。'),
  ).toBeVisible();
  await tools.getByRole('button', { name: '取消生成' }).click();
  await expect(page.getByText('已中断').first()).toBeVisible();
  await page.waitForTimeout(1500);
  await expect(
    page.getByText('模拟任务完成：计划、工具和验证均已成功。'),
  ).toHaveCount(0);
  await tools.getByRole('button', { name: '重置场景' }).click();
  await expect(
    page.getByText('任务回执：已建立一个模拟交付与计划。'),
  ).toHaveCount(0);
});


test('从检视器暂停、继续并确认放弃同一交付', async ({ page }) => {
  await openWorkspace(page);
  const issues = page.getByRole('region', { name: '进行中 Issue' });
  await issues.getByRole('button', { name: /#5 沙盒驱动/ }).click();
  const inspector = page.getByRole('complementary', { name: '检视器' });
  await expect(inspector.getByText('运行中').first()).toBeVisible();

  await inspector.getByRole('button', { name: '更多操作' }).click();
  await inspector.getByRole('menuitem', { name: '暂停' }).click();
  await expect(inspector.getByText('已暂停').first()).toBeVisible();

  await inspector.getByRole('button', { name: '更多操作' }).click();
  await inspector.getByRole('menuitem', { name: '继续' }).click();
  await expect(inspector.getByText('运行中').first()).toBeVisible();

  await issues.getByRole('button', { name: /#6 桌面端控制台原型/ }).click();
  await inspector.getByRole('button', { name: '更多操作' }).click();
  await inspector.getByRole('menuitem', { name: '放弃交付…' }).click();
  await expect(inspector.getByRole('alertdialog', { name: '确认放弃交付' })).toBeVisible();
  await inspector.getByRole('button', { name: '确认放弃' }).click();
  await expect(inspector.getByText('交付已放弃').first()).toBeVisible();
});


test('跨项目打开全局收件箱审批，模拟批准仍受同一权限门禁', async ({ page }) => {
  await openWorkspace(page);
  const projects = page.getByRole('navigation', { name: '项目' });
  await projects.getByRole('button', { name: /specgit/ }).click();
  await page.getByRole('button', { name: /需要你 · \d+/ }).click();
  const inbox = page.getByRole('dialog', { name: '需要你处理的事项' });
  await inbox.getByRole('button', { name: '审阅审批…' }).first().click();
  const sheet = page.getByRole('dialog', { name: '审批单' });
  await expect(
    sheet.getByRole('button', { name: '模拟批准（不生成签名）' }),
  ).toBeEnabled();
  await expect(
    sheet.getByRole('button', { name: /生产签名批准（示例模式拒绝）/ }),
  ).toBeDisabled();
});

test('只读角色禁用写操作，切回可操作后恢复；断线状态可重连', async ({ page }) => {
  await openWorkspace(page);
  const tools = await openTools(page);
  await tools.getByRole('button', { name: '切换为只读' }).click();
  await expect(page.locator('#composer')).toBeDisabled();
  await tools.getByRole('button', { name: '切换为可操作' }).click();
  await expect(page.locator('#composer')).toBeEnabled();
  await tools.getByRole('button', { name: '模拟断线' }).click();
  await expect(page.getByRole('button', { name: /稍后发送/ })).toBeVisible();
  await tools.getByRole('button', { name: '恢复连接' }).click();
  await expect(page.getByRole('button', { name: '发送', exact: true })).toBeVisible();
});
