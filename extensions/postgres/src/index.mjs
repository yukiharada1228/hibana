import PgClient from "pg/lib/client.js";
import PgPool from "pg-pool";
import ConnectionParameters from "pg/lib/connection-parameters.js";
import Connection from "pg/lib/connection.js";
import Query from "pg/lib/query.js";
import Result from "pg/lib/result.js";
import TypeOverrides from "pg/lib/type-overrides.js";
import defaults from "pg/lib/defaults.js";
import utils from "pg/lib/utils.js";
import types from "pg-types";
import { DatabaseError } from "pg-protocol";
import { unsupported } from "./unsupported.mjs";

function configuration(input) {
  const config =
    typeof input === "string" ? { connectionString: input } : { ...input };
  // pg-pool deliberately hides this property from object enumeration.
  if (input && typeof input === "object" && input.password !== undefined)
    config.password = input.password;
  for (const name of ["connection", "stream", "Client", "Promise"])
    if (config[name] !== undefined) unsupported(`Custom ${name}`);
  if (config.enableChannelBinding) unsupported("SCRAM channel binding");
  const params = new ConnectionParameters(config);
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
  if (typeof config.password === "function") {
    const provider = config.password;
    config.password = async (...args) => {
      const value = await provider(...args);
      if (typeof value !== "string")
        throw new TypeError(
          "PostgreSQL password provider must return a string",
        );
      return value;
    };
  }
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
  return { connectionTimeoutMillis: 5000, query_timeout: 10000, ...config };
}

export class Client extends PgClient {
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

export class Pool extends PgPool {
  constructor(config) {
    const options = configuration(config);
    if (options.allowExitOnIdle) unsupported("allowExitOnIdle");
    if (options.maxLifetimeSeconds) unsupported("maxLifetimeSeconds");
    super(options, Client);
  }
}

export {
  Connection,
  Query,
  Result,
  TypeOverrides,
  defaults,
  types,
  DatabaseError,
};
export const { escapeIdentifier, escapeLiteral } = utils;
export default {
  Client,
  Pool,
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
