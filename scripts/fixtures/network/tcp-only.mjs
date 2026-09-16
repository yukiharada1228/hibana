import { Hono } from "hono";
import { connect } from "@hibana/tcp";

const app = new Hono();
app.post("/", async (c) => {
  const { port } = await c.req.json();
  const socket = connect("127.0.0.1", port);
  const bytes = new TextEncoder().encode("TCP only: 日本語🔥");
  const output = [];
  const deadline = Date.now() + 5000;
  let offset = 0,
    ending = false;
  try {
    while (Date.now() < deadline) {
      const state = socket.status();
      if (state.connected) {
        if (offset < bytes.length)
          offset += socket.write(bytes.subarray(offset));
        else if (!ending) {
          socket.end();
          ending = true;
        }
        const input = socket.read();
        if (input) {
          if (!input.length)
            return c.json({
              received: new TextDecoder().decode(Uint8Array.from(output)),
            });
          output.push(...input);
        }
      }
      await new Promise((resolve) => setTimeout(resolve, 1));
    }
    throw new Error("TCP deadline exceeded");
  } finally {
    socket.close();
    socket[Symbol.dispose || Symbol.for("dispose")]?.();
  }
});
export default app;
