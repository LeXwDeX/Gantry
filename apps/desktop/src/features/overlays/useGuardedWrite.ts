import { useCallback, useEffect, useRef, useState } from "react";
import type {
  ApiResult,
  GuardedWriteArgs,
  GuardedWritePreview,
  IpcAdapter,
} from "@/api/shell";
import { ipcErrorText, parseApiFailure } from "./settingsData";

export type WritePhase = "idle" | "preparing" | "submitting";

export interface GuardedWrite {
  preview: GuardedWritePreview | null;
  phase: WritePhase;
  error: string | null;
  prepare: (args: GuardedWriteArgs) => Promise<GuardedWritePreview | null>;
  submit: () => Promise<ApiResult<unknown> | null>;
  discard: () => void;
  clear: () => void;
}

/**
 * 受保护写入的 UI 侧状态：只持有 pending_id。
 * 请求体（含凭据值）由外壳保存在内存中；这里不落地、不打印。
 */
export function useGuardedWrite(ipc: IpcAdapter): GuardedWrite {
  const pendingRef = useRef<string | null>(null);
  const aliveRef = useRef(true);
  const requestSeq = useRef(0);
  const [preview, setPreview] = useState<GuardedWritePreview | null>(null);
  const [phase, setPhase] = useState<WritePhase>("idle");
  const [error, setError] = useState<string | null>(null);

  const discard = useCallback((): void => {
    requestSeq.current += 1;
    const pendingId = pendingRef.current;
    pendingRef.current = null;
    setPreview(null);
    setPhase((current) => (current === "preparing" ? "idle" : current));
    if (pendingId) {
      void ipc.guardedWriteDiscard({ pendingId }).catch(() => {
        // 丢弃是尽力而为；不把错误内容带到界面。
      });
    }
  }, [ipc]);

  const clear = useCallback((): void => {
    discard();
    setPhase("idle");
    setError(null);
  }, [discard]);

  const prepare = useCallback(
    async (args: GuardedWriteArgs): Promise<GuardedWritePreview | null> => {
      discard();
      setPhase("preparing");
      setError(null);
      const seq = ++requestSeq.current;
      try {
        const result = await ipc.guardedWritePrepare(args);
        if (!aliveRef.current || seq !== requestSeq.current) {
          void ipc
            .guardedWriteDiscard({ pendingId: result.pending_id })
            .catch(() => {
              // 组件已卸载或已有更新的预检；丢弃迟到的请求体。
            });
          return null;
        }
        pendingRef.current = result.pending_id;
        setPreview(result);
        if (!result.valid) setError("预览未通过校验，请先修正标出的字段。");
        return result;
      } catch (failure) {
        if (!aliveRef.current || seq !== requestSeq.current) return null;
        setPreview(null);
        setError(ipcErrorText(failure));
        return null;
      } finally {
        if (aliveRef.current && seq === requestSeq.current) setPhase("idle");
      }
    },
    [discard, ipc],
  );

  const submit = useCallback(async (): Promise<ApiResult<unknown> | null> => {
    const pendingId = pendingRef.current;
    if (!pendingId) {
      setError("没有可提交的预览，请重新保存。");
      return null;
    }
    setPhase("submitting");
    setError(null);
    const seq = requestSeq.current;
    try {
      const result = await ipc.guardedWriteSubmit({ pendingId });
      if (!result.ok) {
        void ipc.guardedWriteDiscard({ pendingId }).catch(() => {
          // 提交已失败；尽力释放外壳内存中的请求体。
        });
      }
      pendingRef.current = null;
      if (!aliveRef.current || seq !== requestSeq.current) return result;
      setPreview(null);
      if (!result.ok) setError(parseApiFailure(result));
      return result;
    } catch (failure) {
      void ipc.guardedWriteDiscard({ pendingId }).catch(() => {
        // 提交抛错；尽力释放外壳内存中的请求体。
      });
      pendingRef.current = null;
      if (!aliveRef.current || seq !== requestSeq.current) return null;
      setPreview(null);
      setError(ipcErrorText(failure));
      return null;
    } finally {
      if (aliveRef.current && seq === requestSeq.current) setPhase("idle");
    }
  }, [ipc]);

  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
      requestSeq.current += 1;
      const pendingId = pendingRef.current;
      pendingRef.current = null;
      if (pendingId) {
        void ipc.guardedWriteDiscard({ pendingId }).catch(() => {
          // 卸载时尽力清理外壳内存中的请求体。
        });
      }
    };
  }, [ipc]);

  return { preview, phase, error, prepare, submit, discard, clear };
}
