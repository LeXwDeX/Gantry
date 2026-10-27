import path from "node:path";
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const root = path.dirname(fileURLToPath(import.meta.url));

// mock 模式：浏览器同源访问 /v1，由 Vite 转发到 tools/mock-api（Mock 不开 CORS）。
const mockTarget = `http://127.0.0.1:${process.env.MOCK_PORT ?? "8787"}`;
const mockProxy = { "/v1": { target: mockTarget, changeOrigin: false } };

export default defineConfig(({ mode }) => ({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(root, "src"),
    },
  },
  server: mode === "mock" ? { proxy: mockProxy } : undefined,
  preview: mode === "mock" ? { proxy: mockProxy } : undefined,
  build: {
    outDir: mode === "scenario" ? "dist-scenario" : mode === "mock" ? "dist-mock" : "dist",
    emptyOutDir: true,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./tests/unit/setup.ts"],
    globalSetup: ["./tests/unit/globalSetup.ts"],
    include: ["tests/unit/**/*.test.{ts,tsx}"],
    css: false,
  },
}));
