// Publisher-only: consumers install the precompiled component.
import { fileURLToPath } from "node:url";
import { buildComponent } from "../build-component.mjs";
await buildComponent({
  root: fileURLToPath(new URL(".", import.meta.url)),
  artifact: "hibana_node_net_transport.wasm",
  output: "transport.wasm",
});
