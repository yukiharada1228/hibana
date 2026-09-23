// The same application is built with and without the optional Pool component.
import { Hono } from "hono";
import pg from "pg";

const app = new Hono();
app.get("/", async (c) => {
  const secure = c.env.DB_TLS === "trusted";
  const client = new pg.Client({
    host: c.env.DB_HOST,
    port: Number(c.env.DB_PORT),
    database: c.env.DB_NAME,
    user: c.env.DB_USER,
    password: c.env.DB_PASSWORD,
    ssl: secure ? { ca: c.env.DB_CA } : false,
    channel_binding: secure ? "require" : "disable",
  });
  try {
    await client.connect();
    await client.query("BEGIN READ ONLY");
    const {
      rows: [row],
    } = await client.query(
      "SELECT $1::integer AS answer, $2::text AS greeting",
      [42, "こんにちは 🔥"],
    );
    await client.query("ROLLBACK");
    return c.json({
      ...row,
      hasPool: Object.hasOwn(pg, "Pool"),
      channelBinding: client.channelBindingUsed,
    });
  } finally {
    await client.end();
  }
});
export default app;
