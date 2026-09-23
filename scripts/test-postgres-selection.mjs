import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { setImmediate } from "node:timers/promises";
import test from "node:test";
import { createPostgres } from "../extensions/postgres-core/dist/index.mjs";
import { createPool } from "../extensions/postgres-pool/dist/index.mjs";

class Stream extends EventEmitter {
  writable = true;
  frames = [];
  write(bytes) {
    this.frames.push(bytes);
    return true;
  }
  destroy() {
    this.destroyed = true;
  }
}
const transport = {
  getStream: () => new Stream(),
  isIP: () => 0,
  getSecureStream: () => {
    throw new Error("Not selected");
  },
  validateTransport() {},
};
const config = {
  host: "localhost",
  user: "probe",
  database: "probe",
  password: "fixture",
  ssl: false,
};
test("Client-only factories exclude Pool; Pool is explicitly selected", () => {
  const clientOnly = createPostgres({ transport });
  assert.equal(Object.hasOwn(clientOnly, "Pool"), false);
  assert.equal(typeof clientOnly.Client, "function");
  const pooled = createPostgres({ transport, pool: createPool });
  assert.equal(typeof pooled.Pool, "function");
  for (const pool of [null, true, {}, () => undefined, () => ({})])
    assert.throws(() => createPostgres({ transport, pool }), TypeError);
});
test("selected Pool validates options without opening a connection", async () => {
  let streams = 0;
  const { Pool } = createPostgres({
    transport: {
      ...transport,
      getStream() {
        streams++;
        return new Stream();
      },
    },
    pool: createPool,
  });
  for (const options of [
    { allowExitOnIdle: true },
    { maxLifetimeSeconds: 1 },
    { Client() {} },
    { Promise },
    { stream: {} },
  ])
    assert.throws(() => new Pool({ ...config, ...options }), {
      code: "ERR_NOT_SUPPORTED",
    });
  const hiddenPassword = { ...config };
  Object.defineProperty(hiddenPassword, "password", {
    value: config.password,
    enumerable: false,
  });
  const pool = new Pool(hiddenPassword);
  assert.equal(pool.options.password, config.password);
  assert.equal(pool.options.connectionTimeoutMillis, 5000);
  assert.equal(pool.options.query_timeout, 10000);
  assert.equal(streams, 0);
  await pool.end();
});
function clientFor(authentication) {
  const client = new (createPostgres({ transport, authentication }).Client)(
    config,
  );
  const errors = [];
  client.on("error", (error) => errors.push(error));
  client._attachListeners(client.connection);
  return { client, errors };
}
test("independent factories keep authentication policy isolated, including later input mutations", async () => {
  let hashed = 0;
  const authentication = {
    md5: async () => {
      hashed++;
      return "md5" + "0".repeat(32);
    },
  };
  const allowed = clientFor(authentication),
    denied = clientFor({});
  delete authentication.md5;
  allowed.client._handleAuthMD5Password({ salt: Buffer.alloc(4) });
  denied.client._handleAuthMD5Password({ salt: Buffer.alloc(4) });
  await setImmediate();
  assert.equal(hashed, 1);
  assert.equal(allowed.client.connection.stream.frames.length, 1);
  assert.deepEqual(allowed.errors, []);
  assert.equal(denied.client.connection.stream.frames.length, 0);
  assert.equal(denied.errors[0].code, "ERR_PG_AUTH_UNAVAILABLE");
  assert.equal(denied.client.connection.stream.destroyed, true);
});
test("trust and cleartext authentication require explicit selection", () => {
  for (const method of ["cleartext", "trust"]) {
    const { client, errors } = clientFor({});
    if (method === "cleartext") client._handleAuthCleartextPassword({});
    else client.connection.emit("authenticationOk");
    assert.equal(errors[0].code, "ERR_PG_AUTH_UNAVAILABLE");
    assert.equal(client.connection.stream.frames.length, 0);
  }
  const allowed = clientFor({ trust: true });
  allowed.client.connection.emit("authenticationOk");
  assert.deepEqual(allowed.errors, []);
  assert.equal(allowed.client._authenticationState, "complete");
});
test("unexpected SCRAM and ReadyForQuery cannot bypass authentication", () => {
  for (const action of [
    (client) => client._handleAuthSASLContinue({}),
    (client) => client._handleAuthSASLFinal({}),
    (client) => client._handleReadyForQuery({}),
  ]) {
    const { client, errors } = clientFor({ trust: true });
    action(client);
    assert.equal(errors[0].code, "ERR_PG_AUTH_UNAVAILABLE");
    assert.equal(client.connection.stream.frames.length, 0);
  }
});
test("invalid selection fails before creating a transport", () => {
  for (const authentication of [
    { unknown: true },
    { md5: true },
    { scram: {} },
    { trust: "true" },
    null,
  ])
    assert.throws(
      () => createPostgres({ transport, authentication }),
      TypeError,
    );
  assert.throws(() => createPostgres(), TypeError);
});

// A minimal PostgreSQL peer: request authentication after startup, then record whether the
// client sends a password or disposes the connection. No database is needed.
function authenticationFrame(code, data = Buffer.alloc(0)) {
  const frame = Buffer.alloc(9 + data.length);
  frame[0] = 0x52;
  frame.writeUInt32BE(8 + data.length, 1);
  frame.writeUInt32BE(code, 5);
  data.copy(frame, 9);
  return frame;
}
const md5Request = authenticationFrame(5, Buffer.from([1, 2, 3, 4]));
const cleartextRequest = authenticationFrame(3);
const scramRequest = authenticationFrame(10, Buffer.from("SCRAM-SHA-256\0\0"));
const ready = Buffer.from("5a0000000549", "hex");
class AuthenticationStream extends Stream {
  constructor(request) {
    super();
    this.request = request;
  }
  setNoDelay() {}
  connect() {
    queueMicrotask(() => this.emit("connect"));
  }
  write(bytes, callback) {
    super.write(bytes);
    if (bytes[0] === 0) queueMicrotask(() => this.emit("data", this.request));
    else if (bytes[0] === 0x70) queueMicrotask(() => this.emit("password"));
    callback?.();
    return true;
  }
  destroy(error) {
    if (this.destroyed) return;
    super.destroy();
    this.writable = false;
    queueMicrotask(() => {
      if (error) this.emit("error", error);
      this.emit("close");
    });
  }
  end() {
    this.destroy();
  }
}
function authenticationPeer(authentication, request) {
  const streams = [];
  const pg = createPostgres({
    pool: createPool,
    transport: {
      ...transport,
      getStream() {
        const stream = new AuthenticationStream(request);
        streams.push(stream);
        return stream;
      },
    },
    authentication,
  });
  return { Client: pg.Client, Pool: pg.Pool, streams };
}
const md5Peer = (md5) => authenticationPeer({ md5 }, md5Request);
test("independent pools retain their factory's authentication policy", async () => {
  const request = Buffer.concat([authenticationFrame(0), ready]);
  const allowed = authenticationPeer({ trust: true }, request);
  const denied = authenticationPeer({}, request);
  const accepted = new allowed.Pool(config),
    rejected = new denied.Pool(config);
  try {
    const client = await accepted.connect();
    client.release();
    await assert.rejects(rejected.connect(), {
      code: "ERR_PG_AUTH_UNAVAILABLE",
    });
  } finally {
    await Promise.all([accepted.end(), rejected.end()]);
  }
  assert.ok(
    [...allowed.streams, ...denied.streams].every((stream) => stream.destroyed),
  );
});
test("invalid ports fail before Client or Pool creates a transport", () => {
  let opened = 0;
  const { Client, Pool } = createPostgres({
    pool: createPool,
    transport: {
      ...transport,
      getStream() {
        opened++;
        return new Stream();
      },
    },
    authentication: { trust: true },
  });
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
  for (const Constructor of [Client, Pool]) {
    for (const port of invalid)
      assert.throws(() => new Constructor({ ...config, port }), {
        code: "ERR_SOCKET_BAD_PORT",
      });
    for (const port of ["0", "-1", "70000", "invalid", "5432junk", "1.5"])
      assert.throws(
        () =>
          new Constructor({
            ...config,
            connectionString: `postgres://probe:fixture@localhost/probe?port=${port}`,
          }),
        { code: "ERR_SOCKET_BAD_PORT" },
      );
  }
  assert.equal(opened, 0);
  for (const port of [1, 65535, 5432, "5432", "00080"]) {
    const client = new Client({ ...config, port });
    assert.equal(client.port, Number(port));
    client.connection.stream.destroy();
  }
  for (const [input, expected] of [
    [config, 5432],
    [
      {
        ...config,
        port: -1,
        connectionString: "postgres://probe:fixture@localhost:5433/probe",
      },
      5433,
    ],
    [
      {
        ...config,
        port: -1,
        connectionString: "postgres://probe:fixture@localhost/probe",
      },
      5432,
    ],
  ]) {
    const client = new Client(input);
    assert.equal(client.port, expected);
    client.connection.stream.destroy();
  }
});
test(
  "synchronous transport failures settle Client and Pool and release their sockets",
  { timeout: 3000 },
  async (t) => {
    for (const method of ["setNoDelay", "connect"])
      for (const api of [
        "client-promise",
        "client-callback",
        "pool-promise",
        "pool-callback",
      ])
        await t.test(`${method}: ${api}`, async (t) => {
          const error = new Error(`Fixture ${method} failure`);
          const streams = [];
          const { Client, Pool } = createPostgres({
            pool: createPool,
            transport: {
              ...transport,
              getStream() {
                const stream = new AuthenticationStream(
                  Buffer.concat([authenticationFrame(0), ready]),
                );
                stream[method] = () => {
                  throw error;
                };
                streams.push(stream);
                return stream;
              },
            },
            authentication: { trust: true },
          });
          t.after(() => streams.forEach((stream) => stream.destroy()));
          const pooled = api.startsWith("pool");
          const owner = new (pooled ? Pool : Client)({
            ...config,
            connectionTimeoutMillis: 0,
            query_timeout: 0,
          });
          const events = [];
          owner.on("error", (cause) => events.push(cause));
          const querying = pooled
            ? undefined
            : owner.query("SELECT 1").then(
                () => "unexpected success",
                (cause) => cause.message,
              );
          let calls = 0,
            nestedEnd;
          if (api.endsWith("callback")) {
            let initiating = true;
            const connected = new Promise((resolve) => {
              assert.doesNotThrow(() =>
                owner.connect((cause) => {
                  calls++;
                  assert.equal(initiating, false);
                  if (!pooled) nestedEnd = owner.end();
                  resolve(cause);
                }),
              );
            });
            initiating = false;
            assert.equal(await connected, error);
          } else
            await assert.rejects(owner.connect(), (cause) => cause === error);
          await owner.end();
          await nestedEnd;
          if (!pooled) {
            assert.match(await querying, /Connection terminated/);
            await owner.end();
          } else assert.equal(owner.totalCount, 0);
          await setImmediate();
          if (api.endsWith("callback")) assert.equal(calls, 1);
          assert.deepEqual(events, []);
          assertClosedWithoutPassword(streams);
        });
  },
);
test(
  "selected cleartext and trust authentication still connect through Client and Pool",
  { timeout: 1500 },
  async () => {
    for (const method of ["cleartext", "trust"])
      for (const pooled of [false, true]) {
        const { Client, Pool, streams } = authenticationPeer(
          { [method]: true },
          method === "cleartext"
            ? cleartextRequest
            : Buffer.concat([authenticationFrame(0), ready]),
        );
        const connection = new (pooled ? Pool : Client)({
          ...config,
          password: async () => "fixture",
          connectionTimeoutMillis: 0,
        });
        const result = connection.connect();
        streams[0].once("password", () =>
          streams[0].emit(
            "data",
            Buffer.concat([authenticationFrame(0), ready]),
          ),
        );
        const client = await result;
        assert.equal(client.channelBindingUsed, false);
        assert.equal(
          streams[0].frames.filter((bytes) => bytes[0] === 0x70).length,
          method === "cleartext" ? 1 : 0,
        );
        if (pooled) client.release();
        await connection.end();
        assert.equal(streams[0].destroyed, true);
      }
  },
);
function assertClosedWithoutPassword(streams) {
  assert.ok(streams.length > 0);
  for (const stream of streams) {
    assert.equal(stream.destroyed, true);
    assert.equal(
      stream.frames.some((bytes) => bytes[0] === 0x70),
      false,
    );
  }
}
test(
  "MD5 failures reject Client and Pool connections, invoke callbacks once, and close the socket",
  { timeout: 3000 },
  async (t) => {
    for (const mode of ["throw", "reject"])
      for (const api of ["client", "callback", "pool"])
        await t.test(`${mode}: ${api}`, async (t) => {
          const error = new Error("Fixture digest failure");
          const { Client, Pool, streams } = md5Peer(() => {
            if (mode === "throw") throw error;
            return Promise.reject(error);
          });
          // With timeouts disabled, only the authentication failure can settle
          // connect(). No Client 'error' listener should be required here.
          const options = { ...config, connectionTimeoutMillis: 0 };
          let calls = 0;
          if (api === "pool") {
            const pool = new Pool(options);
            t.after(() => pool.end());
            await assert.rejects(pool.connect(), (cause) => cause === error);
            assert.equal(pool.totalCount, 0);
            assert.equal(pool.waitingCount, 0);
          } else {
            const client = new Client(options);
            t.after(() => client.end());
            if (api === "callback") {
              const cause = await new Promise((resolve) => {
                client.connect((cause) => {
                  calls++;
                  resolve(cause);
                });
              });
              assert.equal(cause, error);
            } else
              await assert.rejects(
                client.connect(),
                (cause) => cause === error,
              );
          }
          await setImmediate();
          if (api === "callback") assert.equal(calls, 1);
          assertClosedWithoutPassword(streams);
        });
  },
);
test(
  "a rejected password provider closes the socket before hashing",
  { timeout: 1500 },
  async (t) => {
    let hashed = false;
    const { Client, streams } = md5Peer(() => {
      hashed = true;
    });
    const error = new Error("Fixture credential failure");
    const client = new Client({
      ...config,
      connectionTimeoutMillis: 0,
      password: async () => {
        throw error;
      },
    });
    t.after(() => client.end());
    await assert.rejects(client.connect(), (cause) => cause === error);
    assert.equal(hashed, false);
    assertClosedWithoutPassword(streams);
  },
);
test(
  "a late MD5 result after disconnection cannot send credentials or another error",
  { timeout: 1500 },
  async () => {
    for (const rejected of [false, true]) {
      let finish, started;
      const hashing = new Promise((resolve) => {
        started = resolve;
      });
      const { Client, streams } = md5Peer(() => {
        started();
        return new Promise((resolve, reject) => {
          finish = () =>
            rejected
              ? reject(new Error("Late digest failure"))
              : resolve("md5" + "0".repeat(32));
        });
      });
      const client = new Client({ ...config, connectionTimeoutMillis: 0 });
      const error = new Error("Fixture disconnected");
      const connection = assert.rejects(
        client.connect(),
        (cause) => cause === error,
      );
      await hashing;
      streams[0].destroy(error);
      await connection;
      finish();
      await setImmediate();
      assertClosedWithoutPassword(streams);
      await client.end();
    }
  },
);
test("transport and SCRAM prototype methods retain their receiver and captured implementation", async () => {
  const calls = [];
  class Transport {
    #stream = new Stream();
    validateTransport() {
      calls.push(this.#stream);
    }
    getStream() {
      return this.#stream;
    }
    getSecureStream() {
      return this.#stream;
    }
    isIP() {
      return this.#stream ? 0 : 4;
    }
  }
  class Scram {
    #session;
    get DEFAULT_MAX_SCRAM_ITERATIONS() {
      return 1234;
    }
    startSession() {
      this.#session = { mechanism: "SCRAM-SHA-256", response: "fixture first" };
      return this.#session;
    }
    async continueSession(session) {
      assert.equal(session, this.#session);
      session.response = "fixture final";
    }
    finalizeSession(session, data) {
      assert.equal(session, this.#session);
      assert.equal(data, "fixture proof");
    }
  }
  const network = new Transport(),
    scram = new Scram();
  const { Client, Connection } = createPostgres({
    transport: network,
    authentication: { scram },
  });
  for (const key of [
    "getStream",
    "getSecureStream",
    "isIP",
    "validateTransport",
  ])
    network[key] = () => {
      throw new Error("Replaced transport method");
    };
  for (const key of ["startSession", "continueSession", "finalizeSession"])
    scram[key] = () => {
      throw new Error("Replaced SCRAM method");
    };
  const client = new Client(config);
  assert.equal(client.scramMaxIterations, 1234);
  assert.equal(calls[0], client.connection.stream);
  client._attachListeners(client.connection);
  await client._handleAuthSASL({ mechanisms: ["SCRAM-SHA-256"] });
  await client._handleAuthSASLContinue({ data: "fixture challenge" });
  client._handleAuthSASLFinal({ data: "fixture proof" });
  client.connection.emit("authenticationOk");
  assert.equal(client._authenticationState, "complete");
  assert.equal(client.connection.stream.frames.length, 2);
  // Exercise the captured isIP/getSecureStream methods as well.
  const secure = new Connection({ ssl: true });
  secure.upgradeToSSL("localhost", () => {});
  assert.equal(secure.stream, calls[0]);
});

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
test(
  "end drains pipelined queries after success or SQL errors without a timeout",
  { timeout: 3000 },
  async (t) => {
    function message(type, text) {
      const body = Buffer.from(text);
      const frame = Buffer.alloc(5 + body.length);
      frame[0] = type.charCodeAt(0);
      frame.writeUInt32BE(4 + body.length, 1);
      body.copy(frame, 5);
      return Buffer.concat([frame, ready]);
    }
    for (const api of ["promise", "callback"])
      for (const sqlError of [false, true])
        await t.test(
          `${api}: ${sqlError ? "SQL error" : "success"}`,
          async (t) => {
            const { Client, streams } = authenticationPeer(
              { trust: true },
              Buffer.concat([authenticationFrame(0), ready]),
            );
            const client = new Client({
              ...config,
              pipeline: true,
              connectionTimeoutMillis: 0,
              query_timeout: 0,
            });
            t.after(() => streams[0].destroy());
            const errors = [],
              results = [];
            let ends = 0;
            client.on("error", (error) => errors.push(error));
            await client.connect();
            const queries = ["BEGIN", "COMMIT"].map(
              (sql) =>
                new Promise((resolve) => {
                  const completed = (error, result) => {
                    results.push(error ? error.code : result.command);
                    resolve();
                  };
                  if (api === "callback") client.query(sql, completed);
                  else
                    client
                      .query(sql)
                      .then((result) => completed(null, result), completed);
                }),
            );
            assert.equal(
              streams[0].frames.filter((frame) => frame[0] === 0x51).length,
              2,
            );
            const ending = new Promise((resolve) => {
              const completed = () => {
                ends++;
                resolve();
              };
              if (api === "callback") client.end(completed);
              else client.end().then(completed);
            });
            streams[0].emit(
              "data",
              sqlError
                ? message("E", "SERROR\0C42601\0MFixture syntax error\0\0")
                : message("C", "BEGIN\0"),
            );
            await setImmediate();
            assert.deepEqual(results, [sqlError ? "42601" : "BEGIN"]);
            assert.equal(ends, 0, "end must wait for the second query");
            assert.notEqual(streams[0].destroyed, true);
            streams[0].emit("data", message("C", "COMMIT\0"));
            await setImmediate();
            assert.deepEqual(results, [sqlError ? "42601" : "BEGIN", "COMMIT"]);
            assert.equal(ends, 1);
            assert.equal(streams[0].destroyed, true);
            assert.deepEqual(errors, []);
            await Promise.all([...queries, ending]);
          },
        );
  },
);
test(
  "end cancels an unfinished connect exactly once at every connection stage",
  { timeout: 4000 },
  async (t) => {
    for (const stage of ["transport", "password", "md5", "scram", "ready"])
      for (const api of ["promise", "callback"])
        await t.test(`${stage}: ${api}`, async () => {
          const pending = deferred(),
            started = deferred();
          const authentication =
            stage === "scram"
              ? {
                  scram: fixtureScram({
                    continueSession: () => {
                      started.resolve();
                      return pending.promise;
                    },
                  }),
                }
              : stage === "md5"
                ? {
                    md5: () => {
                      started.resolve();
                      return pending.promise;
                    },
                  }
                : { cleartext: true, trust: true };
          const request =
            stage === "scram"
              ? scramRequest
              : stage === "md5"
                ? md5Request
                : stage === "ready"
                  ? authenticationFrame(0)
                  : cleartextRequest;
          const { Client, streams } = authenticationPeer(
            authentication,
            request,
          );
          const client = new Client({
            ...config,
            connectionTimeoutMillis: 0,
            ...(stage === "password"
              ? {
                  password: () => {
                    started.resolve();
                    return pending.promise;
                  },
                }
              : {}),
          });
          const errors = [];
          let calls = 0;
          const completed = (error) => {
            calls++;
            if (error) errors.push(error.code);
          };
          if (api === "promise")
            client.connect().then(() => completed(), completed);
          else client.connect(completed);
          if (stage === "scram") {
            await setImmediate();
            streams[0].emit(
              "data",
              authenticationFrame(11, Buffer.from("fixture challenge")),
            );
          }
          if (["password", "md5", "scram"].includes(stage))
            await started.promise;
          else if (stage === "ready") await setImmediate();
          const passwordsBeforeEnd = streams[0].frames.filter(
            (bytes) => bytes[0] === 0x70,
          ).length;
          let closed = 0;
          const end = () =>
            api === "promise"
              ? client.end().then(() => {
                  closed++;
                })
              : new Promise((resolve, reject) => {
                  client.end((error) => {
                    closed++;
                    error ? reject(error) : resolve();
                  });
                });
          await Promise.all([end(), end()]);
          // A password/hash result arriving later must not send another frame or
          // error. Use both resolution and rejection across the two public APIs.
          if (["password", "md5", "scram"].includes(stage)) {
            if (api === "promise") pending.resolve("fixture");
            else pending.reject(new Error("Late result after cancellation"));
          }
          await setImmediate();
          await end();
          assert.equal(calls, 1);
          assert.deepEqual(errors, ["ERR_PG_CONNECTION_CANCELLED"]);
          assert.equal(closed, 3);
          assert.equal(streams[0].destroyed, true);
          assert.equal(
            streams[0].frames.filter((bytes) => bytes[0] === 0x70).length,
            passwordsBeforeEnd,
          );
        });
  },
);
const fixtureScram = (changes = {}) => ({
  startSession: () => ({
    mechanism: "SCRAM-SHA-256",
    response: "fixture first",
  }),
  continueSession: async (session) => {
    session.response = "fixture final";
  },
  finalizeSession() {},
  ...changes,
});
test(
  "connect callbacks can call end again during cancellation",
  { timeout: 1500 },
  async () => {
    const { Client, streams } = authenticationPeer(
      { cleartext: true },
      cleartextRequest,
    );
    const client = new Client({ ...config, connectionTimeoutMillis: 0 });
    let calls = 0,
      nested;
    client.connect((error) => {
      calls++;
      assert.equal(error.code, "ERR_PG_CONNECTION_CANCELLED");
      nested = client.end();
    });
    await client.end();
    await nested;
    await setImmediate();
    assert.equal(calls, 1);
    assertClosedWithoutPassword(streams);
  },
);
test(
  "SCRAM stops before starting a session when its password arrives after disconnect",
  { timeout: 1500 },
  async () => {
    const password = deferred(),
      started = deferred();
    let sessions = 0;
    const { Client, streams } = authenticationPeer(
      {
        scram: fixtureScram({
          startSession() {
            sessions++;
            return { mechanism: "SCRAM-SHA-256", response: "fixture first" };
          },
        }),
      },
      scramRequest,
    );
    const client = new Client({
      ...config,
      connectionTimeoutMillis: 0,
      password: () => {
        started.resolve();
        return password.promise;
      },
    });
    const error = new Error("Fixture disconnected");
    const connection = assert.rejects(
      client.connect(),
      (cause) => cause === error,
    );
    await started.promise;
    streams[0].destroy(error);
    await connection;
    password.resolve("fixture");
    await setImmediate();
    assert.equal(sessions, 0);
    assertClosedWithoutPassword(streams);
    await client.end();
  },
);
test(
  "an authentication success before a password response cannot complete connect",
  { timeout: 1500 },
  async () => {
    for (const [authentication, request] of [
      [{ cleartext: true }, cleartextRequest],
      [{ md5: async () => "md5" + "0".repeat(32) }, md5Request],
    ]) {
      const password = deferred(),
        started = deferred();
      const { Client, streams } = authenticationPeer(authentication, request);
      const client = new Client({
        ...config,
        connectionTimeoutMillis: 0,
        password: () => {
          started.resolve();
          return password.promise;
        },
      });
      let outcome;
      const connected = client.connect().then(
        () => {
          outcome = "connected";
        },
        (error) => {
          outcome = error.code;
        },
      );
      await started.promise;
      streams[0].emit("data", Buffer.concat([authenticationFrame(0), ready]));
      await connected;
      password.resolve("fixture");
      await setImmediate();
      await client.end();
      assert.equal(outcome, "ERR_PG_AUTH_UNAVAILABLE");
      assertClosedWithoutPassword(streams);
    }
  },
);
test("duplicate authentication challenges are rejected before credentials are sent", async () => {
  const { client, errors } = clientFor({
    md5: async () => "md5" + "0".repeat(32),
  });
  client._handleAuthMD5Password({ salt: Buffer.alloc(4) });
  client._handleAuthMD5Password({ salt: Buffer.alloc(4) });
  await setImmediate();
  assert.equal(errors.length, 1);
  assert.equal(errors[0].code, "ERR_PG_AUTH_UNAVAILABLE");
  assertClosedWithoutPassword([client.connection.stream]);
});

test(
  "late SCRAM calculations cannot send a proof or report another error after disconnect",
  { timeout: 1500 },
  async () => {
    for (const rejected of [false, true]) {
      const calculation = deferred(),
        started = deferred();
      const { Client, streams } = authenticationPeer(
        {
          scram: fixtureScram({
            continueSession: () => {
              started.resolve();
              return calculation.promise;
            },
          }),
        },
        scramRequest,
      );
      const client = new Client({ ...config, connectionTimeoutMillis: 0 });
      const error = new Error("Fixture disconnected");
      const connection = assert.rejects(
        client.connect(),
        (cause) => cause === error,
      );
      await setImmediate();
      const stream = streams[0];
      assert.equal(
        stream.frames.filter((bytes) => bytes[0] === 0x70).length,
        1,
      );
      // Record calls as well as wire frames: a closed stream can hide an unwanted
      // late write, but an injected transport may not silently discard it.
      let proofs = 0;
      client.connection.sendSCRAMClientFinalMessage = () => {
        proofs++;
      };
      stream.emit(
        "data",
        authenticationFrame(11, Buffer.from("fixture challenge")),
      );
      await started.promise;
      stream.destroy(error);
      await connection;
      if (rejected) calculation.reject(new Error("Late calculation failure"));
      else calculation.resolve();
      await setImmediate();
      assert.equal(proofs, 0);
      assert.equal(stream.destroyed, true);
      await client.end();
    }
  },
);
test("SCRAM final verification rejects asynchronous implementations instead of accepting an unverified proof", async () => {
  for (const method of ["startSession", "finalizeSession"]) {
    const { client, errors } = clientFor({
      scram: fixtureScram({
        [method]: () =>
          Promise.reject(new Error("Async fixture verifier failed")),
      }),
    });
    await client._handleAuthSASL({ mechanisms: ["SCRAM-SHA-256"] });
    if (method === "finalizeSession") {
      await client._handleAuthSASLContinue({ data: "fixture challenge" });
      client._handleAuthSASLFinal({ data: "fixture proof" });
    }
    client.connection.emit("authenticationOk");
    await setImmediate();
    assert.equal(errors.length, 1);
    assert.match(errors[0].message, /must complete synchronously/);
    assert.equal(client.connection.stream.destroyed, true);
    assert.equal(client.channelBindingUsed, false);
    assert.equal(client._authenticationState, "failed");
  }
});
test("SCRAM rejects continuations before the initial response and finals before the proof", async () => {
  for (const pending of ["password", "calculation"]) {
    const gate = deferred();
    const { client, errors } = clientFor({
      scram: fixtureScram({
        continueSession: () => gate.promise,
      }),
    });
    if (pending === "password") {
      client.password = () => gate.promise;
      client._handleAuthSASL({ mechanisms: ["SCRAM-SHA-256"] });
      client._handleAuthSASLContinue({ data: "fixture challenge" });
    } else {
      await client._handleAuthSASL({ mechanisms: ["SCRAM-SHA-256"] });
      client._handleAuthSASLContinue({ data: "fixture challenge" });
      client._handleAuthSASLFinal({ data: "fixture proof" });
    }
    gate.resolve("fixture");
    await setImmediate();
    assert.equal(errors.length, 1);
    assert.equal(errors[0].code, "ERR_PG_AUTH_UNAVAILABLE");
    assert.equal(client.connection.stream.destroyed, true);
  }
});
test(
  "cleartext and SCRAM failures settle public Client/Pool APIs and release their sockets",
  { timeout: 3000 },
  async () => {
    for (const method of ["cleartext", "scram"])
      for (const failure of ["password", "start", "continue", "final"])
        for (const pooled of [false, true]) {
          if (method === "cleartext" && failure !== "password") continue;
          const error = new Error(`Fixture ${failure} failure`);
          const authentication =
            method === "cleartext"
              ? { cleartext: true }
              : {
                  scram: fixtureScram({
                    ...(failure === "start"
                      ? {
                          startSession() {
                            throw error;
                          },
                        }
                      : {}),
                    ...(failure === "continue"
                      ? { continueSession: () => Promise.reject(error) }
                      : {}),
                    ...(failure === "final"
                      ? {
                          finalizeSession() {
                            throw error;
                          },
                        }
                      : {}),
                  }),
                };
          const { Client, Pool, streams } = authenticationPeer(
            authentication,
            method === "cleartext" ? cleartextRequest : scramRequest,
          );
          const connection = new (pooled ? Pool : Client)({
            ...config,
            connectionTimeoutMillis: 0,
            ...(failure === "password"
              ? {
                  password: async () => {
                    throw error;
                  },
                }
              : {}),
          });
          const result = assert.rejects(
            connection.connect(),
            (cause) => cause === error,
          );
          await setImmediate();
          if (["continue", "final"].includes(failure)) {
            streams[0].emit(
              "data",
              authenticationFrame(11, Buffer.from("fixture challenge")),
            );
            await setImmediate();
            if (failure === "final")
              streams[0].emit(
                "data",
                authenticationFrame(12, Buffer.from("fixture proof")),
              );
          }
          await result;
          assert.equal(streams[0].destroyed, true);
          if (pooled) assert.equal(connection.totalCount, 0);
          await connection.end();
        }
  },
);
