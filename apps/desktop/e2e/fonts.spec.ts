import { expect, test } from "@playwright/test";

test("英文字体与中文回退随包提供，不请求远程字体", async ({ page }) => {
  const fontUrls: string[] = [];
  page.on("request", (request) => {
    if (request.resourceType() === "font") fontUrls.push(request.url());
  });
  await page.goto("/?scenario=default");
  await expect(page.locator("#composer")).toBeVisible();
  const loaded = await page.evaluate(async () => {
    const faces = await Promise.all([
      document.fonts.load('400 13px "Inter"', "Gantry"),
      document.fonts.load('400 12px "JetBrains Mono"', "node_123"),
      document.fonts.load('400 13px "Noto Sans SC Variable"', "设置预算执行图中文"),
    ]);
    return {
      counts: faces.map((group) => group.length),
      ready: faces.every((group) => group.every((face) => face.status === "loaded")),
      family: getComputedStyle(document.body).fontFamily,
    };
  });
  expect(loaded.counts.every((count) => count > 0)).toBe(true);
  expect(loaded.ready).toBe(true);
  expect(loaded.family).toContain("Noto Sans SC Variable");
  expect(fontUrls.some((url) => url.includes("noto-sans-sc"))).toBe(true);
  expect(fontUrls.every((url) => new URL(url).origin === new URL(page.url()).origin)).toBe(true);
});
