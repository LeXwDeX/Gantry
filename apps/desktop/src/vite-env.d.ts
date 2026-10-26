/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly MODE: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

interface Window {
  /** Tauri 2 注入的运行时标记；只有存在时才尝试使用原生外壳。 */
  readonly __TAURI_INTERNALS__?: unknown;
}
