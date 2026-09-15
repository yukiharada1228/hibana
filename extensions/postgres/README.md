# @hibana/postgres

Hibana アプリに追加する PostgreSQL ドライバーです。`pg@8.23.0` の JS クエリ処理を再利用し、SCRAM 認証の SHA-256／HMAC／PBKDF2 と乱数を Rust／Wasm で提供します。通信には別の任意拡張 `@hibana/node-net` を使用します。Hibana 本体・Worker・標準 CLI に DB ドライバーや Node API を追加しません。

## 利用方法

現在は npm registry へ未公開です。配布 tarball を受け取り、アプリにインストールします。

```sh
npm install ./hibana-node-net-0.1.0.tgz ./hibana-postgres-0.1.0.tgz
```

```json
{
  "name": "my-api",
  "main": "src/index.ts",
  "extensions": ["@hibana/node-net", "@hibana/postgres"]
}
```

`import { Client, Pool } from 'pg'` はビルド時にこのパッケージへ解決されます。`pg` を別途導入していても、Wasm に入る実装は本パッケージに固定された版です。Node.js 向けと同じ全機能の実装ではありません。

```ts
import { Hono } from 'hono';
import { Pool } from 'pg';
import { drizzle } from 'drizzle-orm/node-postgres';
import { users } from './schema';

const app = new Hono();
app.get('/users', async (c) => {
  const pool = new Pool({
    connectionString: c.env.DATABASE_URL,
    ssl: { ca: c.env.DATABASE_CA },
    max: 2,
  });
  try {
    const db = drizzle({ client: pool });
    return c.json(await db.select().from(users));
  } finally {
    await pool.end();
  }
});
export default app;
```

接続・Pool はリクエスト内で作り、`finally` で閉じます。リクエストを跨ぐ接続再利用はしません。スキーマとマイグレーションはアプリ側で管理し、マイグレーションは管理用の実行環境から適用してください。ゲストからホスト上の migration ファイルを読む機能はありません。

`sslmode`・`sslrootcert` などを含む接続 URL と `ssl.ca` は併用しないでください。pg の URL 解析が `ssl` オブジェクトを上書きするためです。認証情報は Hibana の環境変数・Secrets から渡します。ゲストの `process.env` に基盤の環境変数は渡しません。

## 対応範囲と制限

- パラメーター付きクエリ、Client／リクエスト内 Pool、pg の型変換、トランザクションを対象にします。
- TLS は証明書検証必須です。CA は PEM 文字列またはバイト列で渡します。`pgpass`、証明書ファイル、Unix socket、ネイティブ libpq、クライアント証明書、SCRAM channel binding、direct TLS negotiation は明示的に拒否します。
- WASI 0.2 に TCP_NODELAY がないため、pg 内部の `setNoDelay(true)` は性能上のヒントとして省略します。公開 `net.Socket` の `setNoDelay` は引き続き未対応です。
- Node プロセスの寿命を操作できないため、Pool の再取得時の `ref` は処理を必要としません。`unref`、`allowExitOnIdle`、`maxLifetimeSeconds` は未対応です。
- SCRAM の反復回数は 1〜100,000、暗号入力は最大 1 MiB に制限します。接続タイムアウトは既定 5 秒、クエリ待機は既定 10 秒です。ランタイムの実行制限も適用されます。
- パッケージは通信権限を増やしません。配備先で管理者が接続先を許可する必要があり、現在の Hibana は private／loopback 宛てと `hibana dev` の外向き通信を拒否します。

動作確認した版と再現手順は [検証報告](../../docs/postgres-compatibility.md)を参照してください。

## 配布者向け

```sh
cd extensions/postgres
npm ci --ignore-scripts
npm pack
```

リポジトリの Rust toolchain と `wasm32-wasip2` が必要です。作者用ビルドが pg 内部の暗号・stream を明示的に差し替え、SASLprep の NFKC を Rust へ委譲し、必要な JS だけを同梱します。元の pg のファイルは変更しません。Node の `crypto`／`fs`／`dns` の汎用 polyfill は登録しません。依存を更新するときは適応箇所と実機試験を再確認してください。利用者のインストール時にはビルドフックを実行せず、Rust・C コンパイラーは不要です。
