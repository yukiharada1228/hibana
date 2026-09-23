// Match the API's public_apps::valid_label, including numeric first characters.
export function isApplicationName(value) {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= 63 &&
    !/[^a-z0-9-]/.test(value) &&
    !value.startsWith("-") &&
    !value.endsWith("-")
  );
}
