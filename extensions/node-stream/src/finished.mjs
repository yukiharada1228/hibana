// Observing completion does not require constructing a stream or its byte helpers.
// The default upstream export is callback-based, like node:stream.finished.
export {
  default,
  default as finished,
} from "../dist/readable-stream/lib/internal/streams/end-of-stream.js";
