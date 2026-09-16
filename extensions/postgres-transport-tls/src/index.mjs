import { tcp } from "@hibana/postgres-transport-tcp";
import { connect } from "tls";

// Support PostgreSQL's SSLRequest upgrade while preserving TCP for ssl:false.
// The core validates SSL options and the TLS implementation verifies certificates.
export const tcpTls = Object.freeze({
  ...tcp,
  getSecureStream: connect,
  validateTransport() {},
});
