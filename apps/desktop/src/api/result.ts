/**
 * `ApiResult` 的判别与错误分类辅助。来源：docs/interfaces/api.md §3.1、
 * docs/modules/desktop-ui.md §9。
 */
import type { ApiResult } from "./shell";

export type FailureKind = "network" | "http";

export function isOk<T>(
  r: ApiResult<T>,
): r is Extract<ApiResult<T>, { ok: true }> {
  return r.ok;
}

export function isNetworkFailure(
  r: ApiResult<unknown>,
): r is Extract<ApiResult<unknown>, { network: string }> {
  return !r.ok && "network" in r;
}

export function problemCode(r: ApiResult<unknown>): string | null {
  return !r.ok && "problem" in r ? r.problem.code : null;
}

/** 写请求遇到这些结果时保持队列项并用同一幂等键退避重试。 */
export function isRetryable(r: ApiResult<unknown>): boolean {
  if (!r.ok && "network" in r) return true;
  if (r.ok) return false;
  const { status, problem } = r;
  if (status >= 500) return true;
  if (status === 429) return true;
  return problem.code === "idempotency_in_progress";
}

/** 这些错误把消息保留为“失败”，供用户编辑后重发或删除。 */
export function isPermanentSendFailure(r: ApiResult<unknown>): boolean {
  if (r.ok || "network" in r) return false;
  return r.status === 403 || r.status === 413 || r.status === 422;
}

export function xEventSeq(r: ApiResult<unknown>): number | null {
  return r.ok && typeof r.headers.x_event_seq === "number"
    ? r.headers.x_event_seq
    : null;
}

export function apiBody<T>(r: ApiResult<T>): T | null {
  return r.ok ? r.body : null;
}
