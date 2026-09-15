import { Hono } from "hono";
import pg from "pg";
import { drizzle } from "drizzle-orm/node-postgres";
import { eq } from "drizzle-orm";
import { entries } from "./schema.mjs";

const app = new Hono();
function configuration(c) {
  let ssl = false;
  if (c.env.DB_TLS === "trusted") ssl = { ca: c.env.DB_CA };
  if (c.env.DB_TLS === "untrusted") ssl = true;
  if (c.env.DB_TLS === "wronghost")
    ssl = { ca: c.env.DB_CA, servername: "wrong.invalid" };
  if (c.env.DB_TLS === "insecure") ssl = { rejectUnauthorized: false };
  return {
    host: c.env.DB_HOST,
    port: Number(c.env.DB_PORT),
    database: c.env.DB_NAME,
    user: c.env.DB_USER,
    password: c.env.DB_PASSWORD,
    connectionTimeoutMillis: 3000,
    query_timeout: 3000,
    ssl,
  };
}
async function connected(c, action, overrides = {}) {
  const client = new pg.Client({ ...configuration(c), ...overrides });
  try {
    await client.connect();
    return await action(client);
  } finally {
    await client.end();
  }
}
app.onError((error, c) =>
  c.json({ code: error.code, error: error.message }, 500),
);

app.get("/", (c) =>
  connected(c, async (client) => {
    const { rows } = await client.query(
      "SELECT $1::integer AS answer, $2::text AS greeting, $3::jsonb AS document, $4::bytea AS bytes",
      [
        42,
        "こんにちは Hibana 🔥",
        { value: "json" },
        Buffer.from([0, 1, 128, 255]),
      ],
    );
    const {
      rows: [security],
    } = await client.query(
      "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    );
    return c.json({ ...rows[0], bytes: [...rows[0].bytes], ssl: security.ssl });
  }),
);

app.get("/callback", (c) =>
  connected(c, async (client) => {
    const result = await new Promise((done, reject) =>
      client.query(
        {
          text: "SELECT $1::integer AS answer",
          values: [42],
          rowMode: "array",
        },
        (error, value) => (error ? reject(error) : done(value)),
      ),
    );
    return c.json(result.rows);
  }),
);

app.get("/prepared", (c) =>
  connected(c, async (client) => {
    const values = [];
    for (const value of [1, 2, 3]) {
      const result = await client.query({
        name: "probe",
        text: "SELECT $1::integer AS value",
        values: [value],
      });
      values.push(result.rows[0].value);
    }
    return c.json(values);
  }),
);

app.get("/sql-error", (c) =>
  connected(c, async (client) => {
    let code;
    try {
      await client.query("SELECT * FROM missing_probe_table");
    } catch (error) {
      code = error.code;
    }
    const { rows } = await client.query("SELECT 42 AS answer");
    return c.json({ code, ...rows[0] });
  }),
);

app.get("/timeout", (c) =>
  connected(
    c,
    async (client) => {
      await client.query("SELECT pg_sleep(2)");
      return c.json({ unexpected: "query completed" });
    },
    { query_timeout: 60 },
  ),
);

app.get("/pool", async (c) => {
  const pool = new pg.Pool({ ...configuration(c), max: 2 });
  try {
    const results = await Promise.all(
      Array.from({ length: 8 }, (_, i) =>
        pool.query("SELECT $1::integer AS value", [i]),
      ),
    );
    return c.json(results.map((result) => result.rows[0].value));
  } finally {
    await pool.end();
  }
});

app.get("/repeat", async (c) => {
  for (let i = 0; i < 40; i++)
    await connected(c, (client) => client.query("SELECT 1"));
  return c.json({ connections: 40 });
});

app.get("/password-provider", (c) => {
  const config = configuration(c);
  return connected(
    c,
    async (client) =>
      c.json((await client.query("SELECT 42 AS answer")).rows[0]),
    {
      password: async (params) => {
        if (params.user !== config.user)
          throw new Error("Password provider lost connection parameters");
        return config.password;
      },
    },
  );
});

app.get("/options", (c) => {
  const config = configuration(c);
  const cases = {
    password: () => new pg.Client({ ...config, password: undefined }),
    channelBinding: () =>
      new pg.Client({ ...config, enableChannelBinding: true }),
    verification: () =>
      new pg.Client({ ...config, ssl: { rejectUnauthorized: false } }),
    clientCertificate: () =>
      new pg.Client({ ...config, ssl: { cert: "certificate" } }),
    filesystem: () =>
      new pg.Client({
        connectionString:
          "postgres://probe:probe@localhost/probe?sslrootcert=/host/secret.pem",
      }),
    scramLimit: () => new pg.Client({ ...config, scramMaxIterations: 0 }),
    native: () => pg.native,
    poolLifetime: () => new pg.Pool({ ...config, allowExitOnIdle: true }),
    poolRotation: () => new pg.Pool({ ...config, maxLifetimeSeconds: 1 }),
  };
  const rejected = [];
  for (const [name, action] of Object.entries(cases)) {
    try {
      action();
    } catch {
      rejected.push(name);
    }
  }
  return c.json(rejected);
});

app.get("/orm", async (c) => {
  const pool = new pg.Pool({ ...configuration(c), max: 2 });
  const db = drizzle({ client: pool });
  try {
    let result;
    await db.transaction(async (tx) => {
      await tx.insert(entries).values({ id: 1, label: "inserted" });
      [result] = await tx
        .update(entries)
        .set({ label: "更新 🔥" })
        .where(eq(entries.id, 1))
        .returning();
      await tx.delete(entries).where(eq(entries.id, 1));
    });
    try {
      await db.transaction(async (tx) => {
        await tx.insert(entries).values({ id: 2, label: "must roll back" });
        throw new Error("rollback probe");
      });
    } catch (error) {
      if (error.message !== "rollback probe") throw error;
    }
    const remaining = await db.select().from(entries);
    return c.json({ result, remaining });
  } finally {
    await pool.end();
  }
});

export default app;
