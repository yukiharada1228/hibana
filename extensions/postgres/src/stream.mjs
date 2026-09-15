import { Socket } from "net";
import { connect } from "tls";

// PostgreSQL treats TCP_NODELAY as a latency optimization. WASI 0.2 has no
// corresponding socket option: only this pg-specific stream drops the hint.
// The public node-net Socket still rejects setNoDelay.
class PostgresSocket extends Socket {
  setNoDelay() {
    return this;
  }
}
export function getStream() {
  return new PostgresSocket();
}
export function getSecureStream(options) {
  return connect(options);
}
