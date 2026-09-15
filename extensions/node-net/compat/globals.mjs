import { Buffer } from "buffer/";
import process from "process/browser.js";
globalThis.Buffer ??= Buffer;
globalThis.global ??= globalThis;
// readable-stream uses the browser process shim for nextTick scheduling only.
globalThis.process ??= process;
