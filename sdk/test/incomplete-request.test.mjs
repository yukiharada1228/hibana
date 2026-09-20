import assert from "node:assert/strict";
import test from "node:test";
import { createServer } from "node:http";
import { setTimeout as delay } from "node:timers/promises";
import { incompleteRequest } from "../scripts/incomplete-request.mjs";

async function fixture(t, handler) {
  const server = createServer();
  server.on("checkContinue", handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const sockets = [];
  t.after(async () => {
    for (const socket of sockets) socket.destroy();
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  });
  return { port: server.address().port, sockets };
}

test("slow upload waits for admission and keeps the final response separate from 100 Continue", async (t) => {
  let allowAdmission, connected;
  const received = new Promise((resolve) => {
    connected = resolve;
  });
  let bytes = 0;
  const { port, sockets } = await fixture(t, (req, res) => {
    allowAdmission = () => res.writeContinue();
    req.on("data", (data) => {
      bytes += data.length;
      res.writeHead(408);
      res.end();
    });
    connected();
  });
  let admitted = false;
  const pending = incompleteRequest(port, sockets).then((result) => {
    admitted = true;
    return result;
  });
  await received;
  await delay(30);
  assert.equal(admitted, false);
  assert.equal(bytes, 0);
  allowAdmission();
  assert.equal(await (await pending).status, 408);
  assert.equal(bytes, 1);
});

test("a busy slot is retried until the incomplete upload is actually admitted", async (t) => {
  let attempts = 0;
  const { port, sockets } = await fixture(t, (req, res) => {
    if (++attempts === 1) {
      res.writeHead(503);
      res.end();
      return;
    }
    res.writeContinue();
    req.once("data", () => {
      res.writeHead(408);
      res.end();
    });
  });
  const upload = await incompleteRequest(port, sockets);
  assert.equal(await upload.status, 408);
  assert.equal(attempts, 2);
});

test("unexpected final responses before admission fail instead of being counted as pending", async (t) => {
  const { port, sockets } = await fixture(t, (_req, res) => {
    res.writeHead(200);
    res.end();
  });
  await assert.rejects(
    incompleteRequest(port, sockets),
    /not admitted \(HTTP 200\)/,
  );
});

test("missing admission has a bounded deadline and closes its socket", async (t) => {
  const { port, sockets } = await fixture(t, () => {});
  const started = performance.now();
  await assert.rejects(
    incompleteRequest(port, sockets, { admissionTimeoutMs: 100 }),
    /closed\/timeout|deadline/,
  );
  assert.ok(performance.now() - started < 2000);
  assert.ok(sockets.every((socket) => socket.destroyed));
});
