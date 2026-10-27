import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  workers: 2,
  reporter: [['list'], ['html', { open: 'never' }]],
  use: {
    baseURL: 'http://127.0.0.1:4173',
    channel: 'chrome',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    { name: 'desktop', use: { viewport: { width: 1440, height: 900 } } },
    { name: 'narrow', use: { viewport: { width: 390, height: 844 } } },
  ],
  webServer: [
    {
      command: 'npm run dev -- --host 127.0.0.1 --port 4173 --strictPort',
      url: 'http://127.0.0.1:4173',
      reuseExistingServer: false,
    },
    {
      command: 'npx vite preview --host 127.0.0.1 --port 4174 --strictPort',
      url: 'http://127.0.0.1:4174',
      reuseExistingServer: false,
    },
    // 真实 API 模式：Mock API（8797）+ 场景控制（8798），再由 mock 模式的 Vite（4177）代理 /v1。
    {
      command: 'node scripts/mock-harness.mjs',
      url: 'http://127.0.0.1:8797/v1/me',
      env: { MOCK_PORT: '8797', MOCK_CONTROL_PORT: '8798', MOCK_SSE_HEARTBEAT_MS: '2000' },
      reuseExistingServer: false,
    },
    {
      command: 'npx vite --mode mock --host 127.0.0.1 --port 4177 --strictPort',
      url: 'http://127.0.0.1:4177',
      env: { MOCK_PORT: '8797' },
      reuseExistingServer: false,
    },
  ],
});
