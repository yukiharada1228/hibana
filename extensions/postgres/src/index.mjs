import { createPostgres } from "@hibana/postgres-core";
import { createPool } from "@hibana/postgres-pool";
import { tcpTls } from "@hibana/postgres-transport-tls";
import { md5 } from "@hibana/postgres-auth-md5";
import { scramSha256 } from "@hibana/postgres-auth-scram";
import { certificateDigests } from "./certificate-digest.mjs";

const pg = createPostgres({
  pool: createPool,
  transport: tcpTls,
  authentication: {
    md5,
    scram: scramSha256({ certificateDigests }),
    cleartext: true,
    trust: true,
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
