# Hibana CLI

Hibanaの実行契約はWebAssembly Componentです。Honoは対応するJavaScriptフレームワークの一つで、専用SDKのインポートは必要ありません。

```ts
import { Hono } from "hono";

const app = new Hono();
app.get("/", c => c.json({ message: "Hello Hibana" }));
export default app;
```

素のJavaScript/TypeScriptでも、`export default { fetch(request, env, context) { ... } }`を使えます。CLI内部の共通変換層がFetch APIをWASI HTTPへ接続します。[Honoのfetch契約](https://hono.dev/docs/api/hono#fetch)をそのまま利用します。

## インストールとテンプレート

CLIはNode.js 24以上が必要です。現時点では開発チェックアウトから利用します。npmでの公開はしていません。

```bash
npm ci --prefix sdk
cargo build --release -p faas-worker
node sdk/src/cli.mjs init my-app --template hono
cd my-app
npm run dev
```

| `--template` | アプリの記述 | 必要なビルドツール |
|---|---|---|
| `hono` | 通常のHonoをdefault export | Node.js、npm、JSコンパイラー依存 |
| `javascript` | Fetch handlerのTypeScript | Node.js、npm、JSコンパイラー依存 |
| `rust` | RustのWASI HTTP handler | Rust、`wasm32-wasip2`ターゲット |
| `go` | GoのWASI HTTP handler | Go 1.25.9以上、固定したcomponentize-go v0.4.2 |

`init`は空のディレクトリに生成します。JS系にはnpm scriptsと開発チェックアウトへの`file:`依存を追加し、`--no-install`でnpmインストールを省略できます。Rust・Goにはnpm依存を生成しません。CLIの実装自体はどの言語でもNode.jsを使用します。

Rust・Go・ビルド済みComponentだけを扱う場合、`npm ci --prefix sdk --omit=optional --omit=dev`でJSコンパイラーとHonoをインストールせずにCLIを使えます。JS向けビルドを追加するときは`npm ci --prefix sdk`を実行してください。

```bash
# リポジトリのルートで実行
rustup target add wasm32-wasip2
node sdk/src/cli.mjs init my-rust --template rust
node sdk/src/cli.mjs dev -c my-rust/hibana.json

node sdk/src/cli.mjs init my-go --template go
node sdk/src/cli.mjs dev -c my-go/hibana.json
node sdk/src/cli.mjs deploy -c my-go/hibana.json
```

Goの`componentize-go`は`go.mod`のtool依存として固定しています。初回のビルドで公式バイナリとGo依存を取得します。バインディングは`bindings/`へ生成し、アプリの`go.mod`やHTTP実装を上書きしません。Goサンプルは生成されたWASI HTTPバインディングを使用します。既存の`net/http.ListenAndServe`アプリを無変更で動かす機能ではありません。[公式ツール](https://github.com/bytecodealliance/componentize-go/tree/v0.4.2)を利用しています。

Rust・GoのプロジェクトにはWIT定義と依存ロックもコピーされるので、生成後のビルドはHibana固有の言語SDKに依存しません。全テンプレートに`GET /`とバイナリを返す`POST /echo`があります。Rust・Goサンプルのecho入力上限は1 MiBです。

## hibana.json

JS系は`main`を指定します。

```json
{
  "name": "my-app",
  "main": "src/index.ts",
  "vars": { "GREETING": "Hello Hibana" },
  "limits": { "memory_mb": 256, "timeout_ms": 15000 }
}
```

その他の言語はビルド手順と出力先を指定します。次はRustの例です。

```json
{
  "name": "my-rust",
  "component": "target/wasm32-wasip2/release/hibana_http_app.wasm",
  "build": {
    "commands": [["cargo", "build", "--release", "--target", "wasm32-wasip2"]],
    "watch": ["src", "Cargo.toml", "Cargo.lock", "wit"]
  }
}
```

- `main`と`component`は排他。ビルド済みWasmは`"component":"./app.wasm"`だけで指定できます。
- `build`は`component`と組み合わせます。`commands`は引数配列のリストで、プロジェクトのディレクトリで順に実行します。暗黙のシェル展開は行いません。失敗時は配備せず、前回の完成済み成果物を保持します。
- `build.watch`はソースファイル・ディレクトリの相対パスです。globは使えません。生成コードを含めないことで再ビルドのループを避けます。設定ファイルと`.dev.vars`は常に監視します。
- WASI HTTP Component専用です。`http`設定は不要で、指定するとエラーになります。bytes handlerや非同期invokeは提供しません。
- `memory_mb`: 1–1024、既定256。
- `memory_mb`は実行内のWasm線形メモリの合計上限です。ホストのHTTPバッファ・コンパイル・コードキャッシュを含むPod全体のメモリとは異なります。Wasm threadsは対象外です。
- `timeout_ms`: 1–30000、既定15000。ホスト処理込みの期限はさらに5秒。
- `fuel`: 任意の正整数。Wasmtimeの命令量上限。
- `vars`: 文字列の環境変数。JSは`fetch`の第2引数（Honoでは`c.env`）、Rust・GoはWASI環境変数として使用します。

CLIはComponentのヘッダーを確認します。WITの一致や全体の検証はランタイム・配備先が行います。`GOOS=wasip1 GOARCH=wasm go build`だけで生成したcore Wasmをそのまま配備することはできません。

## 開発・配備・Secrets

`dev`は完成したComponentを`faas-worker --dev-component`で動かします。`HIBANA_RUNTIME_BIN`または`--runtime`でバイナリを選択でき、開発チェックアウトでは`target/release/faas-worker`を優先します。バイナリがなければRustのreleaseビルドを実行します。ビルド失敗時は起動済みの開発サーバーを維持します。`--no-watch`で監視を無効にできます。

ローカル専用の秘密値は`.dev.vars`にdotenv形式で書きます。値は`vars`より優先します。`.hibana/`と`.dev.vars`をバージョン管理に含めないでください。サーバーのSecretsは`hibana secret put NAME`の標準入力から登録し、次の`deploy`でバージョンへ使用権限を付けます。

`hibana login`には`HIBANA_URL`・`HIBANA_TENANT`・`HIBANA_EMAIL`・`HIBANA_PASSWORD`を使用します。CIでは`HIBANA_TOKEN`を設定できます。配備先を変えた場合、保存済みの別サーバーのトークンは再利用しません。`deploy --version 1.0.0`で版を指定できます。省略時は一意な開発版を採番します。

`hibana rollback`で直前の版へ、`hibana rollback --version 1.0.0`で指定した版へ戻します。環境変数・Secrets・外部データは巻き戻しません。Canaryや重み付き配分はありません。

## 実行モデルと制限

HTTPの契約は`wasi:http/incoming-handler@0.2.3`です。Hono・JS系はesbuildでバンドルし、ComponentizeJS/StarlingMonkeyでComponentに変換します。変換層は`src/javascript.mjs`に閉じており、Rust・Go・既存Componentの配備はJSエンジンを経由しません。`waitUntil`はWASI実行の完了と資源制限の対象です。

本番Workerとdevは同じRust実行モジュールを使います。ただしdevは認証なしのloopback HTTPサーバーで、DB・配布・課金・分散処理はありません。外向き通信はdevでは拒否、本番では管理者の承認が必要です。JSのPOSTなどの入力は変換層でバッファします。レスポンスはストリーム可能ですが、WebSocket・完全なNode.js互換・Workers Bindingは提供しません。Preview 1単体や任意のWIT worldも未対応です。

配備APIの複数操作は単一トランザクションではありません。承認失敗時は新バージョンを有効化しませんが、varsはコンポーネント共通設定として更新されます。失敗後は原因を直して再配備してください。コンポーネント共通varsを含む原子的な切替は今後の課題です。

## オンプレでの役割分担

基盤管理者がKubernetes上のHibana、管理APIのHTTPS、アプリ用DNS/TLS、テナントを用意します。アプリ開発者は配布された管理APIのURLとテナントの認証情報を設定して、同じ`hibana`コマンドで配備します。アプリの配備にkubeconfigやクラスタ管理権限は不要です。現在のdeployはテナント内の管理権限を使用します。

Wranglerを参考にするのは、この短い開発・配備の流れです。サーバーの契約はWASI HTTPに統一し、Hono・JS/TS・Go・Rustのどれも共通の制限と認証を通します。ビルド工程と実行工程の信頼境界、本番前の検証項目は[セキュリティ境界](../docs/security.md)を参照してください。
