import { Duplex } from "@hibana/node-stream/duplex";
import { Buffer } from "node:buffer";
import { ipVersion } from "@hibana/tcp";
import { Transport } from "./transport.mjs";

const CHUNK = 16 * 1024;
const active = new Set();
let timer,
  delay = 1;
export const error = (code, message) =>
  Object.assign(new Error(message), { code });
const asError = (value) => {
  if (value?.payload)
    return error(
      value.payload.code || "EIO",
      value.payload.message || String(value.payload),
    );
  return value instanceof Error
    ? value
    : error(value?.code || "EIO", value?.message || String(value));
};
export function unsupported(name) {
  throw error(
    "ERR_METHOD_NOT_IMPLEMENTED",
    `${name} is not provided by @hibana/node-net`,
  );
}
export function checkOptions(options, allowed) {
  for (const name of Object.keys(options)) {
    if (!allowed.includes(name))
      throw error("ERR_NOT_SUPPORTED", `Unsupported socket option: ${name}`);
  }
}

// Component imports are synchronous. Each call does bounded, nonblocking work;
// one shared timer returns control to the JS engine between readiness checks.
function schedule() {
  if (timer === undefined && active.size) timer = setTimeout(tick, delay);
}
function tick() {
  timer = undefined;
  let progress = false;
  for (const socket of [...active]) {
    try {
      progress = socket._step() || progress;
    } catch (e) {
      socket.destroy(asError(e));
      progress = true;
    }
  }
  delay = progress ? 1 : Math.min(delay * 2, 8);
  schedule();
}
function activate(socket) {
  active.add(socket);
  delay = 1;
  schedule();
}
function deactivate(socket) {
  active.delete(socket);
  if (!active.size && timer !== undefined) {
    clearTimeout(timer);
    timer = undefined;
  }
}

const baseOptions = [
  "allowHalfOpen",
  "highWaterMark",
  "readableHighWaterMark",
  "writableHighWaterMark",
];
const connectOptions = [
  "host",
  "port",
  "timeout",
  "keepAlive",
  "keepAliveInitialDelay",
  ...baseOptions,
];
export function normalizeConnect(args) {
  const values = [...args];
  const callback =
    typeof values.at(-1) === "function" ? values.pop() : undefined;
  let options;
  if (typeof values[0] === "object" && values[0] !== null) {
    options = { ...values.shift() };
  } else {
    const port = values.shift();
    // Numeric string ports are accepted; paths/IPC are deliberately unsupported.
    if (typeof port === "string" && !/^\d+$/.test(port))
      unsupported("IPC sockets");
    options = { port };
    if (typeof values[0] === "string") options.host = values.shift();
    if (values[0] && typeof values[0] === "object")
      Object.assign(options, values.shift());
  }
  if (values.length)
    throw error("ERR_INVALID_ARG_VALUE", "Unexpected connect arguments");
  return { options, callback };
}
export function destination(options) {
  const port = Number(options.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535)
    throw error("ERR_SOCKET_BAD_PORT", "Port must be 1..65535");
  const host = options.host ?? "localhost";
  if (typeof host !== "string" || !host.length)
    throw error("ERR_INVALID_ARG_TYPE", "Host must be a nonempty string");
  return { host, port };
}

export class Socket extends Duplex {
  constructor(options = {}) {
    checkOptions(options, baseOptions);
    super({
      ...options,
      allowHalfOpen: options.allowHalfOpen ?? false,
      autoDestroy: true,
      emitClose: true,
    });
    this._handle = null;
    this._started = false;
    this._connected = false;
    this._reading = false;
    this._readEnded = false;
    this._pendingWrite = null;
    this._pendingFinal = null;
    this._detached = false;
    this._timeout = 0;
    this._lastActivity = Date.now();
    this._timeoutEmitted = false;
    this.bytesRead = 0;
    this.bytesWritten = 0;
    this.connecting = false;
    this.pending = true;
  }
  connect(...args) {
    const { options, callback } = normalizeConnect(args);
    checkOptions(options, connectOptions);
    if (this._started || this.destroyed)
      throw error(
        "ERR_SOCKET_CONNECTING",
        "Create a new Socket for each connection",
      );
    const { host, port } = destination(options);
    if (callback) this.once("connect", callback);
    this._started = true;
    this.connecting = true;
    this._host = host;
    if (
      options.keepAlive !== undefined ||
      options.keepAliveInitialDelay !== undefined
    )
      this.setKeepAlive(
        options.keepAlive ?? false,
        options.keepAliveInitialDelay ?? 0,
      );
    if (options.timeout !== undefined) this.setTimeout(options.timeout);
    try {
      this._handle = this._wrapHandle(new Transport(host, port));
      activate(this);
    } catch (e) {
      Promise.resolve().then(() => this.destroy(asError(e)));
    }
    return this;
  }
  _wrapHandle(handle) {
    return handle;
  }
  _canReadWrite() {
    return this._connected;
  }
  _activity() {
    this._lastActivity = Date.now();
    this._timeoutEmitted = false;
  }
  // Poll one bounded unit of work. Event listeners can destroy or upgrade the
  // socket, so recheck ownership after every callback before touching the handle.
  _step() {
    if (!this._handle || this.destroyed) return false;
    const state = this._handle.status();
    let progress = this._advanceConnection(state);
    if (!this._handle || this.destroyed) return true;

    if (this._canReadWrite()) {
      progress = this._flushWrite() || progress;
      if (!this._handle || this.destroyed) return true;
      progress = this._finishWrite(state) || progress;
      if (!this._handle || this.destroyed) return true;
      progress = this._pullRead() || progress;
    }
    if (
      this._timeout > 0 &&
      !this._timeoutEmitted &&
      Date.now() - this._lastActivity >= this._timeout
    ) {
      this._timeoutEmitted = true;
      this.emit("timeout");
    }
    return progress;
  }

  _advanceConnection(state) {
    let progress = false;
    if (state.connected && !this._connected) {
      this._connected = true;
      this.connecting = false;
      this.pending = false;
      this._activity();
      progress = true;
      if (this._keepAlive) this._handle.keepAlive(...this._keepAlive);
      this.emit("connect");
      if (!this._handle || this.destroyed) return true;
      // Start bounded buffering without switching the stream into flowing mode.
      // Even a caller that only waits for close must observe a peer's EOF.
      this.read(0);
    }
    return progress;
  }

  _flushWrite() {
    const write = this._pendingWrite;
    if (!write) return false;
    const bytes = write.bytes.subarray(write.offset, write.offset + CHUNK);
    const written = this._handle.write(bytes);
    if (written > 0) {
      write.offset += written;
      this.bytesWritten += written;
      this._activity();
    }
    if (write.offset === write.bytes.length) {
      this._pendingWrite = null;
      write.callback();
    }
    return written > 0;
  }

  _finishWrite(state) {
    if (!this._pendingFinal || this._pendingWrite) return false;
    if (!this._endRequested) {
      this._handle.end();
      this._endRequested = true;
      return true;
    }
    if (state.writeClosed) {
      const callback = this._pendingFinal;
      this._pendingFinal = null;
      callback();
      return true;
    }
    return false;
  }

  _pullRead() {
    let progress = false;
    // push(false) propagates readable backpressure to the transport.
    for (let i = 0; i < 4 && this._reading && !this._readEnded; i++) {
      const bytes = this._handle.read();
      if (bytes === undefined || bytes === null) break;
      progress = true;
      if (!bytes.length) {
        this._readEnded = true;
        this._reading = false;
        this.push(null);
        // A non-flowing stream still needs to finish when its buffer is empty.
        // read(0) preserves any buffered data until the caller consumes it.
        this.read(0);
        break;
      }
      this.bytesRead += bytes.length;
      this._activity();
      if (!this.push(Buffer.from(bytes))) this._reading = false;
      if (!this._handle || this.destroyed) break;
    }
    return progress;
  }
  _read() {
    this._reading = true;
    if (this._handle) activate(this);
  }
  _write(chunk, encoding, callback) {
    if (this._detached)
      return callback(
        error(
          "ERR_SOCKET_CLOSED",
          "Socket ownership transferred to another transport",
        ),
      );
    this._pendingWrite = {
      bytes: Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding),
      offset: 0,
      callback,
    };
    if (this._handle) activate(this);
  }
  _final(callback) {
    if (this._detached)
      return callback(
        error(
          "ERR_SOCKET_CLOSED",
          "Socket ownership transferred to another transport",
        ),
      );
    this._pendingFinal = callback;
    if (this._handle) activate(this);
  }
  _destroy(cause, callback) {
    deactivate(this);
    try {
      this._handle?.close();
      this._handle?.[Symbol.dispose || Symbol.for("dispose")]?.();
    } catch (e) {
      cause ??= asError(e);
    }
    this._handle = null;
    this.connecting = false;
    this.pending = false;
    const write = this._pendingWrite,
      final = this._pendingFinal;
    this._pendingWrite = null;
    this._pendingFinal = null;
    const closed = cause || error("ERR_SOCKET_CLOSED", "Socket was destroyed");
    write?.callback(closed);
    final?.(closed);
    callback(cause);
  }
  // Transfer a drained connection to another transport without leaving two owners.
  _transferTo(target) {
    if (
      !this._connected ||
      this.destroyed ||
      this._detached ||
      this.writableEnded ||
      this._readEnded ||
      this.readableLength ||
      this.writableLength ||
      this._pendingWrite
    ) {
      throw error(
        "ERR_SOCKET_INVALID_STATE",
        "Transfer requires an open connection with drained buffers",
      );
    }
    const handle = target._wrapHandle(this._handle);
    deactivate(this);
    target._handle = handle;
    this._handle = null;
    this._detached = true;
    target._host = this._host;
    target._started = true;
    target.pending = false;
    activate(target);
  }

  setTimeout(ms, callback) {
    if (!Number.isFinite(ms) || ms < 0)
      throw error("ERR_OUT_OF_RANGE", "Timeout must be a nonnegative number");
    this._timeout = ms;
    this._activity();
    if (callback) this.once("timeout", callback);
    return this;
  }
  setKeepAlive(enable = false, initialDelay = 0) {
    if (
      !Number.isInteger(initialDelay) ||
      initialDelay < 0 ||
      initialDelay > 0xffffffff
    )
      throw error("ERR_OUT_OF_RANGE", "Invalid keepalive delay");
    this._keepAlive = [Boolean(enable), initialDelay];
    if (this._connected && this._handle) {
      try {
        this._handle.keepAlive(...this._keepAlive);
      } catch (e) {
        this.destroy(asError(e));
      }
    }
    return this;
  }
  setNoDelay() {
    unsupported("TCP_NODELAY (unavailable in WASI 0.2)");
  }
  ref() {
    unsupported("Process lifetime control");
  }
  unref() {
    unsupported("Process lifetime control");
  }
  address() {
    unsupported("Local socket address introspection");
  }
  get readyState() {
    if (this.connecting) return "opening";
    if (this.destroyed || this._detached) return "closed";
    if (this.readable && this.writable) return "open";
    return this.readable ? "readOnly" : this.writable ? "writeOnly" : "closed";
  }
}

export const isIP = (input) =>
  typeof input === "string" ? ipVersion(input) : 0;
export const isIPv4 = (input) => isIP(input) === 4;
export const isIPv6 = (input) => isIP(input) === 6;
export { baseOptions, connectOptions };
