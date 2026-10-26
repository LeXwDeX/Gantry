import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

function rewriteRefs(value) {
  if (Array.isArray(value)) {
    return value.map(rewriteRefs);
  }
  if (value && typeof value === "object") {
    const result = {};
    for (const [key, nested] of Object.entries(value)) {
      if (key === "$ref" && typeof nested === "string") {
        result[key] = nested.replace(/^#\/components\/schemas\//, "#/$defs/");
      } else {
        result[key] = rewriteRefs(nested);
      }
    }
    return result;
  }
  return value;
}

function pointerFor(instancePath, extra) {
  const base = instancePath || "";
  return extra === undefined ? base : `${base}/${extra}`;
}

function normalizeErrors(errors) {
  const seen = new Set();
  const result = [];
  for (const error of errors || []) {
    let code = "type";
    let pointer = error.instancePath || "";
    if (error.keyword === "required") {
      code = "required";
      pointer = pointerFor(error.instancePath, error.params.missingProperty);
    } else if (error.keyword === "additionalProperties") {
      code = "unknown_field";
      pointer = pointerFor(error.instancePath, error.params.additionalProperty);
    } else if (error.keyword === "enum") {
      code = "enum";
    } else if (
      error.keyword === "minimum" ||
      error.keyword === "maximum" ||
      error.keyword === "exclusiveMinimum" ||
      error.keyword === "exclusiveMaximum" ||
      error.keyword === "minLength" ||
      error.keyword === "maxLength" ||
      error.keyword === "minItems" ||
      error.keyword === "maxItems"
    ) {
      code = "range";
    } else if (
      error.keyword === "oneOf" ||
      error.keyword === "anyOf" ||
      error.keyword === "type" ||
      error.keyword === "pattern" ||
      error.keyword === "format"
    ) {
      code = "type";
    }
    const key = `${pointer}|${code}`;
    if (!seen.has(key)) {
      seen.add(key);
      result.push({
        pointer,
        code,
        message: error.message || "invalid value",
      });
    }
  }
  return result;
}

export function createValidator(spec) {
  const ajv = new Ajv2020({
    allErrors: true,
    strict: false,
    allowUnionTypes: true,
    discriminator: false,
  });
  addFormats(ajv);
  ajv.addFormat("uint64", {
    type: "integer",
    validate: (value) => Number.isSafeInteger(value) && value >= 0,
  });
  const defs = rewriteRefs(spec.components.schemas);
  const cache = new Map();

  function compile(schema) {
    const key = JSON.stringify(schema);
    if (!cache.has(key)) {
      cache.set(key, ajv.compile({ ...schema, $defs: defs }));
    }
    return cache.get(key);
  }

  function validate(schema, value) {
    const check = compile(rewriteRefs(schema));
    const ok = check(value);
    return { ok, errors: ok ? [] : normalizeErrors(check.errors) };
  }

  function validateNamed(name, value) {
    return validate({ $ref: `#/components/schemas/${name}` }, value);
  }

  return { validate, validateNamed };
}
