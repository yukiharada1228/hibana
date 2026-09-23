# 任意のアプリ拡張

**単機能の Wasm 部品14個**、Node 互換6個、PostgreSQL の部品6個とプリセット3個を、独立した npm パッケージとして提供します。Hibana 本体への互換機能追加はなく、選んだ拡張とその依存だけをアプリへ合成します。

新規アプリでは必要な部品を個別に選びます。Hono の HTTP 処理だけなら `extensions` の指定は不要です。Node API が必要な場合だけ互換部品を、DB 接続が必要な場合だけ通信・認証を追加します。

## 基礎機能を単独で選ぶ

下記はすべて、独自の Wasm を1つ持ち、他の Hibana 拡張・Node API に依存しません。

| パッケージ | 公開する機能 |
|---|---|
| [@hibana/tcp](tcp/README.md) | 数値 IP アドレスへの TCP 接続。DNS なし |
| [@hibana/dns](dns/README.md) | ホスト名のアドレス解決。TCP ソケットなし |
| [@hibana/tls](tls/README.md) | TLS の暗号化・復号・証明書検証。ネットワーク操作なし |
| [@hibana/random](random/README.md) | 安全な乱数 |
| [@hibana/sha224](sha224/README.md) | SHA-224 |
| [@hibana/sha256](sha256/README.md) | SHA-256 |
| [@hibana/sha384](sha384/README.md) | SHA-384 |
| [@hibana/sha512](sha512/README.md) | SHA-512 |
| [@hibana/sha512-224](sha512-224/README.md) | SHA-512/224 |
| [@hibana/sha512-256](sha512-256/README.md) | SHA-512/256 |
| [@hibana/md5](md5/README.md) | MD5（既存プロトコル互換用） |
| [@hibana/hmac-sha256](hmac-sha256/README.md) | HMAC-SHA-256 |
| [@hibana/pbkdf2-sha256](pbkdf2-sha256/README.md) | PBKDF2-HMAC-SHA-256 |
| [@hibana/unicode-nfkc](unicode-nfkc/README.md) | Unicode NFKC 正規化 |

例えば SHA-256 だけを使う場合は、その tarball を `vendor/` に置き、`hibana.json` に `"extensions": { "@hibana/sha256": "./vendor/hibana-sha256-0.5.0.tgz" }` と指定します。アプリの `package.json` には追加しません。

```js
import { digest } from '@hibana/sha256';
const hash = digest(new TextEncoder().encode('hello'));
```

この構成には乱数・MD5・HMAC・鍵導出・通信・Node API は入りません。IP 宛ての TCP だけなら `@hibana/tcp`、名前解決だけなら `@hibana/dns` です。TLS 部品は呼び出し側から受け取ったバイト列を処理し、TCP や DNS を自ら呼び出しません。

分割単位は独立した機能です。同じソケットの接続・読み書き・終了処理は、所有権を保つため1部品にまとめます。HMAC 内部の SHA-256、PBKDF2 内部の HMAC、TLS 内部の暗号処理・証明書検証は、その機能を成立させる必須の実装です。

## Node API・PostgreSQL を使う

下記は JS のみです。必要な基礎部品を組み合わせ、既存の API へ合わせます。

| パッケージ | 役割・依存 |
|---|---|
| [@hibana/node-buffer](node-buffer/README.md) | npm buffer の Buffer |
| [@hibana/node-events](node-events/README.md) | npm events の EventEmitter |
| [@hibana/node-process](node-process/README.md) | ブラウザ用 process shim。ホスト環境変数・OS 操作は対象外 |
| [@hibana/node-stream](node-stream/README.md) | Readable／Writable／Duplex／Transform／PassThrough／pipeline／compose／finished の個別入口と、通常の stream API |
| [@hibana/node-net](node-net/README.md) | Node 形式の TCP。tcp・dns・stream・buffer を組み合わせる |
| [@hibana/node-tls](node-tls/README.md) | Node 形式の TLS／STARTTLS。node-net・tls・buffer を組み合わせる |
| [@hibana/postgres-core](postgres-core/README.md) | クエリ・型変換・Client。Pool・通信・認証の実装を含まない |
| [@hibana/postgres-pool](postgres-pool/README.md) | 任意の接続プール。core に `pool: createPool` で追加 |
| [@hibana/postgres-transport-tcp](postgres-transport-tcp/README.md) | pg 用 TCP。TLS なし |
| [@hibana/postgres-transport-tls](postgres-transport-tls/README.md) | pg 用 SSLRequest／TLS。証明書ハッシュは含まない |
| [@hibana/postgres-auth-md5](postgres-auth-md5/README.md) | pg の MD5 認証。SCRAM なし |
| [@hibana/postgres-auth-scram](postgres-auth-scram/README.md) | SCRAM 認証。MD5 なし。証明書ハッシュは関数を明示して選ぶ |

Node 形式の `net.connect({host: ...})` は DNS を含みます。Node API を必要としない場合は基礎部品を直接使えます。

Node TCP／TLS は `@hibana/node-stream/duplex` を使います。Socket だけのアプリには `Transform`・`PassThrough`・`pipeline`・`compose` を同梱しません。必要な場合は `@hibana/node-stream/passthrough` などの[機能別入口](node-stream/README.md)を import します。通常の `node:stream` は API をまとめて使う入口として維持します。共有する実装はそのまま使い、拡張パッケージや CLI 設定は増やしていません。

## PostgreSQL の必要な機能を選ぶ

既存ライブラリが `pg` を使い、接続先が SCRAM 認証と SHA-256 の証明書ハッシュを使う場合は、[@hibana/postgres-scram](postgres-scram/README.md) を選べます。Client・Pool・TCP／TLS を設定済みで、アプリ側の接続用 JS やローカル拡張は不要です。

```json
{
  "name": "my-api",
  "main": "src/index.ts",
  "extensions": {
    "@hibana/postgres-scram": "./vendor/hibana-postgres-scram-0.7.5-bundle.tgz"
  }
}
```

アプリや依存ライブラリは `import { Client, Pool } from 'pg'` を使えます。MD5・平文パスワード・認証省略は含めません。必要なパッケージの導入は[配布手順](#利用者向けの配布と移行)を参照してください。

Client だけを使う場合や異なる通信・認証が必要な場合は、[PostgreSQL の組み合わせ方](postgres-core/README.md)に従い、アプリの JS と `hibana.json` で部品を直接選択できます。

| Client 専用の構成 | 依存込みパッケージ数 | Wasm 部品数 |
|---|---:|---:|
| SCRAM＋TLS＋SHA-256 証明書 | 18 | 8 |
| MD5＋TCP | 11 | 3 |

Pool または Drizzle が必要なら `@hibana/postgres-pool` と `pool: createPool` を追加します。パッケージ数は1つ増え、Wasm 部品数は変わりません。上の SCRAM 構成には MD5・SHA-224・SHA-384・SHA-512 系、MD5 構成には SCRAM・TLS・乱数・NFKC が入りません。

`extensions` と拡張の `dependencies` が Wasm 部品の合成範囲を決めます。プリセットで `ssl: false` を指定したり、アプリで Pool を使わなかったりするだけでは、プリセットの依存は外れません。構成を減らすときは個別の部品選択へ切り替えます。

## 複数の機能をまとめて使うプリセット

既存ライブラリの `pg` import に合わせた、設定済みの入口です。接続先に合うものを1つ選びます。

| パッケージ | 含まれる機能 | 依存込みパッケージ数 |
|---|---|---:|
| [@hibana/postgres-scram](postgres-scram/README.md) | Client・Pool・TCP／TLS・SCRAM・SHA-256 証明書ハッシュ | 20 |
| [@hibana/postgres](postgres/README.md) | Client・Pool・TCP／TLS・対応する全認証・証明書ハッシュ | 27 |
| [@hibana/postgres-tcp](postgres-tcp/README.md) | Client・Pool・TCP・対応する全認証。TLS 設定は接続前に拒否 | 19 |

使うプリセットの名前と取得元を、上の例のように `extensions` に指定します。プリセットと独自に `pg` alias を提供する拡張は併用できません。

## 利用者向けの配布と移行

各パッケージは npm registry へ未公開です。PostgreSQL のプリセットは、必要な依存を同梱した tarball（`-bundle.tgz`）を1つ `vendor/` に置き、上記の `hibana.json` から指定します。

```sh
hibana build
# CI では保存済みの hibana-lock.json と一致することを要求する
hibana build --frozen-lockfile
```

アプリの `package.json` に Hibana 拡張は書きません。CLI が `.hibana/extensions/` に取得し、`hibana-lock.json` に依存と整合性情報を保存します。内部の19パッケージと通常の npm 依存は tarball に同梱され、利用者が個別に列挙する必要はありません。`hibana.json`・`hibana-lock.json`・tarball を Git に保存し、`.hibana/` は除外します。同梱版は空のキャッシュでもオフラインで復元でき、アプリの通常の依存は `npm ci` で別途導入します。

`postgres`・`postgres-tcp` にも同じ形式を用意します。1アプリで使用する `pg` プリセットは1つです。内部の拡張は従来の単機能パッケージのままで、Wasm 部品数・権限・コンソールの依存表示も変わりません。

個別部品を直接 import する構成では、必要な未公開部品の tarball とその取得元を `hibana.json` に指定できます。指定した部品はすべて合成対象です。下位の取得元まで利用者に列挙させたくない場合は、配布者が必要な依存だけを持つ拡張として同梱します。同梱版の内部と重なる部品を別途導入すると複数の実体ができるため、同梱版と個別構成は混ぜずに選びます。

既存の構成から移行する場合は、選んだ拡張の名前と取得元を `hibana.json` に移し、アプリの `package.json` から拡張依存を削除して `npm install --ignore-scripts` を実行します。続いて `hibana build` で lock を生成します。旧 `vendor/` ファイルは参照がないことを確認して除去します。[詳細な設定・移行手順](../docs/application-extensions.md)。

CLI は schemaVersion 2 の依存宣言を辿ります。インストールしただけの拡張は有効化せず、暗号の統合 Wasm を自動で追加することもありません。通信先の許可はサーバー管理者が設定します。

0.4.0 の `@hibana/crypto` は廃止し、使うアルゴリズムへ置き換えます。`sha256(data)` は `@hibana/sha256` の `digest(data)`、`randomBytes(n)` は `@hibana/random` の `bytes(n)`、`deriveKey(...)` は `@hibana/pbkdf2-sha256` の `derive(...)` です。`hashByName` の汎用まとめ API はありません。`@hibana/unicode` は `@hibana/unicode-nfkc` へ変更します。

下記のバージョンとその依存を指定して再ビルドします。古い tarball と依存を削除してください。通常の `pg` import はプリセットでそのまま使えます。配列形式の `extensions` は既存構成との互換用に維持しますが、取得・固定は自動では行いません。

独自のローカル拡張で0.6系の `createPostgres` を直接使っていた場合、0.7.0では Pool が自動で付かなくなります。Pool または Drizzle を使う構成には `@hibana/postgres-pool` の依存と `pool: createPool` を追加してください。[移行例](postgres-core/README.md#pool-が必要な場合)。Client だけなら追加不要です。

Node TCP／TLS 0.6.0からは Socket の `.map()`・`.filter()` なども自動では入りません。使う場合は `import "node:stream"` を追加します。通常の読み書き・イベント・バッファ制御には変更ありません。

Node Stream 0.6.1では、機能別入口を共通ファイルから再 export した場合も、未使用の入口をビルド時に除去できます。必要なバイト列変換の初期化と、通常の `node:stream` の初期化は保持します。

Node Stream 0.7.0では、Readable 単独に Writable・Duplex、Writable 単独に Readable・Duplex を同梱しません。型判定用の循環参照だけを移植し、通常の入口と同じコンストラクター・読み書き・終了処理を使います。

現在のバージョンは `postgres`・`postgres-tcp`・`postgres-scram` が0.7.5、`postgres-core`・`postgres-pool` が0.7.0、`postgres-transport-tcp`・`postgres-transport-tls` が0.7.4、`node-net`・`node-tls` が0.6.4、`node-stream` が0.7.0、`tls` が0.5.1です。他の部品のバージョンと、WIT の0.5.0インターフェースは維持しています。各 `package.json` の依存バージョンを揃えてください。配備済みの Wasm に修正を反映するには、アプリの依存更新・再ビルド・再配備が必要です。

## 配布者

Node.js 24 以降、Rust の `wasm32-wasip2`、WASI SDK 34（TLS の ring 用 clang）、アプリ合成に WAC 0.11.0 を使用します。`extensions/Cargo.toml`・`Cargo.lock` と npm workspace が配布者用のビルド・依存を共通管理します。Hibana 本体の Cargo workspace とは独立しています。

共有ビルド処理のツールは workspace 直下、各パッケージのソースが直接 import するビルド用ライブラリはそのパッケージの `devDependencies` にバージョン固定で宣言します。間接依存が上位の `node_modules` に配置されることには依存しません。配布時に組み込むライブラリと、利用者側でも必要な `dependencies` は分けて管理します。

```sh
npm ci --ignore-scripts --prefix extensions
WASI_SDK_PATH=/path/to/wasi-sdk-34 npm run build --prefix extensions
cargo test --locked --workspace --lib --manifest-path extensions/Cargo.toml
cd extensions
npm pack --ignore-scripts --workspaces
```

通常の tarball に JS・各部品の Wasm／WIT・ライセンスを含めます。Node／PostgreSQL 自身は JS のみです。プリセットの依存同梱版は、上のビルド後にリポジトリのルートから次のように生成します。

```sh
npm run pack:bundle --prefix extensions -- postgres-scram ../.local/extension-bundles
```

第1引数は `postgres-scram`・`postgres`・`postgres-tcp` のいずれかです。出力先は npm script の実行ディレクトリである `extensions/` からの相対パスです。通常版の配布内容だけを一時ディレクトリへ展開し、依存を揃えて npm 標準の同梱形式で梱包します。配布者の `npm ci` で取得済みのキャッシュを使い、途中の install／pack フックは実行しません。同梱版は内部の `node_modules` に依存部品の Wasm／WIT とライセンスも含みます。通常版とはファイル名を分け、各部品のバージョンは維持します。

利用者のインストールに Rust や install hook は不要です。PostgreSQL のプリセットは公開部品の組み合わせだけを持ち、クエリ処理や認証コードを重複させません。

検証は `scripts/test-extension-bundles.mjs`（3プリセットの依存同梱・空キャッシュからのオフライン install／ci・JS 解決）、`scripts/test-extension-boundaries.mjs`（単機能・非同梱）、`scripts/test-tcp-tls-extension.mjs`（実通信）、`scripts/test-postgres-compatibility.mjs`（DB・ORM）、`scripts/test-application-extensions.mjs`（汎用合成）。[設計](../docs/application-extensions.md)・[検証結果](../docs/postgres-compatibility.md)も参照してください。
