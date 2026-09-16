import { createPostgres } from "@hibana/postgres-core";
import { createPool } from "@hibana/postgres-pool";
import { tcp } from "@hibana/postgres-transport-tcp";
import { md5 } from "@hibana/postgres-auth-md5";
import { scramSha256 } from "@hibana/postgres-auth-scram";

const pg = createPostgres({
  pool: createPool,
  transport: tcp,
  authentication: { md5, scram: scramSha256(), cleartext: true, trust: true },
});
export const {
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
} = pg;
export default pg;
