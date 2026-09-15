import { Hono } from "hono";
import net from "node:net";
import tls from "node:tls";

const app = new Hono();
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
function exchange(socket, text, secure = false) {
  return new Promise((resolve, reject) => {
    let received = "",
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
      }),
    );
    socket.once(secure ? "secureConnect" : "connect", () => {
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
      const raw = net.connect(p.port, "127.0.0.1");
      await new Promise((resolve, reject) => {
        raw.once("error", reject);
        raw.once("connect", () => raw.write("STARTTLS\n"));
        raw.once("data", (bytes) => {
          if (bytes.toString() !== "READY\n")
            reject(new Error("Bad STARTTLS reply"));
          else resolve();
        });
      });
      result = await exchange(
        tls.connect({ socket: raw, servername: "localhost", ca: p.ca }),
        p.text,
        true,
      );
      result.rawClosed = raw.destroyed;
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
