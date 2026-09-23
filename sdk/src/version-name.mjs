// Keep aligned with the upload API. Existing names remain valid rollback targets.
export function validateVersionName(value) {
  if (
    typeof value !== "string" ||
    value.length < 1 ||
    value.length > 128 ||
    !/^[A-Za-z0-9]/.test(value) ||
    /[^A-Za-z0-9._+-]/.test(value)
  ) {
    throw new Error(
      "Version must be 1..128 ASCII characters, start with a letter or digit, and contain only letters, digits, '.', '_', '+', '-'",
    );
  }
  return value;
}
