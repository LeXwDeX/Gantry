import type { ReactNode } from "react";

/** 仅用于未知设置页；已实现的设置页不再走占位组件。 */
export type PlaceholderKey = "unknown";

const CONTENT: Record<
  PlaceholderKey,
  { title: string; basis: string }
> = {
  unknown: {
    title: "设置",
    basis: "该页尚未提供。已知设置页见左侧导航；配置键见 docs/interfaces/config.md。",
  },
};

export function SettingsPlaceholder({ page }: { page: PlaceholderKey }): ReactNode {
  const content = CONTENT[page];
  return (
    <section className="ov-settings__page">
      <div className="ov-settings__lead">
        <h1 className="ov-settings__h1">{content.title}</h1>
        <p className="ov-placeholder" role="status">
          尚未提供。{content.basis}
        </p>
      </div>
    </section>
  );
}
