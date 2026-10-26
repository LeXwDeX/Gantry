import type { ApiList } from "@/api/types";
import type { ApiResult } from "@/api/shell";

export async function mapLimit<T, R>(
  items: T[],
  limit: number,
  fn: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
  const results: R[] = new Array(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, async () => {
    for (;;) {
      const index = next;
      next += 1;
      if (index >= items.length) return;
      results[index] = await fn(items[index], index);
    }
  });
  await Promise.all(workers);
  return results;
}

export interface PagedResult<T> {
  items: T[];
  pages: Array<ApiResult<ApiList<T>>>;
  error: ApiResult<unknown> | null;
}

/** 逐页读取；失败时返回已收集的数据与失败结果，由调用方决定是否保留旧缓存。 */
export async function fetchAllPages<T>(
  fetchPage: (cursor: string | undefined) => Promise<ApiResult<ApiList<T>>>,
  maxPages = 20,
): Promise<PagedResult<T>> {
  const items: T[] = [];
  const pages: Array<ApiResult<ApiList<T>>> = [];
  let cursor: string | undefined;
  for (let page = 0; page < maxPages; page += 1) {
    const result = await fetchPage(cursor);
    pages.push(result);
    if (!result.ok) return { items, pages, error: result };
    const body = result.body;
    if (!body) return { items, pages, error: null };
    items.push(...body.items);
    if (!body.next_cursor) return { items, pages, error: null };
    cursor = body.next_cursor;
  }
  return { items, pages, error: null };
}
