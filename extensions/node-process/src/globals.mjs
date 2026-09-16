import process from "./index.mjs";
globalThis.process ??= process;
globalThis.global ??= globalThis;
