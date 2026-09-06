import { Hono } from "hono";

// c.env carries [vars] + secrets (Workers-style bindings).
type Bindings = { GREETING: string; API_KEY?: string };

const app = new Hono<{ Bindings: Bindings }>();
app.get("/", (c) => c.text(c.env.GREETING));
app.get("/whoami", (c) => c.json({ hasApiKey: Boolean(c.env.API_KEY) }));
export default app;
