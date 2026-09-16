import { Socket, isIP } from "net";

// WASI 0.2 has no TCP_NODELAY. pg uses it only as a latency hint.
class PostgresSocket extends Socket {
  setNoDelay() {
    return this;
  }
}
function unavailable() {
  throw Object.assign(
    new Error("This PostgreSQL transport does not include TLS"),
    {
      code: "ERR_PG_TLS_UNAVAILABLE",
    },
  );
}
export const tcp = Object.freeze({
  isIP,
  getStream: () => new PostgresSocket(),
  getSecureStream: unavailable,
  validateTransport(params) {
    if (params.ssl) unavailable();
  },
});
