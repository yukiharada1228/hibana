import { Hono } from "hono";
import net from "node:net";
import tls from "node:tls";

const app = new Hono();
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
function exchange(socket, text, secure = false) {
  return new Promise((resolve, reject) => {
    let certificate,
      received = "",
      backpressure = false,
      drained = false;
    socket.setEncoding("utf8");
    socket.on("data", (data) => {
      received += data;
    });
    socket.once("error", reject);
    socket.once("close", () =>
      resolve({
        received,
        backpressure,
        drained,
        bytesRead: socket.bytesRead,
        bytesWritten: socket.bytesWritten,
        authorized: socket.authorized,
        alpn: socket.alpnProtocol,
        certificate,
      }),
    );
    socket.once(secure ? "secureConnect" : "connect", () => {
      if (secure)
        certificate = socket.getPeerCertificate().raw.toString("base64");
      if (!socket.write(text)) {
        backpressure = true;
        socket.once("drain", () => {
          drained = true;
          socket.end();
        });
      } else socket.end();
    });
  });
}
function failure(socket) {
  return new Promise((resolve, reject) => {
    socket.once("secureConnect", () =>
      reject(new Error("Unexpected verified connection")),
    );
    socket.once("error", (e) => resolve({ code: e.code, message: e.message }));
  });
}
async function startTlsSocket(port, options = {}) {
  const raw = net.connect({ port, host: "127.0.0.1", ...options });
  await new Promise((resolve, reject) => {
    raw.once("error", reject);
    raw.once("connect", () => raw.write("STARTTLS\n"));
    raw.once("data", (bytes) => {
      if (bytes.toString() !== "READY\n")
        reject(new Error("Bad STARTTLS reply"));
      else resolve();
    });
  });
  return raw;
}
function rejectInvalidTlsTimeouts(raw) {
  const rejected = [];
  for (const construct of [false, true])
    for (const timeout of [-1, NaN, Infinity, "10", null]) {
      let socket, error;
      try {
        socket = construct
          ? new tls.TLSSocket(raw, { servername: "localhost", timeout })
          : tls.connect({ socket: raw, servername: "localhost", timeout });
      } catch (cause) {
        error = cause.code;
      }
      if (socket) socket.destroy();
      if (error !== "ERR_OUT_OF_RANGE" || raw.readyState !== "open")
        throw new Error("Invalid TLS timeout consumed the raw socket");
      rejected.push(error);
    }
  return rejected;
}
async function echoConnected(socket, text) {
  return new Promise((resolve, reject) => {
    let received = "";
    const onData = (data) => {
      received += data.toString();
      if (received.length >= text.length) {
        socket.off("data", onData);
        socket.off("error", reject);
        resolve(received);
      }
    };
    socket.on("data", onData);
    socket.once("error", reject);
    socket.write(text);
  });
}
async function connectAfterInvalidOptions(port, text, secure, ca) {
  const socket = secure
    ? new tls.TLSSocket(undefined, { servername: "localhost", ca })
    : new net.Socket();
  const rejected = [];
  let rejectedCallbacks = 0;
  try {
    for (const [options, expected] of [
      ...[true, false, [port], {}, null, undefined, 5432n, Symbol("port")].map(
        (port) => [{ port }, "ERR_INVALID_ARG_TYPE"],
      ),
      ...[0, -1, 65536, 1.5, NaN, Infinity, "", "bad"].map((port) => [
        { port },
        "ERR_SOCKET_BAD_PORT",
      ]),
      ...[-1, NaN, Infinity, "10", null].map((timeout) => [
        { timeout },
        "ERR_OUT_OF_RANGE",
      ]),
      ...[-1, 1.5, 0x100000000, NaN, Infinity, "10"].map(
        (keepAliveInitialDelay) => [
          { keepAlive: true, keepAliveInitialDelay, timeout: 1000 },
          "ERR_OUT_OF_RANGE",
        ],
      ),
      [
        { keepAlive: true, keepAliveInitialDelay: 1000, timeout: -1 },
        "ERR_OUT_OF_RANGE",
      ],
    ]) {
      let code;
      try {
        socket.connect(
          { host: "127.0.0.1", port, ...options },
          () => rejectedCallbacks++,
        );
      } catch (error) {
        code = error.code;
      }
      if (
        code !== expected ||
        socket.connecting ||
        socket.destroyed ||
        socket.listenerCount("connect")
      )
        throw new Error(`Invalid options changed the unused socket: ${code}`);
      rejected.push(code);
    }
    const completed = exchange(socket, text, secure);
    socket.connect({ host: "127.0.0.1", port: String(port), timeout: 5000 });
    return { ...(await completed), rejected, rejectedCallbacks };
  } finally {
    socket.destroy();
  }
}
function handshakeEof(socket) {
  return new Promise((resolve, reject) => {
    const errors = [];
    let secured = false;
    const watchdog = setTimeout(() => {
      reject(new Error("TLS EOF did not close the socket"));
      socket.destroy();
    }, 2000);
    socket.once("secureConnect", () => {
      secured = true;
    });
    socket.on("error", (error) => errors.push(error.code));
    socket.once("close", () => {
      clearTimeout(watchdog);
      resolve({ errors, secured, destroyed: socket.destroyed });
    });
  });
}
function passiveClose(socket) {
  return new Promise((resolve, reject) => {
    const events = [];
    let writableAtEnd;
    const watchdog = setTimeout(() => {
      reject(new Error("Peer EOF did not close the unread socket"));
      socket.destroy();
    }, 2000);
    for (const name of ["connect", "secureConnect", "end", "finish"])
      socket.on(name, () => events.push(name));
    socket.on("end", () => {
      writableAtEnd = !socket.writableEnded;
      if (socket.allowHalfOpen) socket.end();
    });
    socket.on("error", (error) => {
      clearTimeout(watchdog);
      reject(error);
      socket.destroy();
    });
    socket.once("close", () => {
      clearTimeout(watchdog);
      events.push("close");
      resolve({ events, writableAtEnd, destroyed: socket.destroyed });
    });
  });
}
async function delayedRead(socket) {
  const closed = passiveClose(socket);
  // Observe rejection immediately even while waiting for the initial buffer.
  closed.catch(() => {});
  try {
    const deadline = Date.now() + 1500;
    while (!socket.readableLength && !socket.destroyed && Date.now() < deadline)
      await pause(1);
    await pause(30);
    const buffered = socket.readableLength;
    const bytesBeforeRead = socket.bytesRead;
    let received = "";
    socket.on("data", (bytes) => {
      received += bytes.toString();
    });
    return { ...(await closed), buffered, bytesBeforeRead, received };
  } finally {
    socket.destroy();
  }
}
app.post("/", async (c) => {
  const p = await c.req.json();
  try {
    let result;
    if (p.test === "tcp")
      result = await exchange(
        net.connect(p.port, p.host || "127.0.0.1"),
        p.text,
      );
    else if (p.test === "tls")
      result = await exchange(
        tls.connect({
          host: "127.0.0.1",
          port: p.port,
          servername: p.servername || "localhost",
          ca: p.ca,
          ALPNProtocols: ["echo/1"],
        }),
        p.text,
        true,
      );
    else if (p.test === "tls-failure")
      result = await failure(
        tls.connect({
          host: "127.0.0.1",
          port: p.port,
          servername: p.servername || "localhost",
          ...(p.ca !== undefined ? { ca: p.ca } : {}),
        }),
      );
    else if (p.test === "tcp-failure")
      result = await failure(
        net.connect({ host: p.host || "127.0.0.1", port: p.port }),
      );
    else if (p.test === "starttls") {
      const raw = await startTlsSocket(p.port);
      result = await exchange(
        tls.connect({ socket: raw, servername: "localhost", ca: p.ca }),
        p.text,
        true,
      );
      result.rawClosed = raw.destroyed;
    } else if (p.test === "connect-options") {
      result = await connectAfterInvalidOptions(p.port, p.text, p.secure, p.ca);
    } else if (p.test === "starttls-half-open") {
      const raw = await startTlsSocket(p.port, { allowHalfOpen: p.halfOpen });
      let socket;
      try {
        const options = {
          servername: "localhost",
          ca: p.ca,
          ...(p.override ? { allowHalfOpen: !p.halfOpen } : {}),
        };
        socket = p.construct
          ? new tls.TLSSocket(raw, options)
          : tls.connect({ socket: raw, ...options });
        result = {
          ...(await passiveClose(socket)),
          allowHalfOpen: socket.allowHalfOpen,
          rawClosed: raw.destroyed,
        };
      } finally {
        socket?.destroy();
        raw.destroy();
      }
    } else if (p.test === "starttls-options") {
      const raw = p.upgrade
        ? await startTlsSocket(p.port)
        : net.connect(p.port, "127.0.0.1");
      try {
        if (!p.upgrade)
          await new Promise((resolve, reject) => {
            raw.once("connect", resolve);
            raw.once("error", reject);
          });
        const rejected = rejectInvalidTlsTimeouts(raw);
        if (p.upgrade) {
          result = await exchange(
            tls.connect({
              socket: raw,
              servername: "localhost",
              ca: p.ca,
              timeout: 5000,
            }),
            p.text,
            true,
          );
          result.rawClosed = raw.destroyed;
        } else result = { received: await echoConnected(raw, p.text) };
        result.rejected = rejected;
      } finally {
        raw.destroy();
      }
    } else if (p.test === "tls-eof") {
      result = [];
      // Exceed both TLS/TCP's 32-resource limits in one invocation. Every failure
      // must release its resources before the next attempt can be made.
      for (let n = 0; n < 40; n++) {
        const raw = p.starttls ? await startTlsSocket(p.port) : undefined;
        const socket = tls.connect({
          ...(raw ? { socket: raw } : { host: "127.0.0.1", port: p.port }),
          servername: "localhost",
        });
        result.push({
          ...(await handshakeEof(socket)),
          rawClosed: raw?.destroyed ?? true,
        });
      }
    } else if (p.test === "passive-eof" || p.test === "delayed-read") {
      result = [];
      for (let n = 0; n < (p.test === "passive-eof" ? 40 : 1); n++) {
        const raw =
          p.transport === "starttls"
            ? await startTlsSocket(p.port, {
                allowHalfOpen: Boolean(p.halfOpen),
              })
            : undefined;
        const options = {
          allowHalfOpen: Boolean(p.halfOpen),
          readableHighWaterMark: 1024,
        };
        const socket =
          p.transport === "tcp"
            ? net.connect({ host: "127.0.0.1", port: p.port, ...options })
            : tls.connect({
                ...(raw
                  ? { socket: raw }
                  : { host: "127.0.0.1", port: p.port }),
                ...options,
                servername: "localhost",
                ca: p.ca,
              });
        result.push({
          ...(await (p.test === "passive-eof"
            ? passiveClose(socket)
            : delayedRead(socket))),
          rawClosed: raw?.destroyed ?? true,
        });
      }
    } else if (p.test === "timeout" || p.test === "tls-timeout") {
      result = await new Promise((resolve, reject) => {
        const socket =
          p.test === "tls-timeout"
            ? tls.connect({
                host: "127.0.0.1",
                port: p.port,
                servername: "localhost",
              })
            : net.connect(p.port, "127.0.0.1");
        socket.once("error", reject);
        socket.setTimeout(60, () => {
          const closedByTimeout = socket.destroyed;
          socket.once("close", () =>
            resolve({ closedByTimeout, closedAfterDestroy: socket.destroyed }),
          );
          socket.destroy();
        });
      });
    } else if (p.test === "parallel") {
      let timerRan = false;
      const timer = pause(10).then(() => {
        timerRan = true;
      });
      const sockets = await Promise.all(
        Array.from({ length: 8 }, (_, n) =>
          exchange(net.connect(p.port, "127.0.0.1"), `${n}:日本語🔥`),
        ),
      );
      result = { sockets, timerRanBeforeCompletion: timerRan };
      await timer;
    } else if (p.test === "limit") {
      const sockets = Array.from({ length: 32 }, () =>
        net.connect(p.port, "127.0.0.1"),
      );
      sockets.forEach((socket) => socket.on("error", () => {}));
      try {
        result = await failure(net.connect(p.port, "127.0.0.1"));
      } finally {
        sockets.forEach((socket) => socket.destroy());
      }
    } else if (p.test === "repeated") {
      for (let n = 0; n < 80; n++)
        await exchange(net.connect(p.port, "127.0.0.1"), "x");
      result = { count: 80 };
    } else if (p.test === "cancel") {
      const socket = net.connect(p.port, "127.0.0.1");
      let errors = 0,
        closes = 0;
      socket.on("error", () => {
        errors++;
      });
      socket.on("close", () => {
        closes++;
      });
      socket.write("x".repeat(1024 * 1024), () => {});
      socket.destroy();
      socket.destroy();
      await pause(20);
      result = { destroyed: socket.destroyed, errors, closes };
    } else if (p.test === "options") {
      const rejected = [];
      for (const call of [
        () => tls.connect({ port: 443, rejectUnauthorized: false }),
        () => tls.connect({ port: 443, checkServerIdentity() {} }),
        () => net.connect({ path: "/tmp/no-socket" }),
        () => new net.Socket().setNoDelay(),
      ]) {
        try {
          call();
          rejected.push(false);
        } catch (e) {
          rejected.push(e.code);
        }
      }
      result = {
        rejected,
        ips: ["127.0.0.1", "::1", "bad", "01.2.3.4"].map(net.isIP),
      };
    } else throw new Error("Unknown test");
    return c.json(result);
  } catch (e) {
    return c.json({ error: e.message, code: e.code, stack: e.stack }, 500);
  }
});
export default app;
