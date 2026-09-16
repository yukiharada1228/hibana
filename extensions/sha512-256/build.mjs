import { fileURLToPath } from "node:url";
import { buildComponent } from "../build-component.mjs";
await buildComponent({
  root: fileURLToPath(new URL(".", import.meta.url)),
  artifact: "hibana_sha512_256.wasm",
  output: "sha512-256.wasm",
});
