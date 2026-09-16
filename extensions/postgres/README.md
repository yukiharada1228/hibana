# @hibana/postgres

0.7.4 の PostgreSQL プリセットです。公開された core・Pool・通信・認証部品を組み合わせ、従来の設定との互換性を維持します。`pg@8.23.0` の JS クエリ処理を再利用し、SCRAM 認証の SHA-256／HMAC／PBKDF2 と乱数を Rust／Wasm で提供します。通信は `@hibana/node-net`・`@hibana/node-tls`、暗号は `@hibana/random`・`@hibana/sha256`・`@hibana/hmac-sha256`・`@hibana/pbkdf2-sha256` など、NFKC 正規化は `@hibana/unicode-nfkc` に委譲します。必要な方式だけ選ぶ場合は [postgres-core の組み合わせ方](../postgres-core/README.md)を使用してください。このプリセット自体は組み合わせの JS のみで、pg のクエリ処理や独自の Wasm を含みません。Hibana 本体・Worker・標準 CLI に DB ドライバーや Node API を追加しません。

## プリセットを利用する場合

このプリセットは Pool・全認証・対応する証明書ハッシュをまとめて含みます。最小構成を作る場合は [postgres-core の個別選択](../postgres-core/README.md)から始めてください。`ssl: false` や Client だけの利用では、このプリセットの依存は外れません。

現在は npm registry へ未公開です。配布 tarball を受け取り、アプリにインストールします。依存する部品の版は npm dependencies に宣言済みです。未公開のため、インストール時には [必要な27パッケージの tarball](../README.md) を指定します。

```sh
npm install --save-exact --ignore-scripts ./vendor/hibana-*.tgz
```

```json
{
  "name": "my-api",
  "main": "src/index.ts",
  "extensions": ["@hibana/postgres"]
}
```

`import { Client, Pool } from 'pg'` はビルド時にこのパッケージへ解決されます。`pg` を別途導入していても、Wasm に入る実装は本パッケージに固定された版です。Node.js 向けと同じ全機能の実装ではありません。

schemaVersion 2 対応の CLI が manifest の依存宣言から必要な拡張を自動的に取り込みます。アプリの `extensions` は上記1行で済みます。

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

TLS を同梱しない構成には [@hibana/postgres-tcp](../postgres-tcp/README.md) を選べます。設定で TLS を無効にしただけでは通常版から TLS の部品は外れないため、配布物ごと選択します。TCP 専用版は TLS 設定や TLS 必須 URL を接続前に拒否します。

## 対応範囲と制限

- パラメーター付きクエリ、Client／リクエスト内 Pool、pg の型変換、トランザクションを対象にします。
- 不正な `port` は Client／Pool の生成時に拒否します。接続開始時に通信部品が同期例外を投げた場合も、接続失敗の通知・ソケット解放・`end()` の完了を保証します。
- `pipeline: true` では、`end()` が投入済みクエリの完了を待ってから接続を閉じます。Promise／callback、SQL エラー後の後続クエリ、タイムアウト無効時の終了を検証しています。
- TLS は証明書検証必須です。CA は PEM 文字列またはバイト列で渡します。`pgpass`、証明書ファイル、Unix socket、ネイティブ libpq、クライアント証明書、direct TLS negotiation は明示的に拒否します。
- SCRAM-SHA-256-PLUS の channel binding に対応します。`channel_binding` は接続 URL または Client／Pool の設定で `disable`・`prefer`（既定）・`require` を指定し、URL の指定を優先します。`enableChannelBinding: true` は `prefer`、`false` は `disable` 相当です。`require` は TLS と検証済みのサーバー証明書を要求し、通常の SCRAM・平文パスワード・MD5・trust 認証への切り替えを拒否します。接続後の `client.channelBindingUsed` で、サーバーの SCRAM 証明まで検証したか確認できます。
- WASI 0.2 に TCP_NODELAY がないため、pg 内部の `setNoDelay(true)` は性能上のヒントとして省略します。公開 `net.Socket` の `setNoDelay` は引き続き未対応です。
- Node プロセスの寿命を操作できないため、Pool の再取得時の `ref` は処理を必要としません。`unref`、`allowExitOnIdle`、`maxLifetimeSeconds` は未対応です。
- SCRAM の反復回数は 1〜100,000、暗号入力は最大 1 MiB に制限します。接続タイムアウトは既定 5 秒、クエリ待機は既定 10 秒です。ランタイムの実行制限も適用されます。
- パッケージは通信権限を増やしません。配備先で管理者が接続先を許可する必要があり、現在の Hibana は private／loopback 宛てと `hibana dev` の外向き通信を拒否します。

動作確認した版と再現手順は [検証報告](../../docs/postgres-compatibility.md)を参照してください。

## 配布者向け

```sh
cd extensions
npm ci --ignore-scripts
npm pack --workspace @hibana/postgres
```

このパッケージのビルドは Node.js のみです。依存する Wasm 部品は[共通手順](../README.md)でビルドします。core と SCRAM の作者用ビルドが固定した pg の内部依存を引数へ移し、SASLprep の NFKC を Rust へ委譲します。本パッケージはその公開部品を組み合わせます。元の pg のファイルは変更しません。Node の `crypto`／`fs`／`dns` の汎用 polyfill は登録しません。依存を更新するときは適応箇所と実機試験を再確認してください。利用者のインストール時にはビルドフックを実行せず、Rust・C コンパイラーは不要です。
