// The TLS engine owns protocol state; the TCP component owns the network socket.
// Keep at most one 16 KiB ciphertext chunk between them, preserving backpressure.
import { createClient } from "@hibana/tls";

const dispose = Symbol.dispose || Symbol.for("dispose");
export class TlsHandle {
  constructor(tcp, options) {
    this.tcp = tcp;
    this.options = options;
    this.session = null;
    this.output = null;
    this.offset = 0;
    this.eof = false;
    this.ending = false;
    this.tcpEnded = false;
  }
  flush() {
    if (!this.output) {
      this.output = this.session.takeOutput();
      this.offset = 0;
    }
    if (this.output) {
      this.offset += this.tcp.write(this.output.subarray(this.offset));
      if (this.offset === this.output.length) this.output = null;
    }
  }
  status() {
    const tcp = this.tcp.status();
    if (!tcp.connected) return { ...tcp, secure: false };
    this.session ??= createClient(this.options);
    this.flush();
    if (!this.eof && this.session.status().wantsInput) {
      const input = this.tcp.read();
      if (input != null) {
        if (input.length) this.session.receive(input);
        else {
          this.eof = true;
          this.session.receiveEof();
        }
      }
    }
    this.flush();
    const tls = this.session.status();
    if (this.ending && !this.tcpEnded && !this.output && !tls.wantsOutput) {
      this.tcp.end();
      this.tcpEnded = true;
    }
    return { ...tcp, secure: tls.secure, alpn: tls.alpn };
  }
  read() {
    return this.session?.read();
  }
  write(data) {
    return this.session ? this.session.write(data) : 0;
  }
  end() {
    this.ending = true;
    this.session?.end();
  }
  keepAlive(...args) {
    return this.tcp.keepAlive(...args);
  }
  peerCertificate() {
    return this.session?.peerCertificate();
  }
  close() {
    try {
      this.session?.close();
      this.session?.[dispose]?.();
    } finally {
      this.session = null;
      this.output = null;
      try {
        this.tcp?.close();
      } finally {
        this.tcp?.[dispose]?.();
        this.tcp = null;
      }
    }
  }
}
