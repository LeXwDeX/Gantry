import { IpcError } from "@/api/shell";

/** 把外壳或传输层抛出的未知错误统一成可判别错误。 */
export function fromUnknownError(e: unknown): IpcError {
  if (e instanceof IpcError) return e;
  if (typeof e === "string") {
    return new IpcError(guessCode(e), e);
  }
  if (e && typeof e === "object") {
    const record = e as Record<string, unknown>;
    const code = typeof record.code === "string" ? record.code : null;
    const message =
      typeof record.message === "string" ? record.message : "外壳调用失败";
    if (code) return new IpcError(code, message);
    return new IpcError("shell.bad_argument", message);
  }
  return new IpcError("shell.bad_argument", "外壳调用失败");
}

function guessCode(message: string): string {
  const match = /shell\.[a-z_]+/.exec(message);
  return match ? match[0] : "shell.bad_argument";
}
