# PostgreSQL・Drizzle の Wasm 検証

2026-09-15、macOS arm64、Node.js 24.12.0 で検証しました。任意拡張 `@hibana/postgres` と `@hibana/node-net` を同梱した Hono アプリから、**Wasm 上で PostgreSQL への接続・認証・クエリと Drizzle ORM の操作を確認しました。** Hibana 本体・標準 CLI・Worker の依存や通信権限は増やしていません。

## 実行した試験

| 対象 | 結果 |
| --- | --- |
| tarball のインストール | コンパイル済み Wasm と JS を利用。インストールフック・Rust ソース・C コンパイラーは不要 |
| CLI の Wasm ビルド | Hono 4.13.7 + pg 8.23.0 の適応版 + Drizzle 0.45.2 を合成 |
| Control Plane の配備検証 | 成功。ホスト import は標準 WASI のみ |
| TCP／TLS の PostgreSQL 接続 | Wasmtime 36.0.14 から PostgreSQL 16.15 へ接続。SSLRequest から TLS に移行しカスタム CA を検証 |
| SCRAM-SHA-256 | 成功。Unicode のマッピング・NFKC 正規化が必要なパスワードでも認証 |
| pg のクエリ処理 | パラメーター、UTF-8、JSONB、bytea、callback、配列形式の行、名前付き prepared statement、SQL エラー後の再利用 |
| リクエスト内 Pool | 最大 2 接続で 8 クエリを処理し、終了時に解放 |
| Drizzle ORM | TCP／TLS の両方で INSERT／UPDATE／SELECT／DELETE、コミット・ロールバック |
| 失敗時処理 | 誤ったパスワード、信頼しない CA、ホスト名不一致、TLS 検証の無効化、クエリタイムアウトを拒否 |
| リソース解放 | 1 回の Wasm 実行内で 40 接続を順次開閉。ソケット上限を累積消費しない |
| 対照試験 | 通常の Node.js + 元の pg でも同じ一時 DB に TCP／TLS 接続・認証・クエリが成功 |

追加の拒否テストで、ホストファイル、ネイティブ libpq、クライアント証明書、channel binding、未対応の Pool 設定と SCRAM 反復制限も確認しました。Hibana の dev runtime が TCP 接続を `EACCES` で拒否すること、CLI の 44 件と既存 TCP／TLS E2E が通ることも確認しています。

## 検証範囲の境界

DB への通信試験は、テスト専用にネットワークを許可した **汎用 Wasmtime** で行います。本番 Hibana のネットワーク設定を変更した試験ではありません。最終 Wasm は実際の Control Plane で検証し、Hibana の dev runtime では外向き通信が拒否されることを別途確認します。

Hibana は private／loopback 宛てを拒否します。パッケージを追加しても、Docker 内部や同一 Kubernetes 内の DB に自動的に接続できるわけではありません。配備時には、現在のポリシーで利用可能な接続先について管理者による egress 許可が必要です。本番の DB へのデプロイ・接続試験は行っていません。

全 DB 試験は新規の一時コンテナだけを使用し、DB を tmpfs に置きます。終了時にコンテナ・認証情報・証明書を削除します。既存 DB と Kubernetes 環境は変更しません。

## 追加した実装

以前は `@hibana/node-net` だけで無変更の `pg` をビルドし、Node 組み込み API 6 種の不足で失敗していました。解決は [PostgreSQL 用の任意拡張](../extensions/postgres/README.md)で行いました。アプリの `import ... from 'pg'` をこの適応版へ解決します。

| 問題 | 拡張側の対応 |
| --- | --- |
| `crypto` と JS エンジンにない NFKC | 認証用 SHA-256／HMAC／PBKDF2、乱数、Unicode 正規化を Rust Component に実装 |
| `util`／`util/types` | pg 内部で必要な通知と日付判定だけを局所的に提供 |
| `fs`／`path`／ネイティブ用 DNS | pgpass とホストファイルへの依存を除外。非対応経路を明示的に拒否し、通常の DNS は WASI sockets に任せる |
| `setNoDelay` | pg 専用 stream では性能上のヒントとして省略。汎用 `net.Socket` の対応範囲は変更しない |
| Pool の認証情報 | pg が非列挙にする password を明示的に引き継ぐ |
| TLS エラーが切断通知に隠れる | TLS エラーを先に通知してから元のソケットを閉じる |

Rust 暗号ライブラリの利用箇所と反復回数・入力サイズを制限します。PBKDF2 の既知ベクトルと上限を Rust テストで確認します。pg のプロトコル・型変換・SCRAM の検証処理を再利用し、Node.js 全体の互換実装は提供しません。

Drizzle は公開の [node-postgres アダプター](https://orm.drizzle.team/docs/get-started-postgresql)を使用しています。確認した範囲は表の CRUD とトランザクションです。他の ORM、COPY、長寿命の通知待機、すべての pg API、性能上限は検証対象外です。

## 再現手順

Node.js 24 以降、Docker、OpenSSL、Rust toolchain、WAC、SDK 依存、ビルド済みの両拡張・Control Plane・Worker・Wasmtime CLI が必要です。[通信拡張のビルド手順](../extensions/node-net/README.md#配布者向けビルドと検証)も参照してください。

```sh
npm ci --prefix sdk
npm ci --ignore-scripts --prefix extensions/node-net
npm ci --ignore-scripts --prefix extensions/postgres
npm ci --ignore-scripts --prefix scripts/fixtures/postgres
# node-net の Rust 部品は別途ビルド（Wasm 対応 clang が必要）。
npm run build --prefix extensions/postgres
docker pull postgres:16
WASMTIME_BIN=/absolute/path/to/wasmtime \
HIBANA_TEST_CP_BIN=target/debug/hibana-control-plane \
HIBANA_TEST_RUNTIME_BIN=target/release/hibana-worker \
node scripts/test-postgres-compatibility.mjs
```

[受入試験](../scripts/test-postgres-compatibility.mjs)は実 tarball と lockfile から一時アプリを組み立て、Wasm の HTTP 応答と DB 操作結果を検査します。**終了コード 0 は全受入試験の成功**です。以前の「ビルド失敗を記録するだけ」の診断から置き換え、CI にも登録しました。

出力は `.local/verification/postgres/report.json` と `hibana-build.log` です。`HIBANA_PG_REPORT_DIR` で出力先、`HIBANA_PG_IMAGE` でローカル PostgreSQL イメージを変更できます。使用した image ID と tarball の integrity を報告書に記録します。

ORM の [schema](../scripts/fixtures/postgres/schema.mjs)と[初期マイグレーション](../scripts/fixtures/postgres/migrations/0001_probe.sql)を fixture として維持します。マイグレーションは試験の管理側から一時 DB に適用します。Hibana の管理用 DB の SeaORM／マイグレーションとは独立しています。
