import { newUlid } from "./ulid.mjs";

export class HttpProblem extends Error {
  constructor(status, code, detail, options = {}) {
    super(detail || code);
    this.status = status;
    this.code = code;
    this.detail = detail || code;
    this.extensions = options.extensions || {};
    this.headers = options.headers || {};
    this.allow = options.allow;
  }
}

export function problemBody({ status, code, detail, instance, requestId, extensions = {} }) {
  return {
    type: `urn:gantry:problem:${code}`,
    title: code,
    status,
    code,
    detail,
    instance,
    request_id: requestId,
    ...extensions,
  };
}

export function setCommonHeaders(res, context, { eventSeq = true } = {}) {
  const { requestId, state } = context;
  const rate = state.server.rate_limits[context.rateClass] || state.server.rate_limits.read;
  res.setHeader("Cache-Control", "no-store");
  res.setHeader("X-Request-Id", requestId);
  res.setHeader("X-Log-Epoch", String(state.logEpoch));
  if (eventSeq) {
    res.setHeader("X-Event-Seq", String(state.head));
  }
  res.setHeader("RateLimit-Limit", String(rate.limit));
  res.setHeader("RateLimit-Remaining", String(rate.remaining));
  res.setHeader("RateLimit-Reset", String(rate.reset));
}

export function sendJson(res, status, body, context, extraHeaders = {}) {
  setCommonHeaders(res, context);
  for (const [key, value] of Object.entries(extraHeaders)) {
    if (value !== undefined && value !== null) {
      res.setHeader(key, String(value));
    }
  }
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "Content-Type": "application/json; charset=utf-8",
    "Content-Length": Buffer.byteLength(payload),
  });
  res.end(payload);
}

export function sendNoContent(res, status, context, extraHeaders = {}) {
  setCommonHeaders(res, context);
  for (const [key, value] of Object.entries(extraHeaders)) {
    if (value !== undefined && value !== null) {
      res.setHeader(key, String(value));
    }
  }
  res.writeHead(status);
  res.end();
}

export function sendProblem(res, problem, context) {
  setCommonHeaders(res, context, { eventSeq: false });
  if (problem.status === 429 || problem.status === 503) {
    res.setHeader("Retry-After", "1");
  }
  if (problem.allow) {
    res.setHeader("Allow", problem.allow);
  }
  for (const [key, value] of Object.entries(problem.headers || {})) {
    if (value !== undefined && value !== null) {
      res.setHeader(key, String(value));
    }
  }
  const body = problemBody({
    status: problem.status,
    code: problem.code,
    detail: problem.detail,
    instance: context.instance,
    requestId: context.requestId,
    extensions: problem.extensions,
  });
  const payload = JSON.stringify(body);
  res.writeHead(problem.status, {
    "Content-Type": "application/problem+json; charset=utf-8",
    "Content-Length": Buffer.byteLength(payload),
  });
  res.end(payload);
}

export function requestId() {
  return newUlid();
}

export async function readBody(req, limit) {
  const declared = Number(req.headers["content-length"] || 0);
  if (Number.isFinite(declared) && declared > limit) {
    throw new HttpProblem(413, "payload_too_large", "request body exceeds the contract limit");
  }
  let size = 0;
  const chunks = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > limit) {
      throw new HttpProblem(413, "payload_too_large", "request body exceeds the contract limit");
    }
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

export function requireJsonContentType(req, raw) {
  if (!raw) {
    return;
  }
  const contentType = String(req.headers["content-type"] || "");
  if (!/^application\/json(?:\s*;|$)/i.test(contentType)) {
    throw new HttpProblem(415, "unsupported_media_type", "body must be application/json");
  }
}
