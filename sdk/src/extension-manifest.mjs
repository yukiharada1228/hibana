// Public configuration is a list of extensions; author details live in a manifest.
export const HTTP_CONTRACT = "wasi:http/incoming-handler@0.2.3";
const API_VERSIONS = [1, 2];
const MAX_ITEMS = 64;
const PACKAGE_NAME = /^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*$/;
const INTERFACE_NAME =
  /^[a-z][a-z0-9-]*:[a-z][a-z0-9-]*\/[a-z][a-z0-9-]*(?:@[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?)?$/;
const FIELDS = new Set([
  "schemaVersion",
  "runtime",
  "aliases",
  "preload",
  "components",
  "imports",
  "wit",
  "permissions",
  "dependencies",
]);
const isObject = (value) =>
  value !== null && typeof value === "object" && !Array.isArray(value);
const isText = (value) =>
  typeof value === "string" && value.trim().length > 0 && !value.includes("\0");
const isList = (value) =>
  Array.isArray(value) &&
  value.length <= MAX_ITEMS &&
  value.every(isText) &&
  new Set(value).size === value.length;

export function isLocalExtension(value) {
  return typeof value === "string" && value.startsWith("./");
}

export function isExtensionPackage(value) {
  return (
    typeof value === "string" && value.length <= 214 && PACKAGE_NAME.test(value)
  );
}

export function validateExtensionList(value = []) {
  if (!Array.isArray(value)) {
    throw new Error(
      'extensions must be an array, for example ["@hibana/node-net"]. Move aliases/WIT/components into hibana.extension.json; replace extensions.packages with this array.',
    );
  }
  const validReference = (name) =>
    isLocalExtension(name)
      ? !name.includes("\\") && !name.split("/").includes("..")
      : isExtensionPackage(name);
  if (!isList(value) || !value.every(validReference)) {
    throw new Error(
      "extensions must list unique installed npm package names or project-relative directories starting with ./ (at most 64)",
    );
  }
}

export function validateManifest(manifest, { javascript }) {
  if (!isObject(manifest)) throw new Error("Manifest must be an object");
  for (const key of Object.keys(manifest)) {
    if (!FIELDS.has(key)) throw new Error(`Unsupported manifest field: ${key}`);
  }
  if (!API_VERSIONS.includes(manifest.schemaVersion))
    throw new Error(
      `Unsupported schemaVersion; this CLI supports ${API_VERSIONS.join(" and ")}`,
    );
  if (manifest.dependencies !== undefined && manifest.schemaVersion !== 2)
    throw new Error("Extension dependencies require schemaVersion 2");
  if (manifest.runtime !== HTTP_CONTRACT)
    throw new Error(
      `Incompatible runtime contract; this CLI supports ${HTTP_CONTRACT}`,
    );

  const {
    aliases = {},
    preload = [],
    components = [],
    imports = [],
    permissions = [],
    dependencies = [],
    wit,
  } = manifest;
  if (!isList(dependencies) || !dependencies.every(isExtensionPackage))
    throw new Error(
      "dependencies must list unique installed npm package names (at most 64)",
    );
  if (
    !isObject(aliases) ||
    Object.keys(aliases).length > MAX_ITEMS ||
    Object.entries(aliases).some(
      ([name, target]) =>
        !isText(name) || !isText(target) || INTERFACE_NAME.test(name),
    )
  ) {
    throw new Error(
      "aliases must map module names to JavaScript files; WIT interfaces belong in imports",
    );
  }
  for (const [name, values] of Object.entries({
    preload,
    components,
    imports,
  })) {
    if (!isList(values))
      throw new Error(
        `${name} must list unique non-empty strings (at most 64)`,
      );
  }
  if (
    !isList(permissions) ||
    permissions.some((name) => name !== "outbound-network")
  ) {
    throw new Error(
      "Unsupported permissions; only outbound-network can be declared",
    );
  }
  if (imports.some((name) => !INTERFACE_NAME.test(name)))
    throw new Error("imports must use WIT interface names");
  if (
    !javascript &&
    (Object.keys(aliases).length ||
      preload.length ||
      imports.length ||
      wit !== undefined)
  ) {
    throw new Error(
      "JavaScript aliases, preloads and generated WIT require a JavaScript/TypeScript main entry",
    );
  }
  if (
    (imports.length || wit !== undefined) &&
    (!imports.length || !isText(wit) || !components.length)
  ) {
    throw new Error(
      "WIT extensions require imports, a wit directory and prebuilt components",
    );
  }
  return {
    aliases,
    preload,
    components,
    imports,
    permissions,
    wit,
    dependencies,
  };
}
