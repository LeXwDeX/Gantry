import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { describe, expect, it } from "vitest";

/**
 * 手写的 `src/api/types.ts` 与 `schemas/openapi.yaml` 的防漂移检查（#135）。
 *
 * 用 TypeScript 编译器读出类型结构，逐字段与 OpenAPI 组件比对：
 * - 字段集合一致（封闭对象 `additionalProperties: false` 时，TS 不能多出字段）；
 * - 可选性一致：TS 的 `?` ⇔ 不在 `required` 里；
 * - 可空性一致：TS 含 `null` ⇔ schema 接受 `null`。
 * 嵌套对象、数组元素、`additionalProperties` 映射与按 `kind` 区分的联合都会递归比对。
 * 枚举与格式不在本检查范围内；`unknown` 与 `Record<string, unknown>` 视为不透明，跳过。
 */

type Schema = Record<string, unknown>;

const here = dirname(fileURLToPath(import.meta.url));
const desktopRoot = resolve(here, "../..");
const typesFile = resolve(desktopRoot, "src/api/types.ts");
const openapi = JSON.parse(
  readFileSync(resolve(desktopRoot, "../../schemas/openapi.yaml"), "utf8"),
) as { components: { schemas: Record<string, Schema> } };
const components = openapi.components.schemas;

/** TS 类型名 → OpenAPI 组件名。 */
const MAPPING: Array<[string, string]> = [
  ["UntrustedText", "UntrustedText"],
  ["IssueRef", "IssueRef"],
  ["PullRequestRef", "PullRequestRef"],
  ["CheckSummary", "CheckSummary"],
  ["Ref", "Ref"],
  ["RefOption", "Option"],
  ["InboxOption", "Option"],
  ["Card", "Card"],
  ["Project", "Project"],
  ["Delivery", "Delivery"],
  ["Worktree", "Worktree"],
  ["WorkflowNode", "Node"],
  ["WorkflowSummary", "WorkflowSummary"],
  ["Workflow", "Workflow"],
  ["Attempt", "Attempt"],
  ["Conversation", "Conversation"],
  ["Message", "Message"],
  ["InboxPayload", "InboxPayload"],
  ["InboxItem", "InboxItem"],
  ["InboxSummary", "InboxSummary"],
  ["ApprovalTarget", "ApprovalTarget"],
  ["Approval", "Approval"],
  ["Device", "Device"],
  ["Me", "MeResponse"],
  ["BudgetLedger", "BudgetLedger"],
  ["SettingsCatalog", "SettingsCatalog"],
  ["SettingsStatus", "SettingsStatus"],
  ["EventEnvelope", "EventEnvelope"],
];

function deref(schema: Schema): Schema {
  let current = schema;
  while (typeof current.$ref === "string") {
    const name = current.$ref.replace("#/components/schemas/", "");
    current = components[name];
    if (!current) throw new Error(`unresolved $ref ${String(schema.$ref)}`);
  }
  return current;
}

function isNullSchema(schema: Schema): boolean {
  const s = deref(schema);
  return s.type === "null" || s.const === null;
}

/** 拆出 schema 的可空性与非 null 分支。 */
function splitSchema(schema: Schema): { nullable: boolean; branches: Schema[] } {
  const s = deref(schema);
  const union = (s.anyOf ?? s.oneOf) as Schema[] | undefined;
  if (union && !s.properties) {
    const branches = union.filter((branch) => !isNullSchema(branch));
    const nullable = branches.length !== union.length;
    if (branches.length === 1) {
      const inner = splitSchema(branches[0]);
      return { nullable: nullable || inner.nullable, branches: inner.branches };
    }
    // 嵌套的联合（如 `oneOf: [{$ref: Ref}, …]` 而 Ref 本身是 oneOf）拍平成一层。
    return { nullable, branches: branches.flatMap((branch) => splitSchema(branch).branches) };
  }
  if (Array.isArray(s.type) && s.type.includes("null")) {
    const rest = s.type.filter((t) => t !== "null");
    return { nullable: true, branches: [{ ...s, type: rest.length === 1 ? rest[0] : rest }] };
  }
  return { nullable: s.type === "null", branches: s.type === "null" ? [] : [s] };
}

const program = ts.createProgram([typesFile], {
  strict: true,
  target: ts.ScriptTarget.ES2022,
  noEmit: true,
});
const checker = program.getTypeChecker();
const source = program.getSourceFile(typesFile);
if (!source) throw new Error("types.ts not loaded");
const moduleSymbol = checker.getSymbolAtLocation(source);
if (!moduleSymbol) throw new Error("types.ts module symbol missing");
const exportsByName = new Map(
  checker.getExportsOfModule(moduleSymbol).map((symbol) => [symbol.getName(), symbol]),
);

function exportedType(name: string): ts.Type {
  const symbol = exportsByName.get(name);
  if (!symbol) throw new Error(`types.ts does not export ${name}`);
  const target = symbol.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(symbol) : symbol;
  return checker.getDeclaredTypeOfSymbol(target);
}

function splitTs(type: ts.Type): { nullable: boolean; members: ts.Type[] } {
  const members = type.isUnion() ? type.types : [type];
  const nullable = members.some((member) => member.flags & ts.TypeFlags.Null);
  const rest = members.filter(
    (member) => !(member.flags & (ts.TypeFlags.Null | ts.TypeFlags.Undefined)),
  );
  return { nullable, members: rest };
}

function isOpaque(type: ts.Type): boolean {
  if (type.flags & (ts.TypeFlags.Unknown | ts.TypeFlags.Any)) return true;
  // Record<string, unknown> 一类：没有具名属性，只有值为 unknown 的索引签名。
  const index = checker.getIndexTypeOfType(type, ts.IndexKind.String);
  return (
    index !== undefined &&
    checker.getPropertiesOfType(type).length === 0 &&
    (index.flags & (ts.TypeFlags.Unknown | ts.TypeFlags.Any)) !== 0
  );
}

function isObjectType(type: ts.Type): boolean {
  return (type.flags & ts.TypeFlags.Object) !== 0 && !checker.isArrayType(type);
}

function kindLiteral(type: ts.Type): string | null {
  const kind = checker.getPropertyOfType(type, "kind");
  if (!kind) return null;
  const kindType = checker.getTypeOfSymbol(kind);
  return kindType.isStringLiteral() ? kindType.value : null;
}

function schemaKind(schema: Schema): string | null {
  const props = schema.properties as Record<string, Schema> | undefined;
  const kind = props?.kind;
  if (!kind) return null;
  if (typeof kind.const === "string") return kind.const;
  return null;
}

function compare(type: ts.Type, schema: Schema, path: string, errors: string[]): void {
  const split = splitTs(type);
  if (split.members.length > 0 && split.members.every(isOpaque)) return;
  // 联合里的不透明兜底成员（如 Record<string, unknown>）不参与比对。
  const ts_ = { nullable: split.nullable, members: split.members.filter((member) => !isOpaque(member)) };
  const s = splitSchema(schema);
  if (ts_.nullable !== s.nullable) {
    errors.push(`${path}: 可空性不一致（TS ${ts_.nullable ? "可空" : "不可空"}，OpenAPI ${s.nullable ? "可空" : "不可空"}）`);
  }
  if (s.branches.length === 0 || ts_.members.length === 0) return;

  if (s.branches.length > 1) {
    compareUnion(ts_.members, s.branches, path, errors);
    return;
  }
  const branch = s.branches[0];
  if (ts_.members.length > 1) {
    // TS 是联合、schema 只有一支（如字面量联合对应 enum）：只比对象成员。
    const objects = ts_.members.filter(isObjectType);
    if (objects.length > 1) compareUnion(objects, [branch], path, errors);
    else if (objects.length === 1) compare(objects[0], branch, path, errors);
    return;
  }
  const member = ts_.members[0];
  if (checker.isArrayType(member)) {
    const items = branch.items as Schema | undefined;
    const element = checker.getTypeArguments(member as ts.TypeReference)[0];
    if (items && element) compare(element, items, `${path}[]`, errors);
    return;
  }
  if (isObjectType(member) && (branch.properties || branch.additionalProperties)) {
    compareObject(member, branch, path, errors);
  }
}

function compareObject(type: ts.Type, schema: Schema, path: string, errors: string[]): void {
  const properties = (schema.properties ?? {}) as Record<string, Schema>;
  const required = new Set((schema.required ?? []) as string[]);
  const tsProps = checker.getPropertiesOfType(type);
  const tsNames = new Set(tsProps.map((prop) => prop.getName()));

  for (const [name, propSchema] of Object.entries(properties)) {
    const prop = checker.getPropertyOfType(type, name);
    if (!prop) {
      errors.push(`${path}.${name}: TS 缺少该字段`);
      continue;
    }
    const optional = (prop.flags & ts.SymbolFlags.Optional) !== 0;
    if (optional === required.has(name)) {
      errors.push(`${path}.${name}: 可选性不一致（TS ${optional ? "可选" : "必填"}，OpenAPI ${required.has(name) ? "必填" : "可选"}）`);
    }
    compare(checker.getTypeOfSymbol(prop), propSchema, `${path}.${name}`, errors);
  }
  if (schema.additionalProperties === false) {
    for (const name of tsNames) {
      if (!(name in properties)) errors.push(`${path}.${name}: OpenAPI 没有该字段`);
    }
  }
  const additional = schema.additionalProperties;
  if (additional && typeof additional === "object") {
    const index = checker.getIndexTypeOfType(type, ts.IndexKind.String);
    if (index) compare(index, additional as Schema, `${path}{}`, errors);
  }
}

function compareUnion(members: ts.Type[], branches: Schema[], path: string, errors: string[]): void {
  for (const member of members) {
    if (!isObjectType(member) || isOpaque(member)) continue;
    const kind = kindLiteral(member);
    // 开放的兜底分支（kind: string + 索引签名）不参与比对。
    if (kind === null && checker.getPropertyOfType(member, "kind") && checker.getIndexTypeOfType(member, ts.IndexKind.String)) {
      continue;
    }
    let match: Schema | undefined;
    if (kind !== null) {
      match = branches.find((branch) => schemaKind(branch) === kind);
    } else {
      // 没有 kind 判别字段：按字段名集合重合度挑最接近的一支。
      const names = new Set(checker.getPropertiesOfType(member).map((prop) => prop.getName()));
      let best = -1;
      for (const branch of branches) {
        if (schemaKind(branch) !== null) continue;
        const keys = Object.keys((branch.properties ?? {}) as Schema);
        const overlap = keys.filter((key) => names.has(key)).length;
        const score = overlap / new Set([...keys, ...names]).size;
        if (score > best) {
          best = score;
          match = branch;
        }
      }
    }
    const label = kind ?? checker.typeToString(member);
    if (!match) {
      errors.push(`${path}<${label}>: OpenAPI 没有对应分支`);
      continue;
    }
    compareObject(member, match, `${path}<${label}>`, errors);
  }
}

describe("types.ts 与 openapi.yaml 的可空性 / 可选性", () => {
  it.each(MAPPING)("%s ↔ %s", (tsName, schemaName) => {
    const schema = components[schemaName];
    expect(schema, `OpenAPI 缺少组件 ${schemaName}`).toBeDefined();
    const errors: string[] = [];
    compare(exportedType(tsName), schema, tsName, errors);
    expect(errors).toEqual([]);
  });

  it("检查器能发现漂移", () => {
    const errors: string[] = [];
    // Option.primary 可空、可选；拿必填且不可空的 UntrustedText.truncated 对照会报错。
    compare(exportedType("UntrustedText"), {
      type: "object",
      additionalProperties: false,
      required: ["text", "source"],
      properties: {
        text: { anyOf: [{ type: "string" }, { type: "null" }] },
        source: { type: "string" },
        truncated: { type: "boolean" },
        extra: { type: "string" },
      },
    }, "UntrustedText", errors);
    expect(errors).toEqual([
      "UntrustedText.text: 可空性不一致（TS 不可空，OpenAPI 可空）",
      "UntrustedText.truncated: 可选性不一致（TS 必填，OpenAPI 可选）",
      "UntrustedText.extra: TS 缺少该字段",
    ]);
  });
});
