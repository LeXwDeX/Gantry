import { HttpProblem } from "./http.mjs";

function fail(detail) {
  throw new HttpProblem(400, "invalid_request", detail);
}

function convertScalar(raw, schema) {
  if (schema.type === "integer") {
    if (!/^(0|[1-9][0-9]*)$/.test(raw)) {
      fail("query parameter must be an integer");
    }
    return Number(raw);
  }
  if (schema.type === "boolean") {
    if (raw === "true") {
      return true;
    }
    if (raw === "false") {
      return false;
    }
    fail("query parameter must be true or false");
  }
  return raw;
}

function checkValue(value, schema, validator) {
  const check = validator.validate(schema, value);
  if (!check.ok) {
    const detail = check.errors.map((error) => error.message).join("; ");
    fail(`query parameter is invalid: ${detail}`);
  }
}

export function validateQuery(searchParams, parameters, validator) {
  const byName = new Map(parameters.map((parameter) => [parameter.name, parameter]));
  const values = {};
  for (const [name, parameter] of byName) {
    const schema = parameter.schema || {};
    const raw = searchParams.getAll(name);
    if (raw.length === 0) {
      if (parameter.required) {
        fail(`query parameter ${name} is required`);
      }
      continue;
    }
    const repeatable = Boolean(parameter["x-gantry-repeatable"]) || schema.type === "array";
    if (raw.length > 1 && !repeatable) {
      fail(`query parameter ${name} must not repeat`);
    }
    if (schema.type === "array") {
      const items = raw.flatMap((entry) => entry.split(",")).filter((entry) => entry !== "");
      for (const entry of items) {
        checkValue(entry, schema.items || {}, validator);
      }
      values[name] = items;
    } else if (repeatable && raw.length > 1) {
      values[name] = raw.map((entry) => convertScalar(entry, schema));
      values[name].forEach((entry) => checkValue(entry, schema, validator));
    } else {
      const converted = convertScalar(raw[0], schema);
      checkValue(converted, schema, validator);
      values[name] = converted;
    }
  }
  for (const key of searchParams.keys()) {
    if (!byName.has(key)) {
      fail(`unknown query parameter ${key}`);
    }
  }
  return values;
}
