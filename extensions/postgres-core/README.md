# @hibana/postgres-core

PostgreSQL のクエリ・型変換・Client を提供する、0.7.0 の JS 部品です。Pool、独自の Wasm、TCP／TLS、MD5／SCRAM の実装は含みません。通信と認証は `createPostgres` に渡します。Hibana 本体・CLI に専用の設定やプラグイン登録機構を追加しません。

## 構成の決め方

接続先に必要な通信方式・認証方式・証明書ハッシュを選び、Client を使う構成から始めます。Pool または Drizzle を使う場合に Pool 部品を追加します。

部品を変更するときは、次の3つを揃えます。

1. `pg.mjs` の import と `createPostgres` に渡す部品。
2. `hibana.extension.json` の `dependencies`。
3. `package.json` の依存と lockfile。

JS の import だけを削除しても、マニフェストの依存に残した Wasm 部品は合成されます。使用しなくなった部品は依存宣言からも外してください。以下は SCRAM＋TLS＋SHA-256 証明書の Client 専用構成です。

## SCRAM と SHA-256 の証明書だけを使う

アプリ内の `database/pg.mjs` で必要な部品を import します。

```js
import { createPostgres } from '@hibana/postgres-core';
import { tcpTls } from '@hibana/postgres-transport-tls';
import { scramSha256 } from '@hibana/postgres-auth-scram';
import { digest } from '@hibana/sha256';

const pg = createPostgres({
  transport: tcpTls,
  authentication: {
    scram: scramSha256({ certificateDigests: { 'SHA-256': digest } }),
  },
});
export const { Client, types } = pg;
export default pg;
```

同じ `database/` に、通常のローカル拡張マニフェストを置きます。アプリの `import ... from 'pg'` をこの構成へ解決します。Drizzle を使う場合は後述の Pool も追加してください。

`database/hibana.extension.json`:

```json
{
  "schemaVersion": 2,
  "runtime": "wasi:http/incoming-handler@0.2.3",
  "dependencies": [
    "@hibana/postgres-core",
    "@hibana/postgres-transport-tls",
    "@hibana/postgres-auth-scram",
    "@hibana/sha256"
  ],
  "aliases": { "pg": "./pg.mjs" }
}
```

`database/package.json`:

```json
{
  "private": true,
  "type": "module",
  "dependencies": {
    "@hibana/postgres-core": "0.7.0",
    "@hibana/postgres-transport-tls": "0.7.3",
    "@hibana/postgres-auth-scram": "0.6.0",
    "@hibana/sha256": "0.5.0"
  }
}
```

アプリの `hibana.json` では `"extensions": ["./database"]` を指定します。上記とその依存の tarball をアプリの `vendor/` に揃え、ルートで `npm install --save-exact --ignore-scripts ./vendor/hibana-*.tgz` を実行します。CLI は `database/` から上位の `node_modules` にあるパッケージを解決できます。npm registry には未公開です。

この構成の Wasm 部品は TCP・DNS・TLS・乱数・SHA-256・HMAC-SHA-256・PBKDF2-SHA-256・NFKC の8個です。MD5・SHA-224・SHA-384・SHA-512 系は含みません。インストール済みの別パッケージがあっても、依存に列挙しなければ合成しません。

## Pool が必要な場合

0.7.0 から Pool は別の [@hibana/postgres-pool](../postgres-pool/README.md) です。上記の構成は Client 専用で、返される `pg` オブジェクトに `Pool` はありません。Pool を使う場合は以下を追加します。

- `import { createPool } from '@hibana/postgres-pool'` と、`createPostgres` の引数 `pool: createPool`。
- ローカル拡張マニフェストの `dependencies` に `@hibana/postgres-pool`。
- `package.json` の依存に `"@hibana/postgres-pool": "0.7.0"` と、その tarball。
- 必要に応じて `export const { Client, Pool, types } = pg`。

これで同じ通信・認証設定を持つ Pool を利用できます。設定の検証と失敗時の処理は Client と共通です。Drizzle 0.45.2 の node-postgres アダプターは内部で `pg.Pool` を参照するため、Drizzle では Pool も選択してください。既存の `postgres`・`postgres-tcp` プリセットは Pool を含み、従来の API を維持します。

## 選択できる部品

| 部品 | 渡す値 | 役割 |
|---|---|---|
| `@hibana/postgres-pool` | `pool: createPool` | 任意の接続プール。省略した構成に pg-pool は入らない |
| `@hibana/postgres-transport-tcp` | `transport: tcp` | TCP 専用。TLS の設定は接続前に拒否 |
| `@hibana/postgres-transport-tls` | `transport: tcpTls` | PostgreSQL SSLRequest による TLS。証明書検証必須 |
| `@hibana/postgres-auth-scram` | `authentication.scram: scramSha256(...)` | SCRAM-SHA-256／PLUS。必要な暗号と NFKC のみ依存 |
| `@hibana/postgres-auth-md5` | `authentication.md5: md5` | 既存の MD5 認証。SCRAM・乱数・NFKC は依存しない |

`authentication` に指定しない認証方式は拒否します。平文パスワードと認証省略を使用する場合だけ、それぞれ `cleartext: true`・`trust: true` を明示します。この2つは暗号計算を行わないため、プロトコル内の許可スイッチです。既定ではどちらも無効です。通信や認証を省略しても、インストール済みパッケージから自動で補完しません。

`certificateDigests` には証明書の署名方式に対応するハッシュ関数を登録します。名前は `SHA-224`・`SHA-256`・`SHA-384`・`SHA-512`・`SHA512-224`・`SHA512-256`。RFC 5929 に従い MD5／SHA-1 の署名は SHA-256 を使います。証明書の更新で署名方式が変わったら、その部品の追加と再ビルドが必要です。未選択のハッシュが必要な接続は `ERR_PG_CERTIFICATE_DIGEST_UNAVAILABLE` で失敗し、弱い方式への自動切替は行いません。

Client／Pool の `channel_binding: 'require'` または接続 URL の `channel_binding=require` は、SCRAM-SHA-256-PLUS と検証済みの証明書・サーバー証明を必須にします。`prefer` はサーバーが PLUS を提供しない場合の通常 SCRAM を許容しますが、PLUS に必要なハッシュの未選択はエラーです。`disable` では証明書ハッシュを使用しません。

同じアプリで複数の `createPostgres` を呼び出せます。認証方式の選択と部品の関数は factory 作成時に保持し、後から設定やメソッドを差し替えても既存 factory の選択は変わりません。通信・SCRAM 部品にはクラスのインスタンスも渡せます。プロトタイプのメソッドと `this` を維持するため、インスタンス内部の状態はそのインスタンスに属します。factory 間で状態も分離したい場合は、それぞれに別のインスタンスを渡してください。

接続中の認証処理が例外や Promise の拒否で失敗すると、`Client.connect()`・`Pool.connect()` はそのエラーで失敗し、ソケットも閉じます。callback 形式でもエラーを1回通知します。接続はリクエスト内で作り、`finally` で閉じてください。[API の対応範囲](../postgres/README.md#対応範囲と制限)。

`port` は1〜65535の整数または数字だけの文字列を指定します。未指定時は pg の既定値5432を使います。0・範囲外・小数・不正な文字列・null などは `Client`／`Pool` の生成時に `ERR_SOCKET_BAD_PORT` で拒否し、通信部品を生成しません。接続 URL の設定を優先し、URL の `?port=` で指定された値も検証します。

独自の通信部品の `setNoDelay()`／`connect()` が同期例外を投げた場合も、`connect()` の Promise／callback に元のエラーを1回通知してソケットを閉じます。接続開始は pg のエラー・終了通知の登録後に行うため、`finally` の `end()` や接続待ちのクエリも未完了のまま残りません。

`Client.connect()` が完了する前に `Client.end()` を呼んだ場合、接続待ちは `ERR_PG_CONNECTION_CANCELLED` で失敗し、`end()` はソケットの終了を待って完了します。Promise と callback の両形式に対応し、繰り返し `end()` を呼んでも接続結果の通知は1回です。接続完了後の通常の終了処理は従来どおりです。

接続後に `pipeline: true` でクエリを投入してから `end()` を呼ぶと、投入済みクエリの結果を受け取ってから接続を閉じます。認証中のキャンセルとは処理を分け、終了待ちの間もクエリの完了応答を処理します。`query_timeout: 0` でも、成功・SQL エラーの各結果と終了を Promise／callback に1回ずつ通知します。

切断・認証失敗の後でパスワード取得やハッシュ計算が終わっても、認証を再開したり認証情報を送信したりしません。重複した認証要求、応答を送る前の認証成功通知、順序が不正な SCRAM メッセージは拒否します。

独自の SCRAM 部品は pg と同じ呼び出し規約に従います。`startSession(mechanisms, stream, maxIterations)` はセッションを同期的に返し、`finalizeSession(session, serverData)` はサーバー証明を同期的に検証して、不正なら例外を投げます。この2つで Promise を返す実装はエラーになります。`continueSession(session, password, serverData, stream)` は Promise を返せます。MD5 関数とパスワード取得関数も同期・非同期の両方に対応します。

## 既存構成との関係

`@hibana/postgres` と `@hibana/postgres-tcp` は、これらの公開部品を組み合わせるプリセットとして維持します。旧来の import と設定で動きます。認証と証明書ハッシュを限定する場合は上記のローカル拡張を使い、プリセットを `extensions` から外します。同じ `pg` alias を二重に有効化すると CLI が拒否します。

作者用 `extensions/build-postgres.mjs` は固定した pg 8.23.0 を、依存を引数で受け取る関数へ移植します。認証の順序と終了処理は `src/authentication.mjs` に集約し、使わなくなった上流 Client の認証ハンドラーと pgpass の処理は配布物から除去します。SCRAM 自体の計算・証明検証は認証部品に委譲します。暗号・通信のグローバルな登録や実行時のソース書き換えは行いません。上流の想定したコードが変わるとビルドを停止します。利用者には生成済み JS と上流ライセンスだけを配布します。
