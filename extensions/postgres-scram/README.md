# @hibana/postgres-scram

SCRAM 認証を使う PostgreSQL 向けの `pg` プリセットです。Client・Pool・TCP／TLS・SCRAM-SHA-256／PLUS・SHA-256 の証明書ハッシュを、既存の単機能拡張から組み合わせます。アプリ側の `pg.mjs` やローカル拡張は不要です。

## 使う

依存を同梱した tarball を1つ `vendor/` に置き、`hibana.json` に指定します。npm registry には未公開です。アプリの `package.json` への追加は不要です。

`hibana.json`:

```json
{
  "name": "my-api",
  "main": "src/index.ts",
  "extensions": {
    "@hibana/postgres-scram": "./vendor/hibana-postgres-scram-0.7.5-bundle.tgz"
  }
}
```

`hibana build` が拡張を取得し、`hibana-lock.json` を生成します。TCP・TLS・暗号などの下位部品は tarball に同梱され、手動で列挙する必要はありません。lock と tarball を Git に保存し、CI では `hibana build --frozen-lockfile` を使います。[配布者向けの生成手順](../README.md#配布者)。

アプリや依存ライブラリの `pg` import は、ビルド時にこの拡張へ解決されます。Hibana 本体への組み込みや Node.js サーバーは不要です。

```js
import { Client } from 'pg';

// リクエスト内で呼び出す。接続 URL はアプリの Secret から渡す。
export async function checkDatabase(connectionString) {
  const client = new Client({ connectionString });
  try {
    await client.connect();
    return (await client.query('SELECT 1 AS connected')).rows[0];
  } finally {
    await client.end();
  }
}
```

LangChain.js や Drizzle などの依存が使う `pg.Pool` と default export も提供します。[pg API の対応範囲と制限](../postgres/README.md#対応範囲と制限)は共通です。接続先は `hibana egress allow db.example.com:5432` またはコンソールで許可し、接続 URL は Secret に保存します。

## 含む機能と制限

- 直接の依存は core・pool・transport-tls・auth-scram・sha256 の5個。プリセットを含む依存全体は20パッケージ、Wasm は8個です。プリセット自身は JS のみで、ドライバーや認証処理を複製しません。
- MD5 認証、平文パスワード認証、認証省略は拒否します。追加の証明書ハッシュも同梱しません。
- TLS の使用は接続設定に従います。TLS と channel binding を必須にする場合は、接続 URL に `sslmode=require&channel_binding=require` を指定します。TLS の証明書検証は必須です。
- Channel binding の証明書ハッシュは SHA-256 に限定します。RFC 5929 に従う MD5／SHA-1 署名からの SHA-256 選択も含みます。SHA-384 などの証明書には `ERR_PG_CERTIFICATE_DIGEST_UNAVAILABLE` で失敗します。

別の証明書ハッシュや認証が必要なら [個別の部品選択](../postgres-core/README.md)か、対応する全認証・ハッシュを含む [@hibana/postgres](../postgres/README.md) を使います。Client 専用などさらに小さい構成も個別選択で作れます。`pg` alias を持つ複数の拡張は併用できません。
