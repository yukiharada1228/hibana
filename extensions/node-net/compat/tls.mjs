import {
  Socket,
  normalizeConnect,
  adopt,
  error,
  unsupported,
  checkOptions,
  baseOptions,
  connectOptions,
} from "./socket.mjs";
import { Buffer } from "buffer/";

const allowed = [
  ...connectOptions,
  "socket",
  "servername",
  "ca",
  "ALPNProtocols",
  "rejectUnauthorized",
];
function tlsOptions(options, host) {
  if (
    options.rejectUnauthorized !== undefined &&
    options.rejectUnauthorized !== true
  ) {
    throw error(
      "ERR_NOT_SUPPORTED",
      "TLS certificate verification is mandatory",
    );
  }
  const name = options.servername ?? host;
  if (typeof name !== "string" || !name.length)
    throw error(
      "ERR_INVALID_ARG_VALUE",
      "A servername or host is required for certificate verification",
    );
  let caPem;
  if (options.ca !== undefined) {
    const certificates = Array.isArray(options.ca) ? options.ca : [options.ca];
    caPem = certificates
      .map((cert) => {
        if (typeof cert === "string") return cert;
        if (cert instanceof Uint8Array)
          return Buffer.from(cert).toString("utf8");
        throw error("ERR_INVALID_ARG_TYPE", "CA must be PEM text or bytes");
      })
      .join("\n");
  }
  const alpn = options.ALPNProtocols ?? [];
  if (
    !Array.isArray(alpn) ||
    alpn.some((p) => typeof p !== "string" || !/^[\x21-\x7e]+$/.test(p))
  ) {
    throw error(
      "ERR_NOT_SUPPORTED",
      "ALPNProtocols must be an array of ASCII protocol names",
    );
  }
  return { serverName: name, caPem, alpn };
}
export class TLSSocket extends Socket {
  constructor(socket, options = {}) {
    checkOptions(options, allowed);
    super(
      Object.fromEntries(
        baseOptions
          .filter((key) => key in options)
          .map((key) => [key, options[key]]),
      ),
    );
    this.encrypted = true;
    this.authorized = false;
    this.authorizationError = null;
    this.alpnProtocol = false;
    this._tlsOptions = tlsOptions(
      options,
      socket?._host ?? options.host ?? "localhost",
    );
    // Validation precedes ownership transfer so failure leaves the raw socket intact.
    if (socket) adopt(socket, this);
  }
  _destroy(cause, callback) {
    if (!this.authorized && cause)
      this.authorizationError = cause.code || cause.message;
    super._destroy(cause, callback);
  }
  getPeerCertificate() {
    unsupported("Peer certificate introspection");
  }
  renegotiate() {
    unsupported("TLS renegotiation");
  }
  getSession() {
    unsupported("TLS session export");
  }
}
export function connect(...args) {
  const { options, callback } = normalizeConnect(args);
  checkOptions(options, allowed);
  const socket = new TLSSocket(options.socket, options);
  if (callback) socket.once("secureConnect", callback);
  if (options.socket) {
    if (options.timeout !== undefined) socket.setTimeout(options.timeout);
  } else {
    const plain = Object.fromEntries(
      connectOptions
        .filter((key) => key in options)
        .map((key) => [key, options[key]]),
    );
    socket.connect(plain);
  }
  return socket;
}
export function createServer() {
  unsupported("TLS servers");
}
export function createSecureContext() {
  unsupported("Reusable TLS contexts");
}
export default { connect, TLSSocket, createServer, createSecureContext };
