import createSasl from "pg/lib/crypto/sasl.js";
import { bytes as random } from "@hibana/random";
import { digest } from "@hibana/sha256";
import { sign } from "@hibana/hmac-sha256";
import { derive } from "@hibana/pbkdf2-sha256";
import { normalizeNfkc } from "@hibana/unicode-nfkc";
import { Buffer } from "node:buffer";

const hashNames = new Set([
  "SHA-224",
  "SHA-256",
  "SHA-384",
  "SHA-512",
  "SHA512-224",
  "SHA512-256",
]);
function missingDigest(name) {
  return Object.assign(
    new Error(
      `Channel binding requires an explicitly selected ${name} certificate digest`,
    ),
    {
      code: "ERR_PG_CERTIFICATE_DIGEST_UNAVAILABLE",
    },
  );
}

export function scramSha256({ certificateDigests = {} } = {}) {
  if (
    !certificateDigests ||
    typeof certificateDigests !== "object" ||
    Array.isArray(certificateDigests)
  )
    throw new TypeError(
      "certificateDigests must map hash names to digest functions",
    );
  const algorithms = new Map(Object.entries(certificateDigests));
  for (const [name, fn] of algorithms)
    if (!hashNames.has(name) || typeof fn !== "function")
      throw new TypeError(`Invalid certificate digest: ${name}`);
  const crypto = {
    normalizeNfkc,
    randomBytes: (length) => Buffer.from(random(length)),
    sha256: async (data) => digest(Buffer.from(data)),
    hmacSha256: async (key, data) => sign(Buffer.from(key), Buffer.from(data)),
    deriveKey: async (password, salt, iterations) =>
      derive(Buffer.from(password), Buffer.from(salt), iterations),
    hashByName(name, data) {
      const hash = algorithms.get(name);
      if (!hash) throw missingDigest(name);
      return hash(Buffer.from(data));
    },
  };
  return Object.freeze(createSasl(crypto));
}
