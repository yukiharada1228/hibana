// Test applications use exactly the same local-extension contract as consumers.
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

export async function writePostgresSelection(
  application,
  {
    tls = true,
    md5 = false,
    scram = true,
    certificateHashes = ["sha256"],
    cleartext = false,
    trust = false,
    pool = true,
  } = {},
) {
  const directory = join(application, "selection");
  await mkdir(directory, { recursive: true });
  const dependencies = [
    "postgres-core",
    `postgres-transport-${tls ? "tls" : "tcp"}`,
  ];
  const imports = [
    'import {createPostgres} from "@hibana/postgres-core";',
    `import {${tls ? "tcpTls" : "tcp"} as transport} from "@hibana/postgres-transport-${tls ? "tls" : "tcp"}";`,
  ];
  const auth = [`cleartext:${cleartext}`, `trust:${trust}`];
  if (pool) {
    dependencies.push("postgres-pool");
    imports.push('import {createPool} from "@hibana/postgres-pool";');
  }
  if (md5) {
    dependencies.push("postgres-auth-md5");
    imports.push('import {md5} from "@hibana/postgres-auth-md5";');
    auth.push("md5");
  }
  if (scram) {
    dependencies.push("postgres-auth-scram");
    imports.push('import {scramSha256} from "@hibana/postgres-auth-scram";');
    const names = {
      sha224: "SHA-224",
      sha256: "SHA-256",
      sha384: "SHA-384",
      sha512: "SHA-512",
      "sha512-224": "SHA512-224",
      "sha512-256": "SHA512-256",
    };
    const hashes = certificateHashes.map((name, index) => {
      if (!names[name]) throw new Error("Unknown test certificate hash");
      dependencies.push(name);
      imports.push(`import {digest as hash${index}} from "@hibana/${name}";`);
      return `${JSON.stringify(names[name])}:hash${index}`;
    });
    auth.push(`scram:scramSha256({certificateDigests:{${hashes.join(",")}}})`);
  }
  await writeFile(
    join(directory, "pg.mjs"),
    `${imports.join("\n")}
const pg = createPostgres({transport,authentication:{${auth.join(",")}}${pool ? ",pool:createPool" : ""}});
export const {Client,${pool ? "Pool," : ""}Connection,Query,Result,TypeOverrides,defaults,types,DatabaseError,escapeIdentifier,escapeLiteral}=pg;
export default pg;
`,
  );
  await writeFile(
    join(directory, "package.json"),
    JSON.stringify({
      private: true,
      type: "module",
      dependencies: Object.fromEntries(
        await Promise.all(
          dependencies.map(async (name) => {
            const pkg = JSON.parse(
              await readFile(
                new URL(`../extensions/${name}/package.json`, import.meta.url),
                "utf8",
              ),
            );
            return [pkg.name, pkg.version];
          }),
        ),
      ),
    }),
  );
  await writeFile(
    join(directory, "hibana.extension.json"),
    JSON.stringify({
      schemaVersion: 2,
      runtime: "wasi:http/incoming-handler@0.2.3",
      dependencies: dependencies.map((name) => `@hibana/${name}`),
      aliases: { pg: "./pg.mjs" },
    }),
  );
  return "./selection";
}
