import {
  Socket,
  normalizeConnect,
  error,
  checkOptions,
  baseOptions,
  connectOptions,
} from "@hibana/node-net/socket";
import { Buffer } from "node:buffer";
import { TlsHandle } from "./handle.mjs";

function unsupported(name) {
  throw error(
    "ERR_METHOD_NOT_IMPLEMENTED",
    `${name} is not provided by @hibana/node-tls`,
  );
}

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
    if (socket && (!(socket instanceof Socket) || socket.encrypted))
      throw error(
        "ERR_TLS_INVALID_STATE",
        "STARTTLS requires a plain TCP socket",
      );
    super({
      ...Object.fromEntries(
        baseOptions
          .filter((key) => key in options)
          .map((key) => [key, options[key]]),
      ),
      // Match Node: an adopted socket retains its half-open policy, including
      // when TLS options contain a different value.
      allowHalfOpen: socket ? socket.allowHalfOpen : options.allowHalfOpen,
    });
    this._secure = false;
    this.encrypted = true;
    this.authorized = false;
    this.authorizationError = null;
    this.alpnProtocol = false;
    this._tlsOptions = tlsOptions(
      options,
      socket?._host ?? options.host ?? "localhost",
    );
    // Validation precedes ownership transfer so failure leaves the raw socket intact.
    if (socket) {
      if (options.timeout !== undefined) this.setTimeout(options.timeout);
      socket._transferTo(this);
      this._rawSocket = socket;
      socket.once("close", () => {
        if (!this.destroyed) this.destroy();
      });
    }
  }
  _wrapHandle(handle) {
    return new TlsHandle(handle, this._tlsOptions);
  }
  _canReadWrite() {
    return this._connected && this._secure;
  }
  _advanceConnection(state) {
    let progress = super._advanceConnection(state);
    if (!this._handle || this.destroyed) return progress;
    if (state.secure && !this._secure) {
      this._secure = true;
      this.authorized = true;
      this.alpnProtocol = state.alpn || false;
      this._activity();
      progress = true;
      this.emit("secureConnect");
    }
    return progress;
  }
  _destroy(cause, callback) {
    if (!this.authorized && cause)
      this.authorizationError = cause.code || cause.message;
    // Deliver certificate/protocol errors before the adopted socket's EOF event.
    try {
      super._destroy(cause, callback);
    } finally {
      if (this._rawSocket && !this._rawSocket.destroyed)
        this._rawSocket.destroy();
    }
  }
  getPeerCertificate(detailed = false) {
    if (detailed) unsupported("Detailed peer certificate chains");
    if (this.destroyed || !this._handle) return null;
    if (!this.authorized) return {};
    const raw = this._handle.peerCertificate();
    return raw ? { raw: Buffer.from(raw) } : {};
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
  if (!options.socket) {
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
