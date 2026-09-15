// Publisher-only build. Consumers receive the compiled Component and WIT in
// the npm tarball; Hibana never runs this script while resolving the extension.
import { spawnSync } from 'node:child_process';
import { copyFile, mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('.', import.meta.url));
const result = spawnSync('cargo', ['build', '--locked', '--release', '--target', 'wasm32-wasip2', '--manifest-path', 'rust-crypto/Cargo.toml'], { cwd: root, stdio: 'inherit' });
if (result.error) throw result.error;
if (result.status !== 0) throw new Error(`Rust extension build failed (${result.signal || result.status})`);
await mkdir(new URL('dist/', import.meta.url), { recursive: true });
await mkdir(new URL('wit/', import.meta.url), { recursive: true });
await copyFile(new URL('rust-crypto/target/wasm32-wasip2/release/user_crypto.wasm', import.meta.url), new URL('dist/crypto.wasm', import.meta.url));
await copyFile(new URL('rust-crypto/wit/world.wit', import.meta.url), new URL('wit/world.wit', import.meta.url));
