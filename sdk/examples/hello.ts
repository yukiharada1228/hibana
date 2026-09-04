// Hibana の Hono サンプル（Cloudflare Workers と同じ書き味）
// `hibana deploy` でこれを WebAssembly Component にして自前基盤で動かす。
import { Hono } from "hono";

const app = new Hono();

app.get("/", (c) => c.text("Hello from Hono on Hibana 🔥"));

app.get("/hello/:name", (c) => c.json({ hello: c.req.param("name") }));

app.post("/echo", async (c) => {
  const body = await c.req.json();
  return c.json({ youSent: body }, 200);
});

export default app;
