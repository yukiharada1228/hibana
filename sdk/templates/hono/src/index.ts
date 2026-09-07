import { Hono } from "hono";

const app = new Hono<{ Bindings: { GREETING: string } }>();
app.get("/", c => c.json({ message: c.env.GREETING }));
app.post("/echo", async c => c.body(await c.req.arrayBuffer()));

export default app;
