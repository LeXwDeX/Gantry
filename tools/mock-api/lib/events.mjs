import { readdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

// 事件 data 的权威 schema 由 xtask 从 core 生成到 schemas/events/<type>.json；
// OpenAPI 的 GantryEvent 只列了夹具里出现的少数类型，不能用来校验 Mock 产生的全部事件。
const eventsDir = fileURLToPath(new URL("../../../schemas/events/", import.meta.url));

const integerFormats = {
  uint16: (value) => Number.isInteger(value) && value >= 0 && value <= 0xffff,
  uint32: (value) => Number.isInteger(value) && value >= 0 && value <= 0xffffffff,
  uint64: (value) => Number.isSafeInteger(value) && value >= 0,
  int64: (value) => Number.isSafeInteger(value),
};

let cached = null;

/** 编译约 80 份事件 schema 较慢；同一进程内的所有 Mock 实例共用一份编译结果。 */
export function loadEventSchemas() {
  cached ??= compileEventSchemas().catch((error) => {
    cached = null;
    throw error;
  });
  return cached;
}

async function compileEventSchemas() {
  const ajv = new Ajv2020({ allErrors: true, strict: false, allowUnionTypes: true });
  addFormats(ajv);
  for (const [name, validate] of Object.entries(integerFormats)) {
    ajv.addFormat(name, { type: "integer", validate });
  }
  ajv.addFormat("double", { type: "number", validate: (value) => Number.isFinite(value) });
  const validators = new Map();
  for (const file of await readdir(eventsDir)) {
    if (!file.endsWith(".json")) {
      continue;
    }
    const schema = JSON.parse(await readFile(`${eventsDir}${file}`, "utf8"));
    validators.set(file.slice(0, -".json".length), ajv.compile(schema));
  }
  return validators;
}

/** 校验事件 data；未知类型或不合法时抛错，与 Mock 对响应体的处理一致（宁可失败也不发出越界数据）。 */
export function assertEventData(validators, type, data) {
  const validate = validators.get(type);
  if (!validate) {
    throw new Error(`mock emitted unknown event type ${type}`);
  }
  if (!validate(data)) {
    throw new Error(`mock event ${type} violates schemas/events: ${JSON.stringify(validate.errors)}`);
  }
}
