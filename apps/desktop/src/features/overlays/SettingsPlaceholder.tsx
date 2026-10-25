import type { ReactNode } from "react";

export type PlaceholderKey =
  | "levels"
  | "github"
  | "sandbox"
  | "policies"
  | "guardian"
  | "notify"
  | "appearance"
  | "unknown";

const CONTENT: Record<
  PlaceholderKey,
  { title: string; milestone: string; basis: string }
> = {
  levels: {
    title: "档位与预算",
    milestone: "M2",
    basis:
      "档位、预算模板与预算上限尚未在本里程碑交付；本页不提供伪配置。参阅 docs/interfaces/config.md §4 与 desktop-ui R-63。",
  },
  github: {
    title: "GitHub App",
    milestone: "M2",
    basis:
      "GitHub App 的私钥与 webhook 轮换尚未排期；本页不提供伪操作。参阅 docs/interfaces/config.md §10。",
  },
  sandbox: {
    title: "沙盒与出网",
    milestone: "M2",
    basis:
      "沙盒驱动、镜像与出网分组尚未排期；本页不提供伪操作。参阅 docs/interfaces/config.md §7 与 §8。",
  },
  policies: {
    title: "危险动作策略",
    milestone: "M2",
    basis:
      "动作策略、路径规则与自动加码尚未排期；本页不提供伪操作。参阅 docs/interfaces/config.md §5。",
  },
  guardian: {
    title: "后台守护",
    milestone: "M2",
    basis:
      "后台守护的分流与巡检尚未排期；本页不提供伪操作。参阅 docs/interfaces/config.md §6。",
  },
  notify: {
    title: "通知",
    milestone: "M2",
    basis:
      "通知渠道与路由尚未排期；本机通知开关属于外观页（M3），本页不提供伪操作。参阅 docs/interfaces/config.md §9 与 §13.2。",
  },
  appearance: {
    title: "外观",
    milestone: "M3",
    basis:
      "本机外观偏好（主题、密度、语言、布局与系统通知开关）属于 M3，尚未交付；本页不提供伪设置。参阅 docs/interfaces/config.md §13.2。",
  },
  unknown: {
    title: "设置",
    milestone: "—",
    basis: "该页尚未提供。参阅 docs/interfaces/config.md。",
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
        <p className="ov-settings__meta">
          计划里程碑：{content.milestone}。本版本不渲染不可用的控件。
        </p>
      </div>
    </section>
  );
}
