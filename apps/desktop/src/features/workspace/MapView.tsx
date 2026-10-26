import type { ReactNode } from "react";
import type { InboxItem } from "@/api/types";
import type { Selection } from "@/api/shell";
import type { ProjectData } from "@/store/state";
import {
  buildMapBands,
  deliveryForSelection,
  sameSelection,
  type MapBand,
  type MapNodeView,
} from "./model";
import { LevelBadge, ToneDot } from "./parts";

export interface MapViewProps {
  data: ProjectData;
  selection: Selection | null;
  needsOnly: boolean;
  needsByDelivery: Map<string, InboxItem[]>;
  onSelect: (selection: Selection) => void;
  onToggleNeedsOnly: () => void;
  onClearSelection: () => void;
}

/**
 * 关系地图：Issue → PR → 工作树 三列自动纵向分带。
 * 只画同一交付内有契约的边；跨交付 refs 没有数据源，不画。
 * 不使用内联样式：节点位置由 CSS 网格、连线由 SVG path 属性给出。
 */
export function MapView({
  data,
  selection,
  needsOnly,
  needsByDelivery,
  onSelect,
  onToggleNeedsOnly,
  onClearSelection,
}: MapViewProps): ReactNode {
  const bands = buildMapBands(data, needsByDelivery);
  const selectedDelivery = deliveryForSelection(selection, data);
  const selectedId = selectedDelivery?.id ?? null;

  if (bands.length === 0) {
    return (
      <p className="ws-empty">
        这个项目还没有进行中的 Issue、PR 或工作树。去「对话」描述一个任务。
      </p>
    );
  }

  return (
    <div
      className="ws-map"
      onKeyDown={(event) => {
        if (event.key === "Escape" && selection) {
          event.preventDefault();
          onClearSelection();
        }
      }}
    >
      <div className="ws-map__toolbar">
        <button
          type="button"
          className={`ws-filter${needsOnly ? " is-on" : ""}`}
          aria-pressed={needsOnly}
          onClick={onToggleNeedsOnly}
        >
          只看需要你
        </button>
        <div className="ws-map__legend" aria-label="连线图例">
          <span>
            <span className="ws-legend-line ws-legend-line--solid" aria-hidden="true" />
            closes / 分支
          </span>
          <span>
            <span className="ws-legend-line ws-legend-line--dashed" aria-hidden="true" />
            尚未开 PR
          </span>
          <span>
            <span className="ws-legend-line ws-legend-line--orphan" aria-hidden="true" />
            缺规格
          </span>
          <span className="ws-muted">沙盒编号：服务端未提供</span>
        </div>
      </div>

      <div className="ws-map__scroll">
        <div className="ws-map__canvas">
          <div className="ws-map__columns" aria-hidden="true">
            <span>Issue</span>
            <span>Pull Request</span>
            <span>工作树</span>
          </div>
          {bands.map((band) => {
            const dimmed =
              (selectedId !== null && band.delivery.id !== selectedId) ||
              (needsOnly && !band.hasNeed);
            const selected = selectedId !== null && band.delivery.id === selectedId;
            return (
              <Band
                key={band.id}
                band={band}
                dimmed={dimmed}
                selected={selected}
                selection={selection}
                onSelect={onSelect}
              />
            );
          })}
        </div>
      </div>
    </div>
  );
}

function Band({
  band,
  dimmed,
  selected,
  selection,
  onSelect,
}: {
  band: MapBand;
  dimmed: boolean;
  selected: boolean;
  selection: Selection | null;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  // shortcut: 单交付超过 12 个节点时按 12 行高度绘制；M4 分组布局时替换。
  const rows = Math.min(Math.max(band.rows, 1), 12);
  const classes = [
    "ws-map__band",
    `ws-map__band--rows-${rows}`,
    dimmed ? "is-dimmed" : "",
    selected ? "is-selected" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={classes} data-delivery-id={band.delivery.id}>
      <svg
        className="ws-map__edges"
        viewBox={`0 0 800 ${Math.max(rows * 76, 76)}`}
        preserveAspectRatio="none"
        aria-hidden="true"
        focusable="false"
      >
        {band.edges.map((edge) => (
          <path
            key={edge.key}
            d={edge.d}
            className={`ws-map__edge ws-map__edge--${edge.kind}${
              selected && edge.highlighted ? " is-highlighted" : ""
            }`}
          />
        ))}
      </svg>
      <div className="ws-map__col ws-map__col--issue">
        {band.issues.map((node) => (
          <Node
            key={node.key}
            node={node}
            selection={selection}
            onSelect={onSelect}
          />
        ))}
        {band.orphan ? (
          <div className="ws-map__node ws-map__node--orphan">
            <span className="ws-map__node-line">
              <span className="tone-dot tone-warn" aria-hidden="true" />
              <span className="mono">—</span>
              <span className="ws-level-badge">?</span>
              <span className="ws-map__node-state ws-map__node-state--warn">
                缺规格
              </span>
            </span>
            <span className="ws-map__node-title">无关联 Issue</span>
          </div>
        ) : null}
      </div>
      <div className="ws-map__col ws-map__col--pr">
        {band.pr ? (
          <Node node={band.pr} selection={selection} onSelect={onSelect} />
        ) : null}
      </div>
      <div className="ws-map__col ws-map__col--worktree">
        {band.worktree ? (
          <Node node={band.worktree} selection={selection} onSelect={onSelect} />
        ) : null}
      </div>
    </div>
  );
}

function Node({
  node,
  selection,
  onSelect,
}: {
  node: MapNodeView;
  selection: Selection | null;
  onSelect: (selection: Selection) => void;
}): ReactNode {
  if (!node.selection) return null;
  const selected = sameSelection(selection, node.selection);
  return (
    <button
      type="button"
      className={`ws-map__node${selected ? " is-selected" : ""}`}
      aria-current={selected ? "true" : undefined}
      title={`${node.label} ${node.title}`}
      onClick={() => onSelect(node.selection as Selection)}
    >
      <span className="ws-map__node-line">
        <ToneDot tone={node.hasNeed ? "need" : node.status.tone} />
        <span className="mono ws-map__node-label">{node.label}</span>
        <LevelBadge level={node.level} />
        <span
          className={`ws-map__node-state tone-text-${
            node.hasNeed ? "need" : node.status.tone
          }`}
        >
          {node.hasNeed ? "! 需要你" : node.status.label}
        </span>
      </span>
      <span className="ws-map__node-title">{node.title}</span>
    </button>
  );
}
