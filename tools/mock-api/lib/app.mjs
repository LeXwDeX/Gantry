import { createHash, randomBytes } from "node:crypto";
import { entityId, newUlid } from "./ulid.mjs";
import {
  HttpProblem,
  readBody,
  requestId,
  requireJsonContentType,
  sendJson,
  sendNoContent,
  sendProblem,
  setCommonHeaders,
} from "./http.mjs";
import { assertEventData, loadEventSchemas } from "./events.mjs";
import { createApplicationState, loadFixtureState } from "./fixtures.mjs";
import { loadOpenApi } from "./openapi.mjs";
import { validateQuery } from "./query.mjs";
import { createValidator } from "./validator.mjs";

const IDEMPOTENCY_RE = /^[A-Za-z0-9_-]{16,64}$/;
const SEVERITY_RANK = { info: 0, warn: 1, blocker: 2 };
const CHALLENGE_TTL_SECONDS = 120;
// 与 core APPROVAL_SHAPES 保持一致：未列出的 action fail-closed（含 push_branch）。
const APPROVAL_SHAPES = new Map([
  ["merge_pr", { target: "pr", digest: /^[0-9a-f]{40}$/ }],
  ["force_push", { target: "branch", digest: /^[0-9a-f]{40}$/ }],
  ["push_outside_prefix", { target: "branch", digest: /^[0-9a-f]{40}$/ }],
  ["resolve_review_thread_human", { target: "review_thread", digest: /^sha256:[0-9a-f]{64}$/ }],
  ["patch_apply_protected", { target: "patch", digest: /^sha256:[0-9a-f]{64}$/ }],
  ["settings_update", { target: "settings", digest: /^sha256:[0-9a-f]{64}$/ }],
  ["device_role_change", { target: "device", digest: /^sha256:[0-9a-f]{64}$/ }],
  ["device_revoke", { target: "device", digest: /^sha256:[0-9a-f]{64}$/ }],
]);
// POST /v1/approvals 只服务内部动作（api.md §6.6）；Forge 动作只能由 policy 创建。
const CREATE_APPROVAL_ACTIONS = new Set([
  "settings_update",
  "device_role_change",
  "device_revoke",
]);

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

/** RFC 8785 的最小子集：对象键排序，其余交给 JSON.stringify；足够覆盖摘要输入。 */
function jcs(value) {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(jcs).join(",")}]`;
  }
  return `{${Object.keys(value)
    .filter((key) => value[key] !== undefined)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${jcs(value[key])}`)
    .join(",")}}`;
}

function deepClone(value) {
  return value === undefined ? undefined : structuredClone(value);
}

function mergePatch(base, patch) {
  if (!patch || typeof patch !== "object" || Array.isArray(patch)) {
    return deepClone(patch);
  }
  const output = base && typeof base === "object" && !Array.isArray(base) ? deepClone(base) : {};
  for (const [key, value] of Object.entries(patch)) {
    if (value === null) {
      delete output[key];
    } else if (value && typeof value === "object" && !Array.isArray(value)) {
      output[key] = mergePatch(output[key], value);
    } else {
      output[key] = deepClone(value);
    }
  }
  return output;
}

function stripSecretValues(value) {
  if (Array.isArray(value)) {
    return value.map(stripSecretValues);
  }
  if (!value || typeof value !== "object") {
    return value;
  }
  const isSecretRef = typeof value.secret_ref === "string";
  const output = {};
  for (const [key, nested] of Object.entries(value)) {
    if (isSecretRef && key === "value") {
      continue;
    }
    output[key] = stripSecretValues(nested);
  }
  if (isSecretRef) {
    output.present = typeof value.present === "boolean" ? value.present : true;
  }
  return output;
}

function publicDevice(device) {
  if (!device) {
    return device;
  }
  const { approval_pubkey: _ignored, ...rest } = device;
  return deepClone(rest);
}

function deviceById(state, id) {
  return state.devices.items.find((device) => device.id === id) || null;
}

function publicDeviceById(state, id) {
  return publicDevice(deviceById(state, id));
}

function findById(collection, id) {
  return collection.items.find((item) => item.id === id) || null;
}

function nowTimestamp(state) {
  return state.me.server.time;
}

function addSeconds(timestamp, seconds) {
  return new Date(Date.parse(timestamp) + seconds * 1000).toISOString();
}

function currentSettingsEntry(state, scope, projectId, resource) {
  if (scope === "global") {
    return state.globalSettings[resource] || null;
  }
  return state.projectSettings[projectId]?.[resource] || null;
}

function currentRev(state, scope, projectId, resource) {
  return currentSettingsEntry(state, scope, projectId, resource)?.rev ?? 0;
}

function etagFor(rev) {
  return `"${rev}"`;
}

function requireIfMatch(headers, etag) {
  const value = headers["if-match"];
  if (!value) {
    throw new HttpProblem(428, "precondition_required", "If-Match is required for settings writes");
  }
  if (value !== etag) {
    throw new HttpProblem(412, "precondition_failed", "If-Match does not match the current ETag", {
      headers: { ETag: etag },
    });
  }
}

function settingsResponse(state, resource, scope, projectId) {
  if (scope === "global") {
    const entry = currentSettingsEntry(state, "global", null, resource);
    return {
      resource,
      scope: "global",
      rev: entry?.rev ?? 0,
      value: entry?.value ?? null,
      warnings: entry?.warnings ?? [],
    };
  }
  const entry = currentSettingsEntry(state, "project", projectId, resource);
  return {
    resource,
    scope: "project",
    project_id: projectId,
    rev: entry?.rev ?? 0,
    value: entry?.value ?? null,
    warnings: entry?.warnings ?? [],
  };
}

function effectiveSettingsResponse(state, resource, projectId) {
  const globalEntry = currentSettingsEntry(state, "global", null, resource);
  const projectEntry = currentSettingsEntry(state, "project", projectId, resource);
  const globalValue = globalEntry?.value;
  const projectValue = projectEntry?.value;
  const value =
    projectValue === null || projectValue === undefined
      ? deepClone(globalValue ?? {})
      : mergePatch(globalValue ?? {}, projectValue);
  return {
    resource,
    scope: "effective",
    project_id: projectId,
    rev: Math.max(globalEntry?.rev ?? 0, projectEntry?.rev ?? 0),
    value,
    clamped: [],
    warnings: [],
  };
}

function dryRunResponse(signing = null) {
  const body = {
    valid: true,
    errors: [],
    warnings: [],
    relaxes: [],
    required_role: signing ? "approver" : "operator",
    requires_signature: signing !== null,
  };
  if (signing) {
    body.signing = signing;
  }
  return body;
}

/** 请求体里本次写入的凭据（带 secret_ref 且给了 value 的对象），返回其 secret_ref 列表。 */
function secretWrites(value, found = []) {
  if (Array.isArray(value)) {
    for (const item of value) {
      secretWrites(item, found);
    }
  } else if (value && typeof value === "object") {
    if (typeof value.secret_ref === "string" && typeof value.value === "string") {
      found.push(value.secret_ref);
    }
    for (const nested of Object.values(value)) {
      secretWrites(nested, found);
    }
  }
  return found;
}

function decodeCursor(cursor) {
  if (valueIsEmpty(cursor)) {
    return null;
  }
  try {
    const parsed = JSON.parse(Buffer.from(cursor, "base64url").toString("utf8"));
    if (!parsed || typeof parsed.id !== "string" || parsed.id.length === 0) {
      throw new Error("missing id");
    }
    return parsed.id;
  } catch {
    throw new HttpProblem(400, "invalid_cursor", "cursor could not be decoded");
  }
}

function valueIsEmpty(value) {
  return value === undefined || value === null || value === "";
}

function encodeCursor(id) {
  return Buffer.from(JSON.stringify({ id }), "utf8").toString("base64url");
}

function paginate(items, query, defaultOrder = "desc") {
  const limit = query.limit ?? 50;
  const order = query.order ?? defaultOrder;
  const lastId = decodeCursor(query.cursor);
  const sorted = [...items].sort((left, right) => {
    const comparison = left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
    return order === "asc" ? comparison : -comparison;
  });
  const filtered = lastId
    ? sorted.filter((item) => (order === "asc" ? item.id > lastId : item.id < lastId))
    : sorted;
  const page = filtered.slice(0, limit);
  const hasMore = filtered.length > page.length;
  return {
    items: page,
    next_cursor: hasMore ? encodeCursor(page[page.length - 1].id) : null,
  };
}

function isTerminal(delivery) {
  return delivery.stage === "closed" || delivery.stage === "abandoned";
}

function findNode(state, id) {
  for (const workflow of state.workflows.items) {
    const node = workflow.nodes.find((item) => item.id === id);
    if (node) {
      return { workflow, node };
    }
  }
  return null;
}

function listInboxSummary(state) {
  const open = state.inbox.items.filter((item) => item.status === "open");
  const byProject = new Map();
  for (const item of open) {
    const key = item.project_id ?? null;
    const current = byProject.get(key) || { project_id: key, open: 0, max_severity: "info" };
    current.open += 1;
    if (SEVERITY_RANK[item.severity] > SEVERITY_RANK[current.max_severity]) {
      current.max_severity = item.severity;
    }
    byProject.set(key, current);
  }
  return {
    total_open: open.length,
    by_project: [...byProject.values()].sort((left, right) =>
      String(left.project_id).localeCompare(String(right.project_id)),
    ),
  };
}

function settingsEntryOrThrow(state, resource) {
  const entry = state.globalSettings[resource];
  if (!entry) {
    throw new HttpProblem(404, "not_found", `unknown settings resource ${resource}`);
  }
  return entry;
}

function matchesEventType(eventType, filters) {
  if (!filters || filters.length === 0) {
    return true;
  }
  return filters.some((filter) => {
    if (filter.endsWith(".*")) {
      return eventType.startsWith(filter.slice(0, -1));
    }
    return eventType === filter;
  });
}

function matchesProject(event, projectIds) {
  if (!projectIds || projectIds.length === 0) {
    return true;
  }
  if (event.project_id === null || event.project_id === undefined) {
    return true;
  }
  return projectIds.includes(event.project_id);
}

function parseSseCursor(headerValue, queryValue) {
  const value = headerValue === undefined ? queryValue : String(headerValue);
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new HttpProblem(400, "invalid_cursor", "event cursor is outside the supported range");
    }
    return value;
  }
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    throw new HttpProblem(400, "invalid_cursor", "event cursor must be a decimal integer");
  }
  const cursor = Number(value);
  if (!Number.isSafeInteger(cursor)) {
    throw new HttpProblem(400, "invalid_cursor", "event cursor is outside the supported range");
  }
  return cursor;
}

function schemaRefName(schema) {
  if (schema && schema.$ref) {
    const match = /^#\/components\/schemas\/(.+)$/.exec(schema.$ref);
    if (match) {
      return match[1];
    }
  }
  return null;
}

function pathParameterSchema(api, schema) {
  const resolved = schema?.$ref
    ? api.spec.components.schemas[schemaRefName(schema)]
    : schema;
  if (!resolved) {
    return null;
  }
  // Keep enum/const out of path checks so an unknown settings resource still yields 404.
  const { enum: _enum, const: _const, ...rest } = resolved;
  return rest;
}

export async function createMockApplication({ specPath, heartbeatMs = 15000 } = {}) {
  const api = await loadOpenApi(specPath);
  const fixtures = await loadFixtureState();
  const state = createApplicationState(fixtures);
  const validator = createValidator(api.spec);
  const idempotency = new Map();
  const challenges = new Map();
  const pausedStages = new Map();
  const sseConnections = new Set();
  const requestLog = [];
  const eventSchemas = await loadEventSchemas();
  // 场景控制：下一次命中某 operationId 的请求返回指定 problem（/__mock/fail-next）。
  const failNext = new Map();

  function deviceActor() {
    return { kind: "device", id: state.me.device.id };
  }

  function projectOfDelivery(deliveryId) {
    return deliveryId ? findById(state.deliveries, deliveryId)?.project_id ?? null : null;
  }

  /**
   * 写入产生的事件：data 先按 schemas/events 校验，信封按 OpenAPI 校验，
   * 然后追加到日志、推进 head，并实时推给已建立的 SSE 连接（data-model §5）。
   */
  function emitEvent(type, subject, data, options = {}) {
    assertEventData(eventSchemas, type, data);
    const seq = state.head + 1;
    const envelope = {
      id: entityId("evt"),
      seq,
      ts: nowTimestamp(state),
      type,
      subject,
      project_id: options.projectId ?? null,
      actor: options.actor ?? deviceActor(),
      causation_id: null,
      correlation_id: options.correlationId === undefined ? subject : options.correlationId,
      data,
    };
    const check = validator.validateNamed("EventEnvelope", envelope);
    if (!check.ok) {
      throw new Error(`mock event envelope violates OpenAPI: ${JSON.stringify(check.errors)}`);
    }
    state.events.push(envelope);
    state.head = seq;
    for (const connection of sseConnections) {
      connection.push(envelope);
    }
    return envelope;
  }

  function fixtureForName(name) {
    switch (name) {
      case "settings":
        return state.globalSettings;
      case "status":
      case "settings-status":
        return state.settingsStatus;
      case "inbox-summary":
        return state.inboxSummary;
      case "events":
        return state.events;
      case undefined:
        return undefined;
      default:
        return state[name];
    }
  }

  for (const operation of api.operations) {
    const name = operation.operation["x-gantry-fixture"];
    if (name && fixtureForName(name) === undefined) {
      throw new Error(`OpenAPI references unknown fixture ${name}`);
    }
  }

  function contextFor(req, url, route) {
    return {
      requestId: requestId(),
      instance: url.pathname,
      state,
      route,
      operation: route.operation,
      rateClass: route.operation["x-gantry-rate"] || "read",
      query: {},
      body: undefined,
      rawBody: "",
      fixture: fixtureForName(route.operation["x-gantry-fixture"]),
    };
  }

  function sendPlan(context, res, plan) {
    const response = api.getResponse(context.operation, plan.status);
    if (plan.noContent || plan.status === 204 || plan.status === 304) {
      sendNoContent(res, plan.status, context, plan.headers || {});
      return;
    }
    if (response) {
      const content = response.content || {};
      const media = content["application/json; charset=utf-8"] || content["application/json"];
      if (media?.schema) {
        const check = validator.validate(media.schema, plan.body);
        if (!check.ok) {
          throw new Error(
            `mock response violates OpenAPI for ${context.operation.operationId}: ${JSON.stringify(check.errors)}`,
          );
        }
      }
    }
    sendJson(res, plan.status, plan.body, context, plan.headers || {});
  }

  function requireIdempotencyKey(context) {
    const policy = context.operation["x-gantry-idempotency"];
    const value = context.req.headers["idempotency-key"];
    if (value === undefined) {
      if (policy === "required") {
        throw new HttpProblem(400, "idempotency_key_required", "Idempotency-Key is required");
      }
      return null;
    }
    if (typeof value !== "string" || !IDEMPOTENCY_RE.test(value)) {
      throw new HttpProblem(400, "invalid_request", "Idempotency-Key must be 16-64 characters of [A-Za-z0-9_-]");
    }
    return value;
  }

  function checkIdempotency(context, key) {
    if (!key) {
      return null;
    }
    // (device_id, key) scope; fingerprint uses the concrete path, not the route template.
    const scopedKey = `${state.me.device.id}\u0000${key}`;
    const fingerprint = sha256(
      `${context.route.method} ${context.instance} ${context.rawBody}`,
    );
    const existing = idempotency.get(scopedKey);
    if (!existing) {
      return { key: scopedKey, fingerprint };
    }
    if (existing.fingerprint !== fingerprint) {
      throw new HttpProblem(422, "idempotency_key_reuse", "Idempotency-Key was used for different content");
    }
    return { replay: existing };
  }

  function storeIdempotency(context, key, fingerprint, plan) {
    if (!key || !fingerprint || plan.status >= 500) {
      return;
    }
    idempotency.set(key, {
      fingerprint,
      status: plan.status,
      body: deepClone(plan.body),
      headers: { ...(plan.headers || {}) },
      noContent: Boolean(plan.noContent),
    });
  }

  function parseBody(context) {
    if (!context.operation.requestBody) {
      return;
    }
    const media = context.operation.requestBody.content?.["application/json"];
    if (!media?.schema) {
      return;
    }
    let raw = "";
    try {
      raw = context.rawBody;
      if (raw.length === 0) {
        if (schemaRefName(media.schema) === "EmptyRequest") {
          context.body = {};
          return;
        }
        throw new HttpProblem(400, "invalid_request", "request body is required");
      }
      requireJsonContentType(context.req, raw);
      let parsed;
      try {
        parsed = JSON.parse(raw);
      } catch {
        throw new HttpProblem(400, "invalid_request", "request body is not valid JSON");
      }
      const check = validator.validate(media.schema, parsed);
      if (!check.ok) {
        throw new HttpProblem(422, "validation_failed", "request body failed schema validation", {
          extensions: { errors: check.errors },
        });
      }
      context.body = parsed;
    } catch (error) {
      if (error instanceof HttpProblem) {
        throw error;
      }
      throw error;
    }
  }

  async function collectBody(context, req) {
    if (!context.operation.requestBody) {
      return;
    }
    const limit = context.operation.requestBody["x-gantry-body-limit"] || 65536;
    context.rawBody = await readBody(req, limit);
  }

  function handleListProjects(context) {
    return { status: 200, body: paginate(context.fixture.items, context.query) };
  }

  function handleGetProject(context) {
    const project = findById(context.fixture, context.params.project_id);
    if (!project) {
      throw new HttpProblem(404, "not_found", "project not found");
    }
    return { status: 200, body: project };
  }

  function handleListDeliveries(context) {
    let items = context.fixture.items;
    if (context.query.project_id) {
      items = items.filter((item) => item.project_id === context.query.project_id);
    }
    if (context.query.stage) {
      const stages = Array.isArray(context.query.stage) ? context.query.stage : [context.query.stage];
      items = items.filter((item) => stages.includes(item.stage));
    }
    if (context.query.stage_status) {
      items = items.filter((item) => item.stage_status === context.query.stage_status);
    }
    if (context.query.active === true) {
      items = items.filter((item) => !isTerminal(item));
    }
    return { status: 200, body: paginate(items, context.query) };
  }

  function handleGetDelivery(context) {
    const delivery = findById(context.fixture, context.params.delivery_id);
    if (!delivery) {
      throw new HttpProblem(404, "not_found", "delivery not found");
    }
    return { status: 200, body: delivery };
  }

  function handlePatchDelivery(context) {
    const delivery = findById(state.deliveries, context.params.delivery_id);
    if (!delivery) {
      throw new HttpProblem(404, "not_found", "delivery not found");
    }
    if (isTerminal(delivery)) {
      throw new HttpProblem(409, "conflict", "delivery is in a terminal stage", {
        extensions: { reason: "invalid_state" },
      });
    }
    const order = ["None", "Low", "Medium", "High", "Max", "Ultimate"];
    const previousCap = delivery.level_cap;
    delivery.level_cap = context.body.level_cap;
    if (order.indexOf(delivery.level_effective) > order.indexOf(delivery.level_cap)) {
      delivery.level_effective = delivery.level_cap;
    }
    delivery.updated_at = nowTimestamp(state);
    // 只发 kind = cap；level_effective 随之派生为 min，不另发事件（desktop-ui 场景契约同此）。
    if (previousCap !== delivery.level_cap) {
      emitEvent(
        "delivery.level_changed",
        delivery.id,
        { from: previousCap, to: delivery.level_cap, kind: "cap", reason: "user_cap" },
        { projectId: delivery.project_id },
      );
    }
    return { status: 200, body: delivery };
  }

  function handleAbandonDelivery(context) {
    const delivery = findById(state.deliveries, context.params.delivery_id);
    if (!delivery) {
      throw new HttpProblem(404, "not_found", "delivery not found");
    }
    if (delivery.stage === "closed") {
      throw new HttpProblem(409, "conflict", "closed delivery cannot be abandoned", {
        extensions: { reason: "invalid_state" },
      });
    }
    if (delivery.stage !== "abandoned") {
      delivery.stage = "abandoned";
      delivery.stage_status = "done";
      delivery.updated_at = nowTimestamp(state);
      emitEvent(
        "delivery.abandoned",
        delivery.id,
        { reason: context.body?.reason ?? null, by: state.me.device.id },
        { projectId: delivery.project_id },
      );
    }
    return { status: 200, body: delivery };
  }

  function handlePauseDelivery(context) {
    const delivery = findById(state.deliveries, context.params.delivery_id);
    if (!delivery) {
      throw new HttpProblem(404, "not_found", "delivery not found");
    }
    if (isTerminal(delivery)) {
      throw new HttpProblem(409, "conflict", "delivery is in a terminal stage", {
        extensions: { reason: "invalid_state" },
      });
    }
    if (delivery.stage_status !== "paused") {
      const existing = pausedStages.get(delivery.id);
      const snapshot = existing?.status === delivery.stage_status ? existing : null;
      pausedStages.set(delivery.id, {
        status: delivery.stage_status,
        openInbox:
          snapshot?.openInbox ??
          new Set(
            state.inbox.items
              .filter((item) => item.delivery_id === delivery.id && item.status === "open")
              .map((item) => item.id),
          ),
      });
      const statusBefore = delivery.stage_status;
      delivery.stage_status = "paused";
      delivery.updated_at = nowTimestamp(state);
      emitEvent(
        "delivery.paused",
        delivery.id,
        { by: state.me.device.id, status_before: statusBefore },
        { projectId: delivery.project_id },
      );
    }
    return { status: 200, body: delivery };
  }

  function handleResumeDelivery(context) {
    const delivery = findById(state.deliveries, context.params.delivery_id);
    if (!delivery) {
      throw new HttpProblem(404, "not_found", "delivery not found");
    }
    if (isTerminal(delivery)) {
      throw new HttpProblem(409, "conflict", "delivery is in a terminal stage", {
        extensions: { reason: "invalid_state" },
      });
    }
    if (delivery.stage_status === "paused" || pausedStages.has(delivery.id)) {
      const before = pausedStages.get(delivery.id);
      if (before) {
        // Dismissed or removed blockers never clear the pause; every recorded item must resolve.
        const resolved =
          before.openInbox.size > 0 &&
          [...before.openInbox].every((id) => findById(state.inbox, id)?.status === "resolved");
        delivery.stage_status = resolved ? "running" : before.status;
        if (resolved || before.openInbox.size === 0) {
          pausedStages.delete(delivery.id);
        }
        delivery.updated_at = nowTimestamp(state);
        emitEvent(
          "delivery.resumed",
          delivery.id,
          { by: state.me.device.id, status_after: delivery.stage_status },
          { projectId: delivery.project_id },
        );
      }
    }
    return { status: 200, body: delivery };
  }

  function handleGetDeliveryBudget(context) {
    const ledger = context.fixture.items.find(
      (item) => item.delivery_id === context.params.delivery_id,
    );
    if (!ledger) {
      throw new HttpProblem(404, "not_found", "budget ledger not found");
    }
    return { status: 200, body: ledger };
  }

  function handleListConversations(context) {
    const project = findById(state.projects, context.params.project_id);
    if (!project) {
      throw new HttpProblem(404, "not_found", "project not found");
    }
    const items = context.fixture.items.filter((item) => item.project_id === project.id);
    return { status: 200, body: paginate(items, context.query) };
  }

  function handleListMessages(context) {
    const conversation = findById(state.conversations, context.params.conversation_id);
    if (!conversation) {
      throw new HttpProblem(404, "not_found", "conversation not found");
    }
    const items = context.fixture.items.filter(
      (item) => item.conversation_id === conversation.id,
    );
    return { status: 200, body: paginate(items, context.query, "desc") };
  }

  function handlePostMessage(context) {
    const conversation = findById(state.conversations, context.params.conversation_id);
    if (!conversation) {
      throw new HttpProblem(404, "not_found", "conversation not found");
    }
    const timestamp = nowTimestamp(state);
    const message = {
      id: entityId("msg"),
      conversation_id: conversation.id,
      role: "user",
      agent_kind: null,
      tone: null,
      text: context.body.text,
      level_cap: context.body.level_cap ?? null,
      refs: context.body.refs ?? [],
      cards: [],
      delivery_id: null,
      created_at: timestamp,
      updated_at: timestamp,
    };
    state.messages.items.push(message);
    emitEvent("message.posted", message.id, deepClone(message), {
      projectId: conversation.project_id,
      correlationId: conversation.id,
    });
    return { status: 201, body: message };
  }

  function handleListWorktrees(context) {
    let items = context.fixture.items;
    if (context.query.project_id) {
      items = items.filter((item) => item.project_id === context.query.project_id);
    }
    if (context.query.delivery_id) {
      const delivery = findById(state.deliveries, context.query.delivery_id);
      if (delivery?.worktree_id) {
        items = items.filter((item) => item.id === delivery.worktree_id);
      } else {
        items = [];
      }
    }
    return { status: 200, body: paginate(items, context.query) };
  }

  function handleGetWorktree(context) {
    const worktree = findById(context.fixture, context.params.worktree_id);
    if (!worktree) {
      throw new HttpProblem(404, "not_found", "worktree not found");
    }
    return { status: 200, body: worktree };
  }

  function handleAcquireWorktreeLock(context) {
    const worktree = findById(context.fixture, context.params.worktree_id);
    if (!worktree) {
      throw new HttpProblem(404, "not_found", "worktree not found");
    }
    if (worktree.lock && worktree.lock.holder !== "human") {
      throw new HttpProblem(409, "worktree_locked", "worktree is locked by another holder", {
        extensions: { lock: worktree.lock },
      });
    }
    const leaseSeconds = context.body.lease_seconds ?? 1800;
    const timestamp = nowTimestamp(state);
    worktree.lock = {
      holder: "human",
      delivery_id: worktree.lock?.holder === "human" ? worktree.lock.delivery_id : null,
      acquired_at: timestamp,
      lease_until: addSeconds(timestamp, leaseSeconds),
    };
    worktree.updated_at = timestamp;
    emitEvent(
      "worktree.locked",
      worktree.id,
      {
        worktree_id: worktree.id,
        holder: "human",
        delivery_id: null,
        lease_until: worktree.lock.lease_until,
      },
      { projectId: worktree.project_id ?? null },
    );
    return { status: 200, body: worktree };
  }

  function handleReleaseWorktreeLock(context) {
    const worktree = findById(context.fixture, context.params.worktree_id);
    if (!worktree) {
      throw new HttpProblem(404, "not_found", "worktree not found");
    }
    if (worktree.lock && worktree.lock.holder !== "human") {
      throw new HttpProblem(409, "worktree_locked", "worktree is locked by another holder", {
        extensions: { lock: worktree.lock },
      });
    }
    const wasLocked = worktree.lock !== null && worktree.lock !== undefined;
    worktree.lock = null;
    worktree.updated_at = nowTimestamp(state);
    if (wasLocked) {
      emitEvent(
        "worktree.unlocked",
        worktree.id,
        { worktree_id: worktree.id, reason: "released" },
        { projectId: worktree.project_id ?? null },
      );
    }
    return { status: 200, body: worktree };
  }

  function handleListDeliveryWorkflows(context) {
    const delivery = findById(state.deliveries, context.params.delivery_id);
    if (!delivery) {
      throw new HttpProblem(404, "not_found", "delivery not found");
    }
    let items = context.fixture.items.filter((item) => item.delivery_id === delivery.id);
    if (context.query.stage) {
      items = items.filter((item) => item.stage === context.query.stage);
    }
    const page = paginate(items, context.query);
    return {
      status: 200,
      body: {
        ...page,
        items: page.items.map(({ nodes: _nodes, ...summary }) => summary),
      },
    };
  }

  function handleGetWorkflow(context) {
    const workflow = findById(context.fixture, context.params.workflow_id);
    if (!workflow) {
      throw new HttpProblem(404, "not_found", "workflow not found");
    }
    const result = deepClone(workflow);
    if (context.query.include_superseded !== true) {
      result.nodes = result.nodes.filter((node) => node.superseded_by === null);
    }
    return { status: 200, body: result };
  }

  function handleListNodeAttempts(context) {
    const node = findNode(state, context.params.node_id);
    if (!node) {
      throw new HttpProblem(404, "not_found", "node not found");
    }
    const items = context.fixture.items.filter((item) => item.node_id === node.node.id);
    return { status: 200, body: paginate(items, context.query, "asc") };
  }

  function handleGetAttempt(context) {
    const attempt = findById(context.fixture, context.params.attempt_id);
    if (!attempt) {
      throw new HttpProblem(404, "not_found", "attempt not found");
    }
    return { status: 200, body: attempt };
  }

  function handleRetryNode(context) {
    const found = findNode(state, context.params.node_id);
    if (!found) {
      throw new HttpProblem(404, "not_found", "node not found");
    }
    if (found.node.status !== "failed") {
      throw new HttpProblem(409, "conflict", "only failed nodes can be retried", {
        extensions: { reason: "invalid_state" },
      });
    }
    const delivery = found.workflow.delivery_id
      ? findById(state.deliveries, found.workflow.delivery_id)
      : null;
    if (
      found.node.superseded_by !== null ||
      delivery?.stage_status === "paused" ||
      (delivery !== null && isTerminal(delivery))
    ) {
      throw new HttpProblem(409, "conflict", "node cannot be retried in its current state", {
        extensions: { reason: "invalid_state" },
      });
    }
    // The engine creates the new attempt asynchronously; the node keeps its current status.
    emitEvent(
      "node.retry_requested",
      found.node.id,
      { node_id: found.node.id, by: state.me.device.id },
      { projectId: projectOfDelivery(found.workflow.delivery_id), correlationId: found.workflow.delivery_id },
    );
    return { status: 202, body: found.node };
  }

  function handleListInbox(context) {
    let items = context.fixture.items;
    const status = context.query.status ?? "open";
    items = items.filter((item) => item.status === status);
    if (context.query.project_id) {
      items = items.filter((item) => item.project_id === context.query.project_id);
    }
    if (context.query.kind) {
      items = items.filter((item) => item.kind === context.query.kind);
    }
    return { status: 200, body: paginate(items, context.query) };
  }

  function handleGetInboxItem(context) {
    const item = findById(context.fixture, context.params.inbox_id);
    if (!item) {
      throw new HttpProblem(404, "not_found", "inbox item not found");
    }
    return { status: 200, body: item };
  }

  function handleDismissInboxItem(context) {
    const item = findById(state.inbox, context.params.inbox_id);
    if (!item) {
      throw new HttpProblem(404, "not_found", "inbox item not found");
    }
    if (item.status === "resolved") {
      throw new HttpProblem(409, "conflict", "resolved inbox item cannot be dismissed", {
        extensions: { reason: "invalid_state" },
      });
    }
    if (item.status !== "dismissed") {
      item.status = "dismissed";
      item.updated_at = nowTimestamp(state);
      emitEvent(
        "inbox.dismissed",
        item.id,
        { inbox_id: item.id, note: context.body?.note ?? null },
        { projectId: item.project_id ?? null, correlationId: item.delivery_id ?? item.id },
      );
    }
    return { status: 200, body: item };
  }

  function handleChooseInboxOption(context) {
    const item = findById(state.inbox, context.params.inbox_id);
    if (!item) {
      throw new HttpProblem(404, "not_found", "inbox item not found");
    }
    if (item.status !== "open") {
      throw new HttpProblem(409, "conflict", "inbox item is not open", {
        extensions: { reason: "invalid_state" },
      });
    }
    if (item.kind === "approval_needed") {
      throw new HttpProblem(409, "conflict", "approval items must be handled through approvals", {
        extensions: { reason: "invalid_state" },
      });
    }
    const options = Array.isArray(item.payload?.options) ? item.payload.options : [];
    if (!options.some((option) => option.id === context.body.option_id)) {
      throw new HttpProblem(422, "validation_failed", "option_id is not offered by this inbox item", {
        extensions: {
          errors: [
            {
              pointer: "/option_id",
              code: "enum",
              message: "option_id is not in payload.options",
            },
          ],
        },
      });
    }
    item.status = "resolved";
    item.updated_at = nowTimestamp(state);
    emitEvent(
      "inbox.resolved",
      item.id,
      {
        inbox_id: item.id,
        resolution: "chosen",
        option_id: context.body.option_id,
        note: context.body.note ?? null,
      },
      { projectId: item.project_id ?? null, correlationId: item.delivery_id ?? item.id },
    );
    return { status: 200, body: item };
  }

  function handleGetInboxSummary() {
    state.inboxSummary = listInboxSummary(state);
    return { status: 200, body: state.inboxSummary };
  }

  function handleListApprovals(context) {
    let items = context.fixture.items;
    if (context.query.status) {
      items = items.filter((item) => item.status === context.query.status);
    }
    if (context.query.delivery_id) {
      items = items.filter((item) => item.delivery_id === context.query.delivery_id);
    }
    return { status: 200, body: paginate(items, context.query) };
  }

  function handleGetApproval(context) {
    const approval = findById(context.fixture, context.params.approval_id);
    if (!approval) {
      throw new HttpProblem(404, "not_found", "approval not found");
    }
    return { status: 200, body: approval };
  }

  function approvalShapeErrors(body) {
    const errors = [];
    const shape = APPROVAL_SHAPES.get(body.action);
    if (!shape) {
      errors.push({
        pointer: "/action",
        code: "enum",
        message: "action has no defined approval shape",
      });
      return errors;
    }
    const kind = body.target?.kind;
    if (kind !== shape.target) {
      errors.push({
        pointer: "/target/kind",
        code: "enum",
        message: `target.kind must be ${shape.target} for ${body.action}`,
      });
    }
    if (typeof body.subject_digest !== "string" || !shape.digest.test(body.subject_digest)) {
      errors.push({
        pointer: "/subject_digest",
        code: "pattern",
        message: `subject_digest does not match the ${body.action} shape`,
      });
    }
    if (kind === "settings") {
      const projectId = body.target?.project_id;
      if (body.target?.scope === "global" && projectId != null) {
        errors.push({
          pointer: "/target/project_id",
          code: "type",
          message: "global settings target must omit project_id",
        });
      }
      if (body.target?.scope === "project" && typeof projectId !== "string") {
        errors.push({
          pointer: "/target/project_id",
          code: "required",
          message: "project settings target requires project_id",
        });
      }
    }
    return errors;
  }

  function createApprovalShapeErrors(body) {
    if (!CREATE_APPROVAL_ACTIONS.has(body.action)) {
      return [
        {
          pointer: "/action",
          code: "enum",
          message: "action is not available for approval creation",
        },
      ];
    }
    return approvalShapeErrors(body);
  }

  function handleCreateApproval(context) {
    const errors = createApprovalShapeErrors(context.body);
    if (errors.length > 0) {
      throw new HttpProblem(422, "validation_failed", "approval shape validation failed", {
        extensions: { errors },
      });
    }
    const timestamp = nowTimestamp(state);
    const approval = {
      id: entityId("apr", Date.parse(timestamp)),
      delivery_id: null,
      action: context.body.action,
      target: context.body.target,
      subject_digest: context.body.subject_digest,
      device_id: null,
      signature: null,
      status: "pending",
      expires_at: addSeconds(timestamp, 86400),
      consumed_by: null,
      created_at: timestamp,
      updated_at: timestamp,
    };
    state.approvals.items.push(approval);
    const target = approval.target;
    emitEvent(
      "approval.requested",
      approval.id,
      {
        action: approval.action,
        target,
        subject_digest: approval.subject_digest,
        expires_at: approval.expires_at,
        config_rev: target.kind === "settings" ? target.base_rev : 0,
        requested_by: deviceActor(),
      },
      { projectId: target.kind === "settings" ? target.project_id ?? null : null, correlationId: approval.id },
    );
    return { status: 201, body: approval };
  }

  function handleDenyApproval(context) {
    const approval = findById(state.approvals, context.params.approval_id);
    if (!approval) {
      throw new HttpProblem(404, "not_found", "approval not found");
    }
    if (approval.status !== "pending") {
      throw new HttpProblem(409, "approval_not_pending", "approval is no longer pending", {
        extensions: { approval_status: approval.status },
      });
    }
    approval.status = "denied";
    approval.updated_at = nowTimestamp(state);
    emitEvent(
      "approval.denied",
      approval.id,
      { reason: context.body?.reason ?? null },
      { projectId: projectOfDelivery(approval.delivery_id), correlationId: approval.delivery_id ?? approval.id },
    );
    return { status: 200, body: approval };
  }

  function handleGrantApproval(context) {
    const approval = findById(state.approvals, context.params.approval_id);
    if (!approval) {
      throw new HttpProblem(404, "not_found", "approval not found");
    }
    const challenge = challenges.get(
      `${approval.id}\u0000${state.me.device.id}`,
    );
    // 同设备换 key 重发已用 nonce 是状态检查的特例（api.md §6.4）。
    if (challenge?.used && challenge.body.payload.nonce === context.body.nonce) {
      throw challengeInvalid("used");
    }
    if (approval.status !== "pending") {
      throw new HttpProblem(409, "approval_not_pending", "approval is no longer pending", {
        extensions: { approval_status: approval.status },
      });
    }
    if (!challenge) {
      throw challengeInvalid("none");
    }
    if (Date.parse(challenge.body.payload.expires_at) <= Date.parse(nowTimestamp(state))) {
      throw challengeInvalid("expired");
    }
    if (challenge.body.payload.nonce !== context.body.nonce) {
      throw challengeInvalid("mismatch");
    }
    challenge.used = true;
    approval.status = "granted";
    approval.device_id = state.me.device.id;
    approval.signature = context.body.signature;
    approval.updated_at = nowTimestamp(state);
    // Mock 不验签；payload_sha256 只是对待签载荷的确定性摘要，供客户端核对事件形状。
    emitEvent(
      "approval.granted",
      approval.id,
      {
        approval_id: approval.id,
        device_id: state.me.device.id,
        approval_pubkey_fpr: deviceById(state, state.me.device.id)?.approval_pubkey_fpr ?? state.me.device.approval_pubkey_fpr,
        payload_sha256: `sha256:${sha256(jcs(challenge.body.payload))}`,
      },
      { projectId: projectOfDelivery(approval.delivery_id), correlationId: approval.delivery_id ?? approval.id },
    );
    return { status: 200, body: approval };
  }

  function challengeInvalid(reason) {
    return new HttpProblem(409, "challenge_invalid", "approval challenge is not usable", {
      extensions: { reason },
    });
  }

  function handleGetApprovalChallenge(context) {
    const approval = findById(state.approvals, context.params.approval_id);
    if (!approval) {
      throw new HttpProblem(404, "not_found", "approval not found");
    }
    if (approval.status !== "pending") {
      throw new HttpProblem(409, "approval_not_pending", "approval is no longer pending", {
        extensions: { approval_status: approval.status },
      });
    }
    const key = `${approval.id}\u0000${state.me.device.id}`;
    const timestamp = nowTimestamp(state);
    const current = challenges.get(key);
    if (
      current &&
      !current.used &&
      Date.parse(current.body.payload.expires_at) > Date.parse(timestamp)
    ) {
      return { status: 200, body: deepClone(current.body) };
    }
    const expiry = addSeconds(timestamp, CHALLENGE_TTL_SECONDS);
    const challenge = deepClone(context.fixture);
    challenge.approval_id = approval.id;
    challenge.payload.action = approval.action;
    challenge.payload.approval_id = approval.id;
    challenge.payload.delivery_id = approval.delivery_id ?? undefined;
    if (challenge.payload.delivery_id === undefined) {
      delete challenge.payload.delivery_id;
    }
    challenge.payload.device_id = state.me.device.id;
    challenge.payload.expires_at =
      Date.parse(expiry) > Date.parse(approval.expires_at) ? approval.expires_at : expiry;
    challenge.payload.nonce = randomBytes(32).toString("base64url");
    challenge.payload.subject_digest = approval.subject_digest;
    challenge.payload.target = approval.target;
    challenges.set(key, { used: false, body: challenge });
    return { status: 200, body: deepClone(challenge) };
  }

  function handleGetSettings(context) {
    settingsEntryOrThrow(state, context.params.resource);
    const etag = etagFor(currentRev(state, "global", null, context.params.resource));
    if (context.req.headers["if-none-match"] === etag) {
      return { status: 304, noContent: true, headers: { ETag: etag } };
    }
    return {
      status: 200,
      body: settingsResponse(state, context.params.resource, "global"),
      headers: { ETag: etag },
    };
  }

  /** 设置写入的签名对象（config.md §13）：目标绑定 scope / project / resource / base_rev，摘要覆盖整个请求体。 */
  function settingsSigning(scope, projectId, resource, baseRev, body) {
    const target = { kind: "settings", scope, resource, base_rev: baseRev };
    if (scope === "project") {
      target.project_id = projectId;
    }
    return {
      action: "settings_update",
      target,
      subject_digest: `sha256:${sha256(jcs(body))}`,
    };
  }

  /**
   * 写入凭据值必须先由审批设备签名：请求头 Gantry-Approval 指向一个已 granted、
   * 动作 / 目标 / 摘要都与本次请求一致的审批；否则 403 signature_required（附 signing）。
   * 通过后消费该审批，同一审批不能重复使用。
   */
  function requireSettingsApproval(context, signing) {
    const approvalId = context.req.headers["gantry-approval"];
    const approval = typeof approvalId === "string" ? findById(state.approvals, approvalId) : null;
    const matches =
      approval !== null &&
      approval.status === "granted" &&
      approval.action === signing.action &&
      approval.subject_digest === signing.subject_digest &&
      jcs(approval.target) === jcs(signing.target);
    if (!matches) {
      throw new HttpProblem(403, "signature_required", "this settings change requires an approver signature", {
        extensions: { signing },
      });
    }
    approval.status = "consumed";
    approval.consumed_by = `settings:${signing.target.resource}`;
    approval.updated_at = nowTimestamp(state);
    emitEvent(
      "approval.consumed",
      approval.id,
      { approval_id: approval.id, consumed_by: approval.consumed_by, subject_digest: approval.subject_digest },
      { projectId: signing.target.project_id ?? null, correlationId: approval.id },
    );
  }

  /** dry_run 与正式写入共用：凭据写入需要签名，其余按中性改动处理（Mock 不计算 relaxes）。 */
  function writeSettings(context, scope, projectId) {
    const resource = context.params.resource;
    const previousRev = currentRev(state, scope, projectId, resource);
    const secrets = secretWrites(context.body.value);
    const signing =
      secrets.length > 0 ? settingsSigning(scope, projectId, resource, previousRev, context.body) : null;
    const etag = etagFor(previousRev);
    if (context.query.dry_run === true) {
      return { status: 200, body: dryRunResponse(signing), headers: { ETag: etag } };
    }
    if (signing) {
      requireSettingsApproval(context, signing);
    }
    const entry = { rev: previousRev + 1, value: stripSecretValues(context.body.value), warnings: [] };
    if (scope === "global") {
      state.globalSettings[resource] = entry;
    } else {
      state.projectSettings[projectId] ??= {};
      state.projectSettings[projectId][resource] = entry;
    }
    emitSettingsUpdated(scope, projectId, resource, previousRev, entry.value, secrets, previousRev + 1);
    return {
      status: 200,
      body: settingsResponse(state, resource, scope, projectId ?? undefined),
      headers: { ETag: etagFor(previousRev + 1) },
    };
  }

  function emitSettingsUpdated(scope, projectId, resource, baseRev, value, secrets, version) {
    emitEvent(
      "settings.updated",
      "settings",
      {
        scope,
        resource,
        schema_version: 1,
        base_rev: baseRev,
        value: deepClone(value),
        relaxes: [],
        secret_changes: secrets.map((ref) => ({ ref, op: "set", version })),
      },
      { projectId: scope === "project" ? projectId : null, correlationId: null },
    );
  }

  function handlePutSettings(context) {
    settingsEntryOrThrow(state, context.params.resource);
    const etag = etagFor(currentRev(state, "global", null, context.params.resource));
    requireIfMatch(context.req.headers, etag);
    return writeSettings(context, "global", null);
  }

  function requireProject(projectId) {
    const project = findById(state.projects, projectId);
    if (!project) {
      throw new HttpProblem(404, "not_found", "project not found");
    }
    return project;
  }

  function handleGetProjectSettings(context) {
    requireProject(context.params.project_id);
    settingsEntryOrThrow(state, context.params.resource);
    const effective = context.query.view === "effective";
    const etag = etagFor(
      effective
        ? Math.max(
            currentRev(state, "global", null, context.params.resource),
            currentRev(state, "project", context.params.project_id, context.params.resource),
          )
        : currentRev(state, "project", context.params.project_id, context.params.resource),
    );
    if (context.req.headers["if-none-match"] === etag) {
      return { status: 304, noContent: true, headers: { ETag: etag } };
    }
    const body = effective
      ? effectiveSettingsResponse(state, context.params.resource, context.params.project_id)
      : settingsResponse(state, context.params.resource, "project", context.params.project_id);
    return { status: 200, body, headers: { ETag: etag } };
  }

  function assertOverridable(resource) {
    if (resource === "notify" || resource === "github") {
      throw new HttpProblem(422, "not_overridable", `${resource} is not overridable per project`);
    }
  }

  function handlePutProjectSettings(context) {
    requireProject(context.params.project_id);
    settingsEntryOrThrow(state, context.params.resource);
    assertOverridable(context.params.resource);
    const etag = etagFor(
      currentRev(state, "project", context.params.project_id, context.params.resource),
    );
    requireIfMatch(context.req.headers, etag);
    return writeSettings(context, "project", context.params.project_id);
  }

  function handleDeleteProjectSettings(context) {
    requireProject(context.params.project_id);
    settingsEntryOrThrow(state, context.params.resource);
    assertOverridable(context.params.resource);
    const etag = etagFor(
      currentRev(state, "project", context.params.project_id, context.params.resource),
    );
    requireIfMatch(context.req.headers, etag);
    const previousRev = currentRev(
      state,
      "project",
      context.params.project_id,
      context.params.resource,
    );
    state.projectSettings[context.params.project_id] ??= {};
    state.projectSettings[context.params.project_id][context.params.resource] = {
      rev: previousRev + 1,
      value: null,
      warnings: [],
    };
    emitSettingsUpdated(
      "project",
      context.params.project_id,
      context.params.resource,
      previousRev,
      null,
      [],
      previousRev + 1,
    );
    return {
      status: 200,
      body: settingsResponse(
        state,
        context.params.resource,
        "project",
        context.params.project_id,
      ),
      headers: { ETag: etagFor(previousRev + 1) },
    };
  }

  function handleGetSettingsCatalog(context) {
    return { status: 200, body: context.fixture };
  }

  function handleGetSettingsStatus(context) {
    return { status: 200, body: context.fixture };
  }

  function handleListDevices(context) {
    return {
      status: 200,
      body: paginate(context.fixture.items.map(publicDevice), context.query),
    };
  }

  function handleRenameDevice(context) {
    const device = deviceById(state, context.params.device_id);
    if (!device) {
      throw new HttpProblem(404, "not_found", "device not found");
    }
    const previousName = device.name;
    device.name = context.body.name;
    device.updated_at = nowTimestamp(state);
    if (previousName !== device.name) {
      emitEvent("device.updated", device.id, { device_id: device.id, changed: { name: device.name } });
    }
    return { status: 200, body: publicDevice(device) };
  }

  function handleSetDeviceRole(context) {
    const device = deviceById(state, context.params.device_id);
    if (!device) {
      throw new HttpProblem(404, "not_found", "device not found");
    }
    if (device.id === state.me.device.id) {
      throw new HttpProblem(409, "conflict", "a device cannot change its own role", {
        extensions: { reason: "self_role_change" },
      });
    }
    if (device.platform === "ios" && context.body.role === "approver") {
      throw new HttpProblem(409, "conflict", "mobile devices cannot become approvers", {
        extensions: { reason: "mobile_cannot_approve" },
      });
    }
    const previousRole = device.role;
    device.role = context.body.role;
    device.updated_at = nowTimestamp(state);
    if (previousRole !== device.role) {
      emitEvent("device.updated", device.id, {
        device_id: device.id,
        changed: { role: { from: previousRole, to: device.role } },
      });
    }
    return { status: 200, body: publicDevice(device) };
  }

  function handleRevokeDevice(context) {
    const device = deviceById(state, context.params.device_id);
    if (!device) {
      throw new HttpProblem(404, "not_found", "device not found");
    }
    if (device.status !== "revoked") {
      device.status = "revoked";
      device.updated_at = nowTimestamp(state);
      emitEvent("device.revoked", device.id, {
        device_id: device.id,
        cert_serials: [device.cert_serial],
        remaining_approvers: state.devices.items.filter(
          (entry) => entry.role === "approver" && entry.status === "active",
        ).length,
      });
      // 吊销本机：已建立的事件流立即断开（desktop-shell §12 要求在心跳间隔内断开）。
      if (device.id === state.me.device.id) {
        setImmediate(() => endStreams());
      }
    }
    state.pushTokens.delete(device.id);
    return { status: 200, body: publicDevice(device) };
  }

  function handleRenewDeviceCertificate() {
    return {
      status: 200,
      body: {
        certificate_chain: deepClone(state.server.certificate_chain),
        not_after: state.me.certificate.not_after,
      },
    };
  }

  function handlePutPushToken(context) {
    state.pushTokens.set(state.me.device.id, context.body);
    return { status: 204, noContent: true };
  }

  function handleGetMe(context) {
    const device = publicDeviceById(state, context.fixture.device.id) ?? publicDevice(context.fixture.device);
    return {
      status: 200,
      body: {
        ...deepClone(context.fixture),
        device,
        server: { ...deepClone(context.fixture.server), time: context.fixture.server.time, log_epoch: state.logEpoch },
      },
    };
  }

  function handlePairDevice(context) {
    const platform = context.body.device.platform;
    if ((platform === "windows" || platform === "macos") && !context.body.approval_pubkey) {
      throw new HttpProblem(422, "validation_failed", "desktop pairing requires approval_pubkey", {
        extensions: {
          errors: [
            { pointer: "/approval_pubkey", code: "required", message: "approval_pubkey is required for desktop devices" },
          ],
        },
      });
    }
    return {
      status: 201,
      body: {
        ...deepClone(context.fixture),
        device: publicDevice(context.fixture.device),
      },
    };
  }

  async function handleSse(req, res, context) {
    const cursor = parseSseCursor(req.headers["last-event-id"], context.query.after);
    if (cursor !== null && cursor > state.head) {
      throw new HttpProblem(409, "cursor_ahead", "Last-Event-ID is greater than the current head");
    }
    const projectIds = context.query.project_id === undefined
      ? null
      : Array.isArray(context.query.project_id)
        ? context.query.project_id
        : [context.query.project_id];
    const types = context.query.types
      ? context.query.types.split(",").map((value) => value.trim()).filter(Boolean)
      : null;

    setCommonHeaders(res, context);
    res.writeHead(200, {
      "Content-Type": "text/event-stream; charset=utf-8",
      "Cache-Control": "no-store",
      "X-Accel-Buffering": "no",
    });
    res.write("retry: 3000\n\n");
    res.flushHeaders?.();

    let closed = false;
    let timer = null;
    const heartbeat = () => {
      if (closed || res.writableEnded) {
        return;
      }
      const payload = {
        seq: state.head,
        ts: state.me.server.time,
        log_epoch: state.logEpoch,
      };
      res.write(`event: heartbeat\nid: ${state.head}\ndata: ${JSON.stringify(payload)}\n\n`);
    };

    const cleanup = () => {
      if (closed) {
        return;
      }
      closed = true;
      if (timer) {
        clearInterval(timer);
      }
      sseConnections.delete(entry);
    };

    const entry = {
      res,
      cleanup,
      // 实时推送新事件；过滤条件与回放相同。
      push(event) {
        if (closed || res.writableEnded) {
          return;
        }
        if (!matchesProject(event, projectIds) || !matchesEventType(event.type, types)) {
          return;
        }
        res.write(`id: ${event.seq}\nevent: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
      },
    };
    sseConnections.add(entry);
    req.on("close", cleanup);
    res.on("close", cleanup);

    if (cursor !== null) {
      for (const event of state.events) {
        if (event.seq <= cursor || event.seq > state.head) {
          continue;
        }
        if (!matchesProject(event, projectIds) || !matchesEventType(event.type, types)) {
          continue;
        }
        res.write(`id: ${event.seq}\nevent: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
      }
    }
    heartbeat();
    timer = setInterval(heartbeat, heartbeatMs);
    timer.unref?.();
  }

  const handlers = {
    getMe: handleGetMe,
    listProjects: handleListProjects,
    getProject: handleGetProject,
    listDeliveries: handleListDeliveries,
    getDelivery: handleGetDelivery,
    patchDelivery: handlePatchDelivery,
    abandonDelivery: handleAbandonDelivery,
    getDeliveryBudget: handleGetDeliveryBudget,
    listConversations: handleListConversations,
    listMessages: handleListMessages,
    postMessage: handlePostMessage,
    listWorktrees: handleListWorktrees,
    getWorktree: handleGetWorktree,
    acquireWorktreeLock: handleAcquireWorktreeLock,
    releaseWorktreeLock: handleReleaseWorktreeLock,
    listDeliveryWorkflows: handleListDeliveryWorkflows,
    getWorkflow: handleGetWorkflow,
    listNodeAttempts: handleListNodeAttempts,
    getAttempt: handleGetAttempt,
    listInbox: handleListInbox,
    getInboxItem: handleGetInboxItem,
    dismissInboxItem: handleDismissInboxItem,
    chooseInboxOption: handleChooseInboxOption,
    getInboxSummary: handleGetInboxSummary,
    listApprovals: handleListApprovals,
    getApproval: handleGetApproval,
    createApproval: handleCreateApproval,
    denyApproval: handleDenyApproval,
    grantApproval: handleGrantApproval,
    getApprovalChallenge: handleGetApprovalChallenge,
    getSettings: handleGetSettings,
    putSettings: handlePutSettings,
    getProjectSettings: handleGetProjectSettings,
    putProjectSettings: handlePutProjectSettings,
    deleteProjectSettings: handleDeleteProjectSettings,
    getSettingsCatalog: handleGetSettingsCatalog,
    getSettingsStatus: handleGetSettingsStatus,
    listDevices: handleListDevices,
    renameDevice: handleRenameDevice,
    setDeviceRole: handleSetDeviceRole,
    revokeDevice: handleRevokeDevice,
    renewDeviceCertificate: handleRenewDeviceCertificate,
    putPushToken: handlePutPushToken,
    pairDevice: handlePairDevice,
    retryNode: handleRetryNode,
    pauseDelivery: handlePauseDelivery,
    resumeDelivery: handleResumeDelivery,
  };

  async function dispatch(req, res, matched, url) {
    const { route } = matched;
    const context = contextFor(req, url, route);
    context.params = matched.params;
    context.req = req;
    context.rateClass = route.operation["x-gantry-rate"] || "read";
    requestLog.push({ method: req.method, path: url.pathname, operationId: route.operation.operationId });
    if (requestLog.length > 500) {
      requestLog.shift();
    }
    const injected = failNext.get(route.operation.operationId);
    if (injected) {
      failNext.delete(route.operation.operationId);
      throw new HttpProblem(injected.status, injected.code, injected.detail);
    }

    // Header precedence: an existing Last-Event-ID wins over the after query parameter.
    if (route.operation.operationId === "streamEvents" && req.headers["last-event-id"] !== undefined) {
      url.searchParams.delete("after");
    }
    const parameters = api.getParameters(route.operation, route.path);
    for (const parameter of parameters) {
      if (parameter.in !== "path") {
        continue;
      }
      const value = matched.params[parameter.name];
      if (value === undefined) {
        continue;
      }
      const schema = pathParameterSchema(api, parameter.schema);
      if (!schema) {
        continue;
      }
      const check = validator.validate(schema, value);
      if (!check.ok) {
        throw new HttpProblem(400, "invalid_request", `path parameter ${parameter.name} is invalid`);
      }
    }
    context.query = validateQuery(
      url.searchParams,
      parameters.filter((parameter) => parameter.in === "query"),
      validator,
    );

    if (route.operation.operationId === "streamEvents") {
      await handleSse(req, res, context);
      return;
    }

    await collectBody(context, req);
    const idempotencyKey = requireIdempotencyKey(context);
    const idempotencyState = checkIdempotency(context, idempotencyKey);
    if (idempotencyState?.replay) {
      const replay = idempotencyState.replay;
      const plan = {
        status: replay.status,
        body: deepClone(replay.body),
        headers: { ...replay.headers, "Idempotent-Replayed": "true" },
        noContent: replay.noContent,
      };
      sendPlan(context, res, plan);
      return;
    }

    parseBody(context);
    const handler = handlers[route.operation.operationId];
    if (!handler) {
      throw new HttpProblem(500, "internal", `mock handler missing for ${route.operation.operationId}`);
    }
    const plan = await handler(context);
    storeIdempotency(context, idempotencyState?.key, idempotencyState?.fingerprint, plan);
    sendPlan(context, res, plan);
  }

  async function handle(req, res) {
    const url = new URL(req.url || "/", "http://127.0.0.1");
    const fallbackContext = {
      requestId: requestId(),
      instance: url.pathname,
      state,
      rateClass: "read",
    };
    try {
      if (url.pathname.startsWith("/__mock/")) {
        await handleControl(req, res, url);
        return;
      }
      if (url.pathname === "/v1/pair" && req.method !== "POST") {
        throw new HttpProblem(404, "not_found", "pair is only available on the pairing listener");
      }
      let matched;
      try {
        matched = api.find(req.method || "GET", url.pathname);
      } catch {
        throw new HttpProblem(400, "invalid_request", "path is not valid URI encoding");
      }
      if (!matched) {
        throw new HttpProblem(404, "not_found", "route not found");
      }
      if (!matched.route) {
        throw new HttpProblem(405, "method_not_allowed", "method is not allowed for this path", {
          allow: matched.group.allow.join(", "),
        });
      }
      await dispatch(req, res, matched, url);
    } catch (error) {
      if (res.headersSent) {
        res.end();
        return;
      }
      if (error instanceof HttpProblem) {
        sendProblem(res, error, { ...fallbackContext, ...(error.context || {}) });
        return;
      }
      console.error(`[mock-api] internal error: ${error instanceof Error ? error.message : String(error)}`);
      sendProblem(
        res,
        new HttpProblem(500, "internal", "unexpected mock error"),
        fallbackContext,
      );
    }
  }

  function endStreams() {
    for (const connection of [...sseConnections]) {
      connection.cleanup();
      connection.res.end();
    }
    sseConnections.clear();
  }

  async function resetState() {
    const fresh = createApplicationState(await loadFixtureState());
    for (const key of Object.keys(state)) {
      delete state[key];
    }
    Object.assign(state, fresh);
    idempotency.clear();
    challenges.clear();
    pausedStages.clear();
    failNext.clear();
    requestLog.length = 0;
    endStreams();
  }

  /**
   * 场景控制面（desktop-ui §12；#224）：只在 Mock 上存在，不属于 api.md 契约，也不进 OpenAPI。
   *   POST /__mock/reset          以夹具重建全部状态并断开事件流
   *   POST /__mock/events         { type, subject, data, project_id?, correlation_id?, actor? } 追加并实时推送
   *   POST /__mock/drop-streams   断开已建立的事件流（客户端按游标重连）
   *   POST /__mock/mutate         { collection, id, patch } | { collection, push } | { set, value }
   *   POST /__mock/clock          { advance_s } | { time }：推进或设置服务端时间
   *   POST /__mock/fail-next      { operation_id, status, code, detail? }：下一次该操作返回指定 problem
   *   GET  /__mock/requests       请求日志；DELETE 清空
   */
  async function handleControl(req, res, url) {
    const send = (status, body) => {
      const payload = body === undefined ? "" : JSON.stringify(body);
      res.writeHead(status, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" });
      res.end(payload);
    };
    const action = url.pathname.slice("/__mock/".length);
    const raw = await readBody(req, 1 << 20);
    let body = {};
    try {
      body = raw ? JSON.parse(raw) : {};
    } catch {
      send(400, { error: "invalid_json" });
      return;
    }
    try {
      if (action === "requests" && req.method === "GET") {
        send(200, requestLog);
      } else if (action === "requests" && req.method === "DELETE") {
        requestLog.length = 0;
        send(204);
      } else if (req.method !== "POST") {
        send(405, { error: "method_not_allowed" });
      } else if (action === "reset") {
        await resetState();
        send(200, { head: state.head, log_epoch: state.logEpoch });
      } else if (action === "events") {
        const envelope = emitEvent(body.type, body.subject, body.data ?? {}, {
          projectId: body.project_id ?? null,
          correlationId: body.correlation_id,
          actor: body.actor,
        });
        send(201, envelope);
      } else if (action === "drop-streams") {
        endStreams();
        send(204);
      } else if (action === "mutate") {
        if (typeof body.set === "string") {
          state[body.set] = body.value;
          send(200, { [body.set]: state[body.set] });
          return;
        }
        const items = Array.isArray(state[body.collection]?.items) ? state[body.collection].items : null;
        if (!items) {
          send(400, { error: "unknown_collection" });
          return;
        }
        if (body.push) {
          items.push(body.push);
          send(201, body.push);
          return;
        }
        const item = items.find((entry) => entry.id === body.id);
        if (!item) {
          send(404, { error: "unknown_item" });
          return;
        }
        Object.assign(item, body.patch ?? {});
        send(200, item);
      } else if (action === "clock") {
        const base = typeof body.time === "string" ? body.time : addSeconds(state.me.server.time, Number(body.advance_s ?? 0));
        if (Number.isNaN(Date.parse(base))) {
          send(400, { error: "invalid_time" });
          return;
        }
        state.me.server.time = new Date(Date.parse(base)).toISOString();
        send(200, { time: state.me.server.time });
      } else if (action === "fail-next") {
        failNext.set(body.operation_id, {
          status: Number(body.status),
          code: String(body.code),
          detail: body.detail ?? String(body.code),
        });
        send(204);
      } else {
        send(404, { error: "unknown_control" });
      }
    } catch (error) {
      send(400, { error: error instanceof Error ? error.message : String(error) });
    }
  }

  async function close() {
    endStreams();
  }

  return { api, state, validator, handle, close, requestLog, idempotency };
}
