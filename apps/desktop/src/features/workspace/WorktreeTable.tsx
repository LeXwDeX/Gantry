import type { ReactNode } from "react";
import type { InboxItem } from "@/api/types";
import type { Selection } from "@/api/shell";
import type { ProjectData } from "@/store/state";
import { cleanText } from "@/render";
import { sameSelection, worktreeRows } from "./model";
import { ToneDot } from "./parts";

export interface WorktreeTableProps {
  data: ProjectData;
  selection: Selection | null;
  needsByDelivery: Map<string, InboxItem[]>;
  onSelect: (selection: Selection) => void;
}

/** 工作树表：五列；沙盒没有数据源，显式写“服务端未提供”。 */
export function WorktreeTable({
  data,
  selection,
  needsByDelivery,
  onSelect,
}: WorktreeTableProps): ReactNode {
  const rows = worktreeRows(data, needsByDelivery);
  if (rows.length === 0) {
    return <p className="ws-empty">这个项目没有工作树。</p>;
  }
  return (
    <div
      className="ws-table-scroll"
      tabIndex={0}
      role="region"
      aria-label="工作树列表"
    >
      <table className="ws-table">
        <caption className="visually-hidden">工作树</caption>
        <thead>
          <tr>
            <th scope="col">工作树</th>
            <th scope="col">关联</th>
            <th scope="col">沙盒</th>
            <th scope="col">写权</th>
            <th scope="col">状态</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const selected = sameSelection(selection, row.selection);
            return (
              <tr key={row.key} className={selected ? "is-selected" : undefined}>
                <td>
                  <button
                    type="button"
                    className="ws-table__name"
                    aria-current={selected ? "true" : undefined}
                    onClick={() => onSelect(row.selection)}
                  >
                    {cleanText(row.worktree.branch, true)}
                  </button>
                </td>
                <td className="mono">{row.related}</td>
                <td className="mono ws-muted">{row.sandbox}</td>
                <td>{row.holder}</td>
                <td>
                  <span className="ws-state">
                    <ToneDot tone={row.status.tone} />
                    {row.status.label}
                  </span>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
