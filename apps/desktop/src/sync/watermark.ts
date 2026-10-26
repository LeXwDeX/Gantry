export function minEventSeq(
  values: Array<number | null | undefined>,
): number | null {
  let min: number | null = null;
  for (const value of values) {
    if (typeof value !== "number" || !Number.isFinite(value)) continue;
    if (min === null || value < min) min = value;
  }
  return min;
}

export function maxEventSeq(...values: Array<number | null | undefined>): number | null {
  let max: number | null = null;
  for (const value of values) {
    if (typeof value !== "number" || !Number.isFinite(value)) continue;
    if (max === null || value > max) max = value;
  }
  return max;
}

/**
 * 后拉的快照不应回退缓存：仅当快照水位严格低于缓存水位时丢弃。
 * 水位只来自快照响应的 X-Event-Seq，不是事件 seq。
 */
export function isSnapshotStale(
  snapshotSeq: number | null,
  asOf: number | null,
): boolean {
  return snapshotSeq !== null && asOf !== null && snapshotSeq < asOf;
}

/** 事件 seq ≤ 快照水位说明快照已包含该事件，可跳过对应资源的刷新。 */
export function isBelowWatermark(
  eventSeq: number,
  asOf: number | null,
): boolean {
  return asOf !== null && eventSeq <= asOf;
}
