import { Hono } from "hono";
import pg from "pg";
import { createPostgres } from "@hibana/postgres-core";
import { createPool } from "@hibana/postgres-pool";
import { Socket, isIP } from "net";
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
    channel_binding: c.env.DB_CHANNEL_BINDING || "prefer",
  };
}
async function connected(c, action, overrides = {}) {
  const client = new pg.Client({ ...configuration(c), ...overrides });
  if (c.env.DB_TAMPER_CERT === "true") {
    client.connection.once("sslconnect", () => {
      const stream = client.connection.stream;
      const original = stream.getPeerCertificate.bind(stream);
      stream.getPeerCertificate = () => {
        const raw = Buffer.from(original().raw);
        raw[raw.length - 1] ^= 1;
        return { raw };
      };
    });
  }
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

app.get("/channel-binding", (c) =>
  connected(c, async (client) => {
    const {
      rows: [result],
    } = await client.query("SELECT 42 AS answer");
    return c.json({ ...result, channelBinding: client.channelBindingUsed });
  }),
);

app.get("/channel-binding-url", (c) => {
  const cfg = configuration(c);
  const url = new URL(`postgresql://${cfg.host}:${cfg.port}/${cfg.database}`);
  url.username = cfg.user;
  url.password = cfg.password;
  url.searchParams.set(
    "channel_binding",
    c.env.DB_CHANNEL_BINDING || "require",
  );
  return connected(
    c,
    async (client) => {
      await client.query("SELECT 1");
      return c.json({ channelBinding: client.channelBindingUsed });
    },
    { connectionString: url.toString(), channel_binding: "disable" },
  );
});

app.get("/tls-required-url", (c) => {
  const cfg = configuration(c);
  const url = new URL(`postgresql://${cfg.host}:${cfg.port}/${cfg.database}`);
  url.username = cfg.user;
  url.password = cfg.password;
  url.searchParams.set("sslmode", "require");
  return connected(
    c,
    async (client) =>
      c.json((await client.query("SELECT 1 AS answer")).rows[0]),
    { connectionString: url.toString() },
  );
});

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

app.get("/pipeline-end", (c) =>
  connected(
    c,
    async (client) => {
      const callback = c.req.query("api") === "callback";
      const sqlError = c.req.query("error") === "true";
      const notifications = [0, 0];
      let endNotifications = 0;
      const queries = [1, 42].map(
        (value, index) =>
          new Promise((resolve) => {
            const completed = (error, result) => {
              notifications[index]++;
              resolve(error ? { code: error.code } : result.rows[0]);
            };
            const query =
              sqlError && index === 0
                ? { text: "SELECT 1 / 0 AS value" }
                : { text: "SELECT $1::integer AS value", values: [value] };
            if (callback) client.query(query, completed);
            else
              client
                .query(query)
                .then((result) => completed(null, result), completed);
          }),
      );
      // Submit end before yielding, so both queries are still in flight.
      const ending = new Promise((resolve, reject) => {
        const completed = (error) => {
          endNotifications++;
          error ? reject(error) : resolve();
        };
        if (callback) client.end(completed);
        else client.end().then(() => completed(), completed);
      });
      const [results] = await Promise.all([Promise.all(queries), ending]);
      return c.json({
        results,
        notifications,
        endNotifications,
        closed: client.connection.stream.destroyed,
      });
    },
    { pipeline: true, query_timeout: 0 },
  ),
);

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

app.get("/cancel-connect", async (c) => {
  let started, release;
  const waiting = new Promise((resolve) => {
    started = resolve;
  });
  const config = configuration(c);
  const client = new pg.Client({
    ...config,
    password: () => {
      started();
      return new Promise((resolve) => {
        release = resolve;
      });
    },
  });
  let notifications = 0;
  const completed = (error) => {
    notifications++;
    return error?.code ?? "connected";
  };
  const connecting =
    c.req.query("api") === "callback"
      ? new Promise((resolve) =>
          client.connect((error) => resolve(completed(error))),
        )
      : client.connect().then(() => completed(), completed);
  try {
    await Promise.race([
      waiting,
      connecting.then((code) => {
        throw new Error(`Connect finished before password lookup: ${code}`);
      }),
    ]);
    await Promise.all([client.end(), client.end()]);
    release(config.password);
    return c.json({
      code: await connecting,
      notifications,
      closed: client.connection.stream.destroyed,
    });
  } finally {
    release?.(config.password);
    await client.end();
  }
});

app.get("/invalid-ports", (c) => {
  const config = configuration(c);
  const invalid = [
    0,
    -1,
    65536,
    70000,
    NaN,
    Infinity,
    1.5,
    "invalid",
    "5432junk",
    "1.5",
    "",
    null,
    true,
    {},
    [],
  ];
  const inputs = [
    ...invalid.map((port) => ({ ...config, port })),
    ...["0", "-1", "70000", "invalid", "5432junk", "1.5"].map((port) => ({
      ...config,
      connectionString: `postgres://probe:fixture@localhost/probe?port=${port}`,
    })),
  ];
  const rejected = {};
  for (const [name, Constructor] of Object.entries({
    client: pg.Client,
    pool: pg.Pool,
  })) {
    rejected[name] = 0;
    for (const input of inputs) {
      try {
        new Constructor(input);
      } catch (error) {
        if (error.code !== "ERR_SOCKET_BAD_PORT") throw error;
        rejected[name]++;
      }
    }
  }
  return c.json(rejected);
});

app.get("/connect-error", async (c) => {
  const streams = [];
  const error = Object.assign(new Error("Fixture connection start failure"), {
    code: "ERR_FIXTURE_CONNECT",
  });
  class FailingSocket extends Socket {
    setNoDelay() {
      if (c.req.query("method") === "setNoDelay") throw error;
      return this;
    }
    connect() {
      throw error;
    }
  }
  const { Client, Pool } = createPostgres({
    pool: createPool,
    transport: {
      isIP,
      getStream() {
        const stream = new FailingSocket();
        streams.push(stream);
        return stream;
      },
      getSecureStream() {
        throw new Error("TLS must not start");
      },
      validateTransport() {},
    },
  });
  const pooled = c.req.query("owner") === "pool";
  const owner = new (pooled ? Pool : Client)({
    ...configuration(c),
    connectionTimeoutMillis: 0,
  });
  let code,
    notifications = 0;
  const completed = (cause) => {
    notifications++;
    return cause?.code ?? "connected";
  };
  try {
    code =
      c.req.query("api") === "callback"
        ? await new Promise((resolve) =>
            owner.connect((cause) => resolve(completed(cause))),
          )
        : await owner.connect().then(() => completed(), completed);
  } finally {
    await owner.end();
  }
  return c.json({
    code,
    notifications,
    closed: streams.length === 1 && streams.every((stream) => stream.destroyed),
    remaining: pooled ? owner.totalCount : 0,
  });
});

app.get("/options", (c) => {
  const config = configuration(c);
  const cases = {
    password: () => new pg.Client({ ...config, password: undefined }),
    channelBinding: () =>
      new pg.Client({ ...config, channel_binding: "invalid" }),
    channelBindingWithoutTls: () =>
      new pg.Client({ ...config, ssl: false, channel_binding: "require" }),
    channelBindingOption: () =>
      new pg.Client({ ...config, enableChannelBinding: "require" }),
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
