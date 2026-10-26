import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const METHODS = new Set(["get", "post", "put", "patch", "delete"]);

export const defaultSpecPath = fileURLToPath(
  new URL("../../../schemas/openapi.yaml", import.meta.url),
);

function compilePath(pattern) {
  const names = [];
  const parts = pattern.split("/").filter(Boolean).map((segment) => {
    const match = /^\{([A-Za-z0-9_]+)\}$/.exec(segment);
    if (!match) {
      return segment.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    }
    names.push(match[1]);
    return "([^/]+)";
  });
  return {
    regex: new RegExp(`^/${parts.join("/")}/?$`),
    names,
    staticRank: parts.filter((part) => !part.startsWith("(")).length,
  };
}

function resolveRef(spec, value) {
  if (!value || typeof value !== "object" || !value.$ref) {
    return value;
  }
  const match = /^#\/components\/([^/]+)\/(.+)$/.exec(value.$ref);
  if (!match) {
    return value;
  }
  return spec.components?.[match[1]]?.[match[2]] ?? value;
}

export async function loadOpenApi(specPath = defaultSpecPath) {
  const spec = JSON.parse(await readFile(specPath, "utf8"));
  const groups = [];
  for (const [path, pathItem] of Object.entries(spec.paths || {})) {
    const { regex, names, staticRank } = compilePath(path);
    const methods = new Map();
    for (const [method, operation] of Object.entries(pathItem || {})) {
      if (!METHODS.has(method)) {
        continue;
      }
      methods.set(method, {
        method: method.toUpperCase(),
        path,
        operation,
        paramNames: names,
      });
    }
    groups.push({
      path,
      regex,
      names,
      staticRank,
      methods,
      allow: [...methods.keys()].map((method) => method.toUpperCase()).sort(),
    });
  }
  groups.sort((left, right) => {
    if (right.staticRank !== left.staticRank) {
      return right.staticRank - left.staticRank;
    }
    return right.path.length - left.path.length;
  });

  const operations = groups.flatMap((group) =>
    [...group.methods.values()].map((route) => ({
      ...route,
      group,
    })),
  );

  function match(pathname) {
    for (const group of groups) {
      const matchResult = group.regex.exec(pathname);
      if (!matchResult) {
        continue;
      }
      const params = {};
      group.names.forEach((name, index) => {
        params[name] = decodeURIComponent(matchResult[index + 1]);
      });
      return { group, params };
    }
    return null;
  }

  function find(method, pathname) {
    const matched = match(pathname);
    if (!matched) {
      return null;
    }
    return {
      ...matched,
      route: matched.group.methods.get(method.toLowerCase()) || null,
    };
  }

  function getParameters(operation, groupPath) {
    const pathItem = spec.paths[groupPath];
    const parameters = [...(pathItem?.parameters || []), ...(operation.parameters || [])].map(
      (parameter) => resolveRef(spec, parameter),
    );
    return parameters;
  }

  function getOperation(operationId) {
    return operations.find((route) => route.operation.operationId === operationId) || null;
  }

  function getResponse(operation, status) {
    const response = operation.responses?.[String(status)];
    return resolveRef(spec, response);
  }

  return {
    spec,
    groups,
    operations,
    match,
    find,
    getParameters,
    getOperation,
    getResponse,
  };
}

export { compilePath };
