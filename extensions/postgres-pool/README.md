# @hibana/postgres-pool

PostgreSQL の接続プールだけを提供する0.7.0の JS 部品です。`pg-pool` 3.14.0 を使用し、Client・プロトコル・通信・認証・Wasm は含みません。`createPostgres` に `pool: createPool` を渡した構成だけで有効になります。

```js
import { createPostgres } from '@hibana/postgres-core';
import { createPool } from '@hibana/postgres-pool';
import { tcpTls } from '@hibana/postgres-transport-tls';
import { scramSha256 } from '@hibana/postgres-auth-scram';
import { digest } from '@hibana/sha256';

const pg = createPostgres({
  transport: tcpTls,
  authentication: {
    scram: scramSha256({ certificateDigests: { 'SHA-256': digest } }),
  },
  pool: createPool,
});
export const { Client, Pool, types } = pg;
export default pg;
```

[個別の部品構成](../postgres-core/README.md)では、`hibana.json` の `extensions` に `"@hibana/postgres-pool": "./vendor/hibana-postgres-pool-0.7.0.tgz"` を追加します。`hibana build` が取得・固定するため、アプリの `package.json` への追加は不要です。拡張作者が別の拡張から利用する場合は、その配布パッケージの `dependencies` に宣言します。

`createPostgres` が `createPool(Client, configuration)` を呼び出し、その factory の Client と接続設定の検証関数を渡します。Pool はその組み合わせを保持し、他の factory の認証・通信設定を使用しません。Pool の生成時にも設定を検証しますが、その時点ではソケットを作りません。

Pool はリクエスト内で作り、`finally` で `await pool.end()` を呼びます。`allowExitOnIdle`・`maxLifetimeSeconds` は未対応です。Promise／callback、接続失敗・認証失敗・接続待ちのタイムアウトと解放は従来と同じです。

`@hibana/postgres`・`@hibana/postgres-tcp` は本パッケージを選択済みなので、従来どおり `import { Pool } from 'pg'` を使えます。Client だけを使う場合は `postgres-core` の構成で `pool` を省略してください。インストール済みでも自動では追加しません。

Drizzle 0.45.2 の node-postgres アダプターは内部で `pg.Pool` を参照するため、Client を渡す場合も本部品を選択するか、既存プリセットを使ってください。
