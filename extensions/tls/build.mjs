import { fileURLToPath } from "node:url";
import { buildComponent } from "../build-component.mjs";
await buildComponent({
  root: fileURLToPath(new URL(".", import.meta.url)),
  artifact: "hibana_tls.wasm",
  output: "tls.wasm",
});
