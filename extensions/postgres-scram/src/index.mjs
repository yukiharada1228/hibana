import { createPostgres } from "@hibana/postgres-core";
import { createPool } from "@hibana/postgres-pool";
import { tcpTls } from "@hibana/postgres-transport-tls";
import { scramSha256 } from "@hibana/postgres-auth-scram";
import { digest } from "@hibana/sha256";

const pg = createPostgres({
  pool: createPool,
  transport: tcpTls,
  authentication: {
    scram: scramSha256({ certificateDigests: { "SHA-256": digest } }),
  },
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
