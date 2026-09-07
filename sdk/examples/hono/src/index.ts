import { Hono } from "hono";
const app = new Hono<{ Bindings: { GREETING: string; TEST_SECRET?: string } }>();
app.get("/", c => c.json({ message: c.env.GREETING }));
app.post("/echo", async c => c.body(await c.req.arrayBuffer()));
app.get("/headers", c => c.json({ envHeader: c.req.header("x-hibana-env") ?? null, eventHeader: c.req.header("x-hibana-event") ?? null, greeting: c.env.GREETING }));
app.get("/secret", c => c.json({ configured: c.env.TEST_SECRET === "hibana-test-secret" }));
app.get("/stream", c => {
  c.executionCtx.waitUntil(new Promise(resolve => setTimeout(resolve, 100)));
  const stream = new ReadableStream({ async start(controller) {
    controller.enqueue(new TextEncoder().encode("data: first\n\n"));
    await new Promise(resolve => setTimeout(resolve, 200));
    controller.enqueue(new TextEncoder().encode("data: second\n\n"));
    controller.close();
  } });
  return new Response(stream, { headers: { "content-type": "text/event-stream" } });
});
export default app;
