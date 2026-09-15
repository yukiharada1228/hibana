// pg's authentication primitives, backed by a guest-owned Rust Component.
// This is deliberately not a global node:crypto or WebCrypto implementation.
import * as rust from "hibana:postgres/crypto@0.1.0";

export const normalizeNfkc = rust.normalizeNfkc;

function bytes(value) {
  return typeof value === "string"
    ? Buffer.from(value, "utf8")
    : Buffer.from(value);
}
export function randomBytes(length) {
  if (!Number.isInteger(length) || length < 0 || length > 4096)
    throw new RangeError("Authentication random byte count must be 0..4096");
  return Buffer.from(rust.randomBytes(length));
}
export async function sha256(data) {
  return rust.sha256(bytes(data));
}
export async function hmacSha256(key, data) {
  return rust.hmacSha256(bytes(key), bytes(data));
}
export async function deriveKey(password, salt, iterations) {
  if (!Number.isInteger(iterations) || iterations < 1 || iterations > 100000)
    throw new RangeError("SCRAM iterations must be 1..100000");
  return rust.deriveKey(bytes(password), bytes(salt), iterations);
}
export async function md5(data) {
  return Buffer.from(rust.md5(bytes(data))).toString("hex");
}
export async function postgresMd5PasswordHash(user, password, salt) {
  const inner = await md5(password + user);
  return "md5" + (await md5(Buffer.concat([Buffer.from(inner), salt])));
}
export async function hashByName() {
  throw new Error("SCRAM channel binding is not supported by @hibana/postgres");
}
