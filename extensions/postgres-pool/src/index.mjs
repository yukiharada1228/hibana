import PgPool from "pg-pool";

// Called by createPostgres only when the application selects this component.
// Capture the supplied Client and validator so a pool retains the same policy.
export function createPool(Client, configuration) {
  if (typeof Client !== "function" || typeof configuration !== "function")
    throw new TypeError(
      "createPool requires a Client and configuration validator",
    );
  return class Pool extends PgPool {
    constructor(config) {
      const options = configuration(config);
      for (const name of ["allowExitOnIdle", "maxLifetimeSeconds"])
        if (options[name])
          throw Object.assign(
            new Error(`${name} is not supported by @hibana/postgres-pool`),
            { code: "ERR_NOT_SUPPORTED" },
          );
      super(options, Client);
    }
  };
}
