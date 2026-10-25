import type { ReactNode } from "react";
import type { Level } from "@/api/types";
import type { Tone } from "@/theme/tone";

export function ToneDot({ tone }: { tone: Tone }): ReactNode {
  return <span className={`tone-dot tone-${tone}`} aria-hidden="true" />;
}

export function NeedPill({
  kind,
  label,
}: {
  kind?: string;
  label?: string;
}): ReactNode {
  const text = label ?? (kind ? `! 需要你 · ${kind}` : "! 需要你");
  return <span className="ws-need-pill">{text}</span>;
}

export function LevelBadge({ level }: { level: Level }): ReactNode {
  return (
    <span className="ws-level-badge" title="档位上限">
      {level}
    </span>
  );
}

export function GantryMark(): ReactNode {
  return (
    <svg
      width="18"
      height="18"
      viewBox="0 0 18 18"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      aria-hidden="true"
    >
      <path d="M2 16V3h14v13" />
      <path d="M2 6h14" />
      <path d="M9 6v5" />
      <rect x="6.5" y="11" width="5" height="3" rx="0.5" />
    </svg>
  );
}

export function GearIcon(): ReactNode {
  return (
    <svg
      width="15"
      height="15"
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      aria-hidden="true"
    >
      <circle cx="10" cy="10" r="2.5" />
      <path d="M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M4.7 15.3l1.4-1.4M13.9 6.1l1.4-1.4" />
    </svg>
  );
}

export function Chevron({ open }: { open: boolean }): ReactNode {
  return (
    <span className="ws-chevron" aria-hidden="true">
      {open ? "▾" : "▸"}
    </span>
  );
}
