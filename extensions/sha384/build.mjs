import { fileURLToPath } from "node:url";
import { buildComponent } from "../build-component.mjs";
await buildComponent({
  root: fileURLToPath(new URL(".", import.meta.url)),
  artifact: "hibana_sha384.wasm",
  output: "sha384.wasm",
});
