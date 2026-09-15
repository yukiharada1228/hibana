// Application-owned subset: SHA-256, UTF-8/Uint8Array input, hex output only.
// The hash implementation is provided by a Rust Wasm Component, not a host API.
import { sha256 } from 'example:crypto/hash@1.0.0';

export function createHash(algorithm) {
  if (algorithm !== 'sha256') throw new Error('This extension implements SHA-256 only');
  let chunks = [], finished = false;
  const hash = {
    update(value) {
      if (finished) throw new Error('Digest already called');
      const bytes = typeof value === 'string' ? new TextEncoder().encode(value) : value;
      if (!(bytes instanceof Uint8Array)) throw new TypeError('Expected a string or Uint8Array');
      chunks.push(bytes.slice());
      return hash;
    },
    digest(encoding) {
      if (finished) throw new Error('Digest already called');
      if (encoding !== 'hex') throw new Error('This extension implements hex output only');
      finished = true;
      const bytes = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.length, 0));
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
      chunks = [];
      return sha256(bytes);
    },
  };
  return hash;
}
