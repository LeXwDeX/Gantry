import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { createValidator } from "../lib/validator.mjs";

const root = new URL("../../../", import.meta.url);

async function readJson(path) {
  return JSON.parse(await readFile(new URL(path, root), "utf8"));
}

async function loadSpec() {
  return JSON.parse(await readFile(new URL("schemas/openapi.yaml", root), "utf8"));
}

function assertValid(validator, name, value, label = name) {
  const result = validator.validateNamed(name, value);
  assert.equal(result.ok, true, `${label} invalid: ${JSON.stringify(result.errors)}`);
}

test("OpenAPI covers the 48 contract operations and no invented result route", async () => {
  const spec = await loadSpec();
  const methods = new Set(["get", "post", "put", "patch", "delete"]);
  const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
    Object.entries(item)
      .filter(([method]) => methods.has(method))
      .map(([method, operation]) => ({ method, path, operation })),
  );
  assert.equal(operations.length, 48);
  assert.equal(new Set(operations.map(({ operation }) => operation.operationId)).size, 48);
  assert.equal(Object.keys(spec.paths).some((path) => path.includes("/results")), false);
  for (const { method, path, operation } of operations) {
    assert.ok(operation.operationId, `${method} ${path} has no operationId`);
    assert.ok(operation["x-gantry-role"], `${method} ${path} has no x-gantry-role`);
    assert.ok(operation["x-gantry-idempotency"], `${method} ${path} has no idempotency annotation`);
    assert.ok(Object.keys(operation.responses).length > 0, `${method} ${path} has no responses`);
  }
  assert.ok(spec.components.schemas.ModelCredential, "model credential must be typed");
  assert.equal(spec.components.schemas.SettingsResource.enum.length, 8);

  const knownFixtures = new Set([
    "server", "me", "devices", "pairing", "projects", "deliveries", "budgets",
    "conversations", "messages", "worktrees", "workflows", "workflows-list", "node",
    "attempts", "inbox", "inbox-summary", "approvals", "challenge", "settings", "catalog",
    "settings-status", "device-renew", "events",
  ]);
  for (const { method, path, operation } of operations) {
    const fixture = operation["x-gantry-fixture"];
    if (fixture) {
      assert.ok(knownFixtures.has(fixture), `${method} ${path} references unknown fixture ${fixture}`);
    }
  }
});

test("every request and response schema compiles", async () => {
  const spec = await loadSpec();
  const validator = createValidator(spec);
  for (const pathItem of Object.values(spec.paths)) {
    for (const [method, operation] of Object.entries(pathItem)) {
      if (!["get", "post", "put", "patch", "delete"].includes(method)) continue;
      const requestSchema = operation.requestBody?.content?.["application/json"]?.schema;
      if (requestSchema) validator.validate(requestSchema, undefined);
      for (const [status, response] of Object.entries(operation.responses)) {
        if (status === "204") continue;
        const content = response.content || {};
        const media = content["application/json; charset=utf-8"] || content["application/json"];
        if (media?.schema) validator.validate(media.schema, undefined);
      }
    }
  }
});

test("fixtures use the entity schemas and preserve coherent IDs", async () => {
  const spec = await loadSpec();
  const validator = createValidator(spec);
  const devices = await readJson("fixtures/api/devices.json");
  for (const [index, item] of devices.items.entries()) assertValid(validator, "Device", item, `device[${index}]`);
  assert.equal(JSON.stringify(devices).includes("approval_pubkey\""), false);
  assert.equal(JSON.stringify(devices).includes("\"approval_pubkey\""), false);

  const projects = await readJson("fixtures/api/projects.json");
  for (const item of projects.items) assertValid(validator, "Project", item);

  const deliveries = await readJson("fixtures/api/deliveries.json");
  for (const item of deliveries.items) assertValid(validator, "Delivery", item);
  for (const delivery of deliveries.items) {
    assert.ok(projects.items.some((project) => project.id === delivery.project_id));
  }

  const budgets = await readJson("fixtures/api/budgets.json");
  for (const item of budgets.items) {
    assertValid(validator, "BudgetLedger", item);
    assert.ok(deliveries.items.some((delivery) => delivery.id === item.delivery_id));
  }

  const conversations = await readJson("fixtures/api/conversations.json");
  for (const item of conversations.items) {
    assertValid(validator, "Conversation", item);
    assert.ok(projects.items.some((project) => project.id === item.project_id));
  }

  const messages = await readJson("fixtures/api/messages.json");
  for (const item of messages.items) {
    assertValid(validator, "Message", item);
    assert.ok(conversations.items.some((conversation) => conversation.id === item.conversation_id));
  }

  const worktrees = await readJson("fixtures/api/worktrees.json");
  for (const item of worktrees.items) assertValid(validator, "Worktree", item);
  for (const delivery of deliveries.items) {
    if (delivery.worktree_id) {
      assert.ok(worktrees.items.some((worktree) => worktree.id === delivery.worktree_id));
    }
  }

  const workflows = await readJson("fixtures/api/workflows.json");
  for (const item of workflows.items) {
    const { nodes, ...summary } = item;
    assertValid(validator, "WorkflowSummary", summary);
    assertValid(validator, "Workflow", item);
    nodes.forEach((node) => assertValid(validator, "Node", node));
    assert.ok(deliveries.items.some((delivery) => delivery.id === item.delivery_id));
  }

  const attempts = await readJson("fixtures/api/attempts.json");
  const nodeIds = new Set(workflows.items.flatMap((workflow) => workflow.nodes.map((node) => node.id)));
  for (const item of attempts.items) {
    assertValid(validator, "Attempt", item);
    assert.ok(nodeIds.has(item.node_id));
  }

  const inbox = await readJson("fixtures/api/inbox.json");
  for (const item of inbox.items) assertValid(validator, "InboxItem", item);

  const approvals = await readJson("fixtures/api/approvals.json");
  for (const item of approvals.items) assertValid(validator, "Approval", item);

  assertValid(validator, "MeResponse", await readJson("fixtures/api/me.json"));
  assertValid(validator, "PairResponse", await readJson("fixtures/api/pairing.json"));
  const catalog = await readJson("fixtures/api/catalog.json");
  assertValid(validator, "SettingsCatalog", catalog);
  assert.equal(Object.keys(catalog.policies.actions).length, 20);
  assert.equal(Object.keys(catalog.guardian.triage).length, 14);
  assert.equal(Object.keys(catalog.egress.suggested_groups).length, 6);
  assert.ok(catalog.roles.approver_keys.includes("policies.allow_globs"));
  assertValid(validator, "SettingsStatus", await readJson("fixtures/api/settings-status.json"));
  assertValid(validator, "InboxSummary", await readJson("fixtures/api/inbox-summary.json"));
  assertValid(validator, "ApprovalChallenge", await readJson("fixtures/api/challenge.json"));
});

test("settings fixtures use resource-specific schemas and never expose secret values", async () => {
  const spec = await loadSpec();
  const validator = createValidator(spec);
  const settings = await readJson("fixtures/api/settings.json");
  const mapping = spec["x-gantry-contract"].settings_resource_schemas;
  for (const [resource, entry] of Object.entries(settings.global)) {
    assert.ok(entry.rev >= 0);
    const schemaName = mapping[resource].$ref.split("/").pop();
    assertValid(validator, schemaName, entry.value, `settings.${resource}`);
    assert.equal(JSON.stringify(entry.value).includes("\"value\""), false);
  }
  for (const [projectId, overrides] of Object.entries(settings.project)) {
    assert.ok(projectId.startsWith("prj_"));
    for (const [resource, entry] of Object.entries(overrides)) {
      const schemaName = mapping[resource].$ref.split("/").pop();
      assertValid(validator, schemaName, entry.value, `project.${projectId}.${resource}`);
      assert.equal(JSON.stringify(entry.value).includes("\"value\""), false);
    }
  }
});

test("recorded events use complete core envelopes and defined data shapes", async () => {
  const spec = await loadSpec();
  const validator = createValidator(spec);
  const events = await readJson("fixtures/api/events.json");
  assert.ok(events.events.length > 0);
  let previous = 0;
  for (const event of events.events) {
    assertValid(validator, "GantryEvent", event);
    assert.ok(event.seq > previous);
    previous = event.seq;
    assert.equal(event.project_id === null || event.project_id.startsWith("prj_"), true);
    assert.ok(event.actor.kind);
    assert.ok(event.causation_id === null || event.causation_id.startsWith("evt_"));
    assert.ok(event.correlation_id === null || typeof event.correlation_id === "string");
  }
  assert.equal(events.head, previous);
  assert.ok(events.log_epoch >= 0);
});

test("all GET fixtures satisfy their response schemas", async () => {
  const spec = await loadSpec();
  const validator = createValidator(spec);
  const checks = [
    ["Project", "fixtures/api/projects.json"],
    ["Delivery", "fixtures/api/deliveries.json"],
    ["Message", "fixtures/api/messages.json"],
    ["Worktree", "fixtures/api/worktrees.json"],
    ["Attempt", "fixtures/api/attempts.json"],
    ["InboxItem", "fixtures/api/inbox.json"],
    ["Approval", "fixtures/api/approvals.json"],
  ];
  for (const [schema, path] of checks) {
    const data = await readJson(path);
    data.items.forEach((item) => assertValid(validator, schema, item, path));
  }
});
