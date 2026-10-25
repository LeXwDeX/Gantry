import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

async function openWorkspace(page: Page, scenario = 'default') {
  await page.goto(`/?scenario=${scenario}`);
  await expect(page.locator('#composer')).toBeVisible();
}

async function noOverflow(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
}

async function accessible(page: Page) {
  const { violations } = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21aa']).analyze();
  expect(violations.filter(({ impact }) => impact === 'serious' || impact === 'critical')).toEqual([]);
}

test('工作区、地图、工作树和设置保持原型层级与窄屏布局', async ({ page }, info) => {
  await openWorkspace(page);
  await expect(page.getByRole('button', { name: '需要你 · 4', exact: true })).toBeVisible();
  await expect(page.getByText('示例数据', { exact: true })).toBeVisible();
  await noOverflow(page);
  await accessible(page);
  await page.screenshot({ path: info.outputPath('workspace.png'), fullPage: true });

  await page.getByRole('tab', { name: '关系地图', exact: true }).click();
  await expect(page.getByRole('button', { name: '只看需要你', exact: true })).toBeVisible();
  await noOverflow(page);
  await accessible(page);
  await page.screenshot({ path: info.outputPath('map.png'), fullPage: true });

  await page.getByRole('tab', { name: '工作树', exact: true }).click();
  await expect(page.getByRole('columnheader', { name: '写权', exact: true })).toBeVisible();
  await noOverflow(page);
  await accessible(page);
  await page.screenshot({ path: info.outputPath('worktrees.png'), fullPage: true });

  await page.getByRole('button', { name: '设置', exact: true }).click();
  await expect(page.getByRole('heading', { name: '模型', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '测试连接', exact: true })).toHaveCount(0);
  await noOverflow(page);
  await accessible(page);
  await page.screenshot({ path: info.outputPath('settings.png'), fullPage: true });
});

test('选择只影响检视器，项目切换和设置返回保留草稿', async ({ page }) => {
  await openWorkspace(page);
  const composer = page.locator('#composer');
  await composer.fill('这份草稿属于 gantry');
  await page.getByRole('region', { name: '进行中 PR', exact: true }).getByRole('button', { name: /#9/ }).click();
  await expect(page.getByRole('button', { name: /@.*全项目/ })).toBeVisible();

  const projects = page.getByRole('navigation', { name: '项目', exact: true });
  await projects.getByRole('button', { name: /specgit/ }).click();
  await expect(composer).toHaveValue('');
  await composer.fill('这份草稿属于 specgit');
  await projects.getByRole('button', { name: /gantry/ }).click();
  await expect(composer).toHaveValue('这份草稿属于 gantry');

  await page.getByRole('button', { name: '设置', exact: true }).click();
  await page.getByRole('button', { name: '返回工作区', exact: true }).click();
  await expect(composer).toHaveValue('这份草稿属于 gantry');
});

test('只读角色不能发送或进入设置', async ({ page }) => {
  await openWorkspace(page, 'viewer');
  await expect(page.locator('#composer')).toBeDisabled();
  await expect(page.getByRole('button', { name: /^发送/ })).toBeDisabled();
  await expect(page.getByRole('button', { name: '设置', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: '项目设置', exact: true })).toHaveCount(0);
});

test('离线消息留在队列，界面不显示发送成功', async ({ page }) => {
  await openWorkspace(page, 'offline');
  await page.locator('#composer').fill('离线时保留的消息');
  await page.getByRole('button', { name: /稍后发送/ }).click();
  await expect(page.locator('#composer')).toHaveValue('');
  await expect(page.getByText('离线时保留的消息', { exact: true })).toBeVisible();
  await expect(page.getByText('待发送', { exact: true })).toBeVisible();
});

test('收件箱可用键盘关闭，焦点回到入口', async ({ page }, info) => {
  await openWorkspace(page);
  const trigger = page.getByRole('button', { name: '需要你 · 4', exact: true });
  await trigger.click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await noOverflow(page);
  await accessible(page);
  await page.screenshot({ path: info.outputPath('inbox.png'), fullPage: true });
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(trigger).toBeFocused();
});

test('审批展示完整摘要，但开发场景不能签名', async ({ page }, info) => {
  await openWorkspace(page);
  await page.getByRole('button', { name: '需要你 · 4', exact: true }).click();
  await page.getByRole('dialog').getByRole('button', { name: /审阅并签名/ }).first().click();
  const sheet = page.getByRole('dialog');
  await expect(sheet.getByText(/[a-f0-9]{40}/).first()).toBeVisible();
  await expect(sheet.getByText(/未签名信息/)).toBeVisible();
  await expect(sheet.getByRole('button', { name: /用 (Touch ID|Windows Hello) 签名批准/ })).toBeDisabled();
  await noOverflow(page);
  await accessible(page);
  await page.screenshot({ path: info.outputPath('approval.png'), fullPage: true });
});

test('不可信内容不生成元素、不加载远程图片，链接主机可见', async ({ page }) => {
  const externalRequests: string[] = [];
  const dialogs: string[] = [];
  page.on('request', (request) => {
    if (new URL(request.url()).hostname !== '127.0.0.1') externalRequests.push(request.url());
  });
  page.on('dialog', async (dialog) => {
    dialogs.push(dialog.message());
    await dialog.dismiss();
  });
  await openWorkspace(page, 'unsafe');
  await expect(page.getByText(/evil\.example/).first()).toBeVisible();
  await expect(page.locator('iframe, img, [onerror], [onload], [href^="javascript:"]')).toHaveCount(0);
  await noOverflow(page);
  expect(externalRequests).toEqual([]);
  expect(dialogs).toEqual([]);
});

test('正式产物在浏览器中关闭运行，不回退示例', async ({ page }) => {
  await page.goto('http://127.0.0.1:4174/?scenario=default');
  await expect(page.getByText(/外壳.*(不可用|未连接|未接入|未就绪)/).first()).toBeVisible();
  await expect(page.locator('#composer')).toHaveCount(0);
  await expect(page.getByText('示例数据', { exact: true })).toHaveCount(0);
});

test('受保护保存清空凭据且不假报成功，未保存离开受拦截', async ({ page }) => {
  await openWorkspace(page);
  await page.locator('#composer').fill('返回后仍保留');
  await page.getByRole('button', { name: '设置', exact: true }).click();
  const settings = page.getByRole('dialog');
  await expect(settings.getByText('示例数据', { exact: true })).toBeVisible();
  await settings.getByLabel('大脑模型 模型', { exact: true }).selectOption('anthropic/claude-opus-4-1');
  await settings.getByRole('button', { name: '返回工作区', exact: true }).click();
  await expect(page.getByRole('alertdialog')).toBeVisible();
  await page.getByRole('button', { name: '继续编辑', exact: true }).click();
  const secret = settings.getByLabel('anthropic API key', { exact: true });
  await secret.fill('test-only-not-a-real-key');
  await settings.getByRole('button', { name: '保存', exact: true }).click();
  await expect(secret).toHaveValue('');
  await expect(settings.getByRole('button', { name: '确认提交', exact: true })).toBeDisabled();
  await expect(settings.getByText(/已保存（rev/)).toHaveCount(0);
  await settings.getByRole('button', { name: '返回工作区', exact: true }).click();
  await page.getByRole('button', { name: '放弃改动并返回', exact: true }).click();
  await expect(page.locator('#composer')).toHaveValue('返回后仍保留');
});

for (const phase of ['revoked', 'cert_expired'] as const) {
  test(`运行中 ${phase} 立即阻塞工作区`, async ({ page }) => {
    await openWorkspace(page);
    await page.evaluate((next) => window.__GANTRY_SCENARIO__!.setConnection(next), phase);
    await expect(page.locator('#composer')).toHaveCount(0);
    await expect(page.getByText(phase === 'revoked' ? '此设备已被吊销，需要重新配对。' : '证书已过期，需要重新配对。', { exact: true })).toBeVisible();
    await page.evaluate(() => window.__GANTRY_SCENARIO__!.setConnection('live'));
    await expect(page.locator('#composer')).toHaveCount(0);
  });
}
