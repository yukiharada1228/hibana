import createClient from "pg/lib/client.js";
import ConnectionParameters from "pg/lib/connection-parameters.js";
import createConnection from "pg/lib/connection.js";
import Query from "pg/lib/query.js";
import Result from "pg/lib/result.js";
import TypeOverrides from "pg/lib/type-overrides.js";
import defaults from "pg/lib/defaults.js";
import utils from "pg/lib/utils.js";
import types from "pg-types";
import { DatabaseError } from "pg-protocol";
import { parse as parseConnectionString } from "pg-connection-string";
import { unsupported } from "./unsupported.mjs";
import { authenticatedClient } from "./authentication.mjs";

// Capture the implementation once while retaining its receiver (including
// prototype methods and private fields). The wrapper itself carries no state.
function bindMethods(value, names, label) {
  return Object.fromEntries(
    names.map((name) => {
      const method = value?.[name];
      if (typeof method !== "function")
        throw new TypeError(`${label} must provide ${name}`);
      return [name, method.bind(value)];
    }),
  );
}
function validateAuthentication(value) {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new TypeError("authentication must be an object");
  const auth = { ...value };
  for (const key of Object.keys(auth))
    if (!["scram", "md5", "cleartext", "trust"].includes(key))
      throw new TypeError(`Unknown PostgreSQL authentication option: ${key}`);
  for (const key of ["cleartext", "trust"])
    if (auth[key] !== undefined && typeof auth[key] !== "boolean")
      throw new TypeError(`${key} authentication must be a boolean`);
  if (auth.md5 !== undefined && typeof auth.md5 !== "function")
    throw new TypeError("md5 authentication must be a function");
  if (auth.scram !== undefined) {
    auth.scram = Object.freeze({
      ...bindMethods(
        auth.scram,
        ["startSession", "continueSession", "finalizeSession"],
        "SCRAM authentication",
      ),
      DEFAULT_MAX_SCRAM_ITERATIONS:
        auth.scram.DEFAULT_MAX_SCRAM_ITERATIONS ?? 100000,
    });
  }
  return Object.freeze(auth);
}

function validatePort(value) {
  const port =
    typeof value === "string" && /^\d+$/.test(value) ? Number(value) : value;
  if (
    typeof port !== "number" ||
    !Number.isInteger(port) ||
    port < 1 ||
    port > 65535
  )
    throw Object.assign(
      new RangeError("PostgreSQL port must be an integer from 1 to 65535"),
      { code: "ERR_SOCKET_BAD_PORT" },
    );
}

// Each factory owns its transport and authentication policy. Nothing is registered
// globally, so applications may use multiple independent configurations.
export function createPostgres({ transport, authentication = {}, pool } = {}) {
  if (pool !== undefined && typeof pool !== "function")
    throw new TypeError("pool must be a Pool factory");
  const network = Object.freeze(
    bindMethods(
      transport,
      ["getStream", "getSecureStream", "isIP", "validateTransport"],
      "PostgreSQL transport",
    ),
  );
  const auth = validateAuthentication(authentication);
  class Connection extends createConnection(network) {
    _connectStream(port, host) {
      // Let pg attach both Connection and Client error/close listeners first.
      // A synchronous transport failure then follows the normal error cleanup.
      Promise.resolve().then(() => {
        if (this.stream.destroyed) return;
        try {
          this.stream.setNoDelay(true);
          this.stream.connect(port, host);
        } catch (error) {
          this.emit("error", error);
        }
      });
    }
  }
  const PgClient = createClient({
    Connection,
    defaultMaxScramIterations:
      auth.scram?.DEFAULT_MAX_SCRAM_ITERATIONS ?? 100000,
  });
  function configuration(input) {
    const config =
      typeof input === "string" ? { connectionString: input } : { ...input };
    // pg-pool deliberately hides this property from object enumeration.
    if (input && typeof input === "object" && input.password !== undefined)
      config.password = input.password;
    for (const name of ["connection", "stream", "Client", "Promise"])
      if (config[name] !== undefined) unsupported(`Custom ${name}`);
    if (
      config.enableChannelBinding !== undefined &&
      typeof config.enableChannelBinding !== "boolean"
    )
      throw new TypeError("enableChannelBinding must be a boolean");
    // Match pg's precedence: URL options override the configuration object.
    const urlOptions = config.connectionString
      ? parseConnectionString(config.connectionString)
      : {};
    const channelBinding =
      urlOptions.channel_binding ??
      config.channel_binding ??
      (config.enableChannelBinding === false ? "disable" : "prefer");
    if (!["disable", "prefer", "require"].includes(channelBinding))
      throw new TypeError("channel_binding must be disable, prefer or require");
    // Validate before pg's parseInt can truncate fractions/junk or default a 0.
    // A URL without a port uses pg's default, overriding config.port as pg does.
    const port = Object.hasOwn(urlOptions, "port")
      ? urlOptions.port === ""
        ? undefined
        : urlOptions.port
      : config.port;
    if (port !== undefined) validatePort(port);
    const params = new ConnectionParameters(config);
    validatePort(params.port);
    network.validateTransport(params);
    if (channelBinding === "require" && !params.ssl)
      throw new Error("channel_binding=require needs TLS");
    for (const key of ["host", "user", "database"])
      if (typeof params[key] !== "string" || !params[key])
        throw new TypeError(`Provide an explicit PostgreSQL ${key}`);
    if (params.isDomainSocket) unsupported("Unix domain sockets");
    if (
      typeof params.password !== "string" &&
      typeof params.password !== "function"
    )
      throw new TypeError(
        "Provide password as a string or async function; pgpass is unavailable",
      );
    if (params.ssl && typeof params.ssl === "object") {
      for (const key of Object.keys(params.ssl))
        if (
          !["ca", "servername", "rejectUnauthorized", "ALPNProtocols"].includes(
            key,
          )
        )
          unsupported(`TLS option ${key}`);
      if (
        params.ssl.rejectUnauthorized !== undefined &&
        params.ssl.rejectUnauthorized !== true
      )
        unsupported("Disabling TLS certificate verification");
    }
    if (params.sslnegotiation === "direct")
      unsupported("Direct TLS negotiation; use PostgreSQL SSLRequest");
    if (
      config.scramMaxIterations !== undefined &&
      (!Number.isInteger(config.scramMaxIterations) ||
        config.scramMaxIterations < 1 ||
        config.scramMaxIterations > 100000)
    )
      throw new RangeError("scramMaxIterations must be 1..100000");
    return {
      connectionTimeoutMillis: 5000,
      query_timeout: 10000,
      ...config,
      channel_binding: channelBinding,
      enableChannelBinding: channelBinding !== "disable" && Boolean(params.ssl),
    };
  }

  class Client extends authenticatedClient(PgClient, auth) {
    constructor(config) {
      super(configuration(config));
    }
    // A Wasm invocation has no Node process lifetime to retain. Pool calls ref()
    // when reusing an idle connection; end() remains mandatory before returning.
    ref() {
      return this;
    }
    unref() {
      unsupported("Process lifetime control");
    }
  }

  // Pool is an explicitly supplied JS component. Client-only factories do not
  // import or construct it; both APIs use this factory's configuration policy.
  const Pool = pool?.(Client, configuration);
  if (pool !== undefined && typeof Pool !== "function")
    throw new TypeError("Pool factory must return a constructor");
  const { escapeIdentifier, escapeLiteral } = utils;
  return {
    Client,
    ...(pool === undefined ? {} : { Pool }),
    Connection,
    Query,
    Result,
    TypeOverrides,
    defaults,
    types,
    DatabaseError,
    escapeIdentifier,
    escapeLiteral,
    get native() {
      return unsupported("Native libpq");
    },
  };
}
