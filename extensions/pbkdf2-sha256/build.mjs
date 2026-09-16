import { fileURLToPath } from "node:url";
import { buildComponent } from "../build-component.mjs";
await buildComponent({
  root: fileURLToPath(new URL(".", import.meta.url)),
  artifact: "hibana_pbkdf2_sha256.wasm",
  output: "pbkdf2-sha256.wasm",
});
