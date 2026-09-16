// A disposable TLS PostgreSQL peer that offers weaker or out-of-order authentication.
// It never connects to a database and only counts credential/query frames.
import assert from "node:assert/strict";
import { createServer } from "node:net";
import { createSecureContext, TLSSocket } from "node:tls";

function authentication(code, payload = Buffer.alloc(0)) {
  const frame = Buffer.alloc(9 + payload.length);
  frame[0] = 82;
  frame.writeUInt32BE(8 + payload.length, 1);
  frame.writeUInt32BE(code, 5);
  payload.copy(frame, 9);
  return frame;
}
const scram = authentication(10, Buffer.from("SCRAM-SHA-256\0\0"));
const md5 = authentication(5, Buffer.from([1, 2, 3, 4]));
const cleartext = authentication(3),
  ok = authentication(0);
const ready = Buffer.from([90, 0, 0, 0, 5, 73]);
const exchanges = {
  scram: [scram],
  cleartext: [cleartext],
  md5: [md5],
  trust: [ok, ready],
  "early-cleartext": [cleartext, ok, ready],
  "early-md5": [md5, ok, ready],
  "duplicate-md5": [md5, md5],
  "early-scram-continuation": [
    scram,
    authentication(11, Buffer.from("fixture challenge")),
  ],
  "early-scram-final": [
    scram,
    authentication(12, Buffer.from("fixture proof")),
  ],
};

export async function authenticationFixture({ cert, key }) {
  const context = createSecureContext({ cert, key });
  const sockets = new Set();
  const fixture = { mode: "scram", credentialOrQueryMessages: 0 };
  function track(socket) {
    sockets.add(socket);
    socket.on("error", () => {});
    socket.once("close", () => sockets.delete(socket));
  }
  const server = createServer((socket) => {
    track(socket);
    socket.once("data", (request) => {
      assert.equal(request.toString("hex"), "0000000804d2162f");
      socket.write("S", () => {
        const secure = new TLSSocket(socket, {
          isServer: true,
          secureContext: context,
        });
        track(secure);
        let buffer = Buffer.alloc(0),
          started = false;
        secure.on("data", (data) => {
          buffer = Buffer.concat([buffer, data]);
          if (!started) {
            if (buffer.length < 4 || buffer.length < buffer.readUInt32BE(0))
              return;
            buffer = buffer.subarray(buffer.readUInt32BE(0));
            started = true;
            assert.ok(
              exchanges[fixture.mode],
              "Unknown authentication fixture mode",
            );
            secure.write(Buffer.concat(exchanges[fixture.mode]));
          }
          while (
            buffer.length >= 5 &&
            buffer.length >= buffer.readUInt32BE(1) + 1
          ) {
            if ([112, 81, 80].includes(buffer[0]))
              fixture.credentialOrQueryMessages++;
            buffer = buffer.subarray(buffer.readUInt32BE(1) + 1);
          }
        });
      });
    });
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  fixture.port = server.address().port;
  fixture.close = async () => {
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => server.close(resolve));
  };
  return fixture;
}
