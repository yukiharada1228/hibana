// Node's hostname-aware transport composes two independent WASI components.
import { connect, ipVersion } from "@hibana/tcp";
import { lookup } from "@hibana/dns";

function dispose(resource) {
  if (!resource) return;
  try {
    resource.close();
  } finally {
    resource[Symbol.dispose || Symbol.for("dispose")]?.();
  }
}

export class Transport {
  constructor(host, port) {
    this.port = port;
    this.socket = null;
    this.query = null;
    this.lastError = null;
    if (ipVersion(host)) this.socket = connect(host, port);
    else this.query = lookup(host);
  }
  status() {
    if (this.socket) {
      try {
        const state = this.socket.status();
        if (state.connected) {
          dispose(this.query);
          this.query = null;
          this.lastError = null;
        }
        return state;
      } catch (error) {
        dispose(this.socket);
        this.socket = null;
        if (!this.query) throw error;
        this.lastError = error;
      }
    }
    const answer = this.query.next();
    if (answer.done) {
      dispose(this.query);
      this.query = null;
      throw (
        this.lastError ||
        Object.assign(new Error("No usable address"), { code: "ENOTFOUND" })
      );
    }
    if (answer.address) {
      try {
        this.socket = connect(answer.address, this.port);
      } catch (error) {
        this.lastError = error;
      }
    }
    return { connected: false, writeClosed: false };
  }
  read() {
    return this.socket?.read();
  }
  write(data) {
    return this.socket?.write(data) ?? 0;
  }
  end() {
    return this.socket.end();
  }
  keepAlive(...args) {
    return this.socket.keepAlive(...args);
  }
  close() {
    try {
      dispose(this.query);
    } finally {
      this.query = null;
      try {
        dispose(this.socket);
      } finally {
        this.socket = null;
      }
    }
  }
}
