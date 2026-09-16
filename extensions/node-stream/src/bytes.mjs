import legacy from "../dist/readable-stream/lib/internal/streams/legacy.js";
import { Buffer } from "node:buffer";

// readable-stream 4.7.0 normally initializes these in its full entry.
// Share the byte helpers without loading optional stream operators.
legacy.Stream._isUint8Array ??= (value) => value instanceof Uint8Array;
legacy.Stream._uint8ArrayToBuffer ??= (value) =>
  Buffer.from(value.buffer, value.byteOffset, value.byteLength);
