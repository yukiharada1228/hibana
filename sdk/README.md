# Hibana CLI

Hibanaの実行契約はWebAssembly Componentです。Honoは対応するJavaScriptフレームワークの一つで、専用SDKのインポートは必要ありません。

```ts
import { Hono } from 'hono'

const app = new Hono()

app.get('/', (c) => c.text('Hello from Hono on Hibana 🔥'))

export default app
```

素のJavaScript/TypeScriptでも、`export default { fetch(request, env, context) { ... } }`を使えます。CLI内部の共通変換層がFetch APIをWASI HTTPへ接続します。[Honoのfetch契約](https://hono.dev/docs/api/hono#fetch)をそのまま利用します。

## インストールとテンプレート

CLIはNode.js 24以上が必要です。CLI・ローカル実行ランタイム・Kubernetes基盤は別々に導入します。開発者のPCに基盤のソースやDocker/kubectlは不要です。[リモートCLI構成・配布・オンプレ接続](../docs/remote-cli.md)に全手順があります。配布先はGitHub Releasesです。npmレジストリへの公開は無効にしています。

```bash
npm install -g https://github.com/yukiharada1228/hibana/releases/download/v0.1.0/hibana-cli-0.1.0.tgz
hibana runtime install
hibana init my-app --template hono
cd my-app
hibana dev
```

| `--template` | アプリの記述 | 必要なビルドツール |
|---|---|---|
| `hono` | 通常のHonoをdefault export | Node.js、npm、JSコンパイラー依存 |
| `javascript` | Fetch handlerのTypeScript | Node.js、npm、JSコンパイラー依存 |
| `rust` | RustのWASI HTTP handler | Rust、`wasm32-wasip2`ターゲット |
| `go` | GoのWASI HTTP handler | Go 1.25.9以上、固定したcomponentize-go v0.4.2 |

`init`は空のディレクトリに生成します。JS系にはnpm scriptsとCLIと同じバージョンのGitHub Release URLを指定した`@hibana/cli`依存を追加します。閉域環境のtarballや開発用ディレクトリは`--cli-package PATH`で明示できます。`--no-install`でnpmインストールを省略できます。Rust・Goにはnpm依存を生成しません。CLIの実装自体はどの言語でもNode.jsを使用します。

Rust・Go・ビルド済みComponentだけを扱う場合、`npm ci --prefix sdk --omit=optional --omit=dev`でJSコンパイラーとHonoをインストールせずにCLIを使えます。JS向けビルドを追加するときは`npm ci --prefix sdk`を実行してください。

```bash
# リポジトリのルートで実行
rustup target add wasm32-wasip2
node sdk/src/cli.mjs init my-rust --template rust
node sdk/src/cli.mjs dev -c my-rust/hibana.json --runtime /opt/hibana/bin/hibana-worker

node sdk/src/cli.mjs init my-go --template go
node sdk/src/cli.mjs dev -c my-go/hibana.json --runtime /opt/hibana/bin/hibana-worker
node sdk/src/cli.mjs deploy -c my-go/hibana.json
```

Goの`componentize-go`は`go.mod`のtool依存として固定しています。初回のビルドで公式バイナリとGo依存を取得します。バインディングは`bindings/`へ生成し、アプリの`go.mod`やHTTP実装を上書きしません。Goサンプルは生成されたWASI HTTPバインディングを使用します。既存の`net/http.ListenAndServe`アプリを無変更で動かす機能ではありません。[公式ツール](https://github.com/bytecodealliance/componentize-go/tree/v0.4.2)を利用しています。

Rust・GoのプロジェクトにはWIT定義と依存ロックもコピーされるので、生成後のビルドはHibana固有の言語SDKに依存しません。Honoテンプレートは公式の最小サンプルの応答テキストを変更した`GET /`だけです。JavaScript・Rust・Goには`GET /`とバイナリを返す`POST /echo`があります。Rust・Goサンプルのecho入力上限は1 MiBです。

## アプリと基盤の操作

```bash
hibana dev                         # ローカル開発。Ctrl+Cで停止
hibana deploy                      # hibana.jsonのアプリを配備
hibana list                        # 現在のテナントのアプリ
hibana delete                      # hibana.jsonのnameを削除
hibana delete my-app --dry-run      # 削除対象を確認
hibana delete --name my-app --yes   # 名前を指定して削除
hibana delete --all --yes           # 現在のテナントの全アプリ
hibana list --all-tenants
hibana delete --all --all-tenants --yes
# 基盤管理者のみ: 既存オンプレのKubernetes資格情報で操作
hibana platform status --kubeconfig /secure/config --context onprem
hibana platform stop --kubeconfig /secure/config --context onprem
hibana platform start --kubeconfig /secure/config --context onprem
hibana platform uninstall --kubeconfig /secure/config --context onprem --yes
```

`delete [NAME]`、`--name`、`--config/-c`、`--dry-run`はWranglerに近い形式です。`--force`と`--yes/-y`は確認を省略しますが、実行中アプリの削除を拒否するサーバー側の保護は無効化しません。削除にはRead・Adminスコープを持つテナント管理者の認証情報が必要です。Read・Deployだけでは削除できません。ソースコードやビルドツールは不要です。`--all-tenants`はプラットフォーム管理者用の`BOOTSTRAP_ADMIN_TOKEN`が必要です。

削除は公開URLと通常の一覧からアプリを除き、実行履歴とWasm成果物を残します。同じ名前で再デプロイ可能です。基盤の停止はデータを保持します。Kubernetesの導入条件・既存クラスタへの配備・撤去範囲は[基盤管理ガイド](../deploy/kubernetes/README.md)を参照してください。

## hibana.json

JS系は`main`を指定します。

```json
{
  "name": "my-app",
  "main": "src/index.ts",
  "vars": {},
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

`dev`は完成したComponentを`hibana-worker --dev-component`で動かします。`--runtime`、`HIBANA_RUNTIME_BIN`、CLIと同じバージョンの管理済みランタイム、PATHの順に選択します。`hibana runtime install --from FILE --sha256 HASH`でオフライン導入できます。`hibana runtime install`でGitHub Releaseの対応OS版を取得できます。基盤のソースを探す処理や暗黙のRustビルド・ダウンロードはありません。ビルド失敗時は起動済みの開発サーバーを維持します。`--no-watch`で監視を無効にできます。

ローカル専用の秘密値は`.dev.vars`にdotenv形式で書きます。値は`vars`より優先します。`.hibana/`と`.dev.vars`をバージョン管理に含めないでください。サーバーのSecretsは管理者が`hibana secret put NAME`の標準入力から登録し、`hibana secret allow-deploy NAME`でそのアプリへの利用を許可します。`hibana.json`に`"secrets": ["NAME"]`を指定し、通常の開発者が`deploy`します。省略時は空配列で、Secretを自動列挙・注入しません。`secret deny-deploy NAME`は今後の配備だけを拒否します。既存バージョンでの利用も止める場合は`secret delete NAME`を使います。

`hibana login --profile onprem --url https://api.example.internal --tenant team --email dev@example.internal --password-stdin`でログインします。`HIBANA_URL`・`HIBANA_TENANT`・`HIBANA_EMAIL`・`HIBANA_PASSWORD`でも設定できます。接続先と認証はPC共通のプロファイルに保存し、`--profile onprem`で選択できます。CIでは`HIBANA_TOKEN`を設定できます。配備先を変えた場合、保存済みの別サーバーのトークンは再利用しません。`deploy --version 1.0.0`で版を指定できます。省略時は一意な開発版を採番します。

実行用Workerが設定された配備先では、`deploy`はWorkerでの事前コンパイルを待ってから公開します。初回HTTPへのコンパイル待ちを避けるため、その時間はデプロイ所要時間に含まれます。準備に失敗すると配備はエラーになり、旧版の公開設定を維持します。`rollback`も切替先を準備してから公開します。

`hibana rollback`で直前の版へ、`hibana rollback --version 1.0.0`で指定した版へ戻します。コード・環境変数・選択したSecretsの参照を一緒に戻します。Secretsの値・外部データは巻き戻しません。Canaryや重み付き配分はありません。

## 実行モデルと制限

HTTPの契約は`wasi:http/incoming-handler@0.2.3`です。Hono・JS系はesbuildでバンドルし、ComponentizeJS/StarlingMonkeyでComponentに変換します。変換層は`src/javascript.mjs`に閉じており、Rust・Go・既存Componentの配備はJSエンジンを経由しません。`waitUntil`はWASI実行の完了と資源制限の対象です。

コンパイル済みコードはWorkerごとに再利用し、Store・Wasmインスタンスはリクエストごとに新しく作ります。追加・交換されたWorkerはバックグラウンドで準備され、その間の呼び出しは準備済みWorkerへ送ります。全候補が未準備の場合は準備完了まで503を返します。KubernetesのReadyだけで全アプリの準備完了を保証するものではありません。

本番Workerとdevは同じRust実行モジュールを使います。ただしdevは認証なしのloopback HTTPサーバーで、DB・配布・課金・分散処理はありません。外向き通信はdevでは拒否、本番では管理者の承認が必要です。JSのPOSTなどの入力は変換層でバッファします。レスポンスはストリーム可能ですが、WebSocket・完全なNode.js互換・Workers Bindingは提供しません。Preview 1単体や任意のWIT worldも未対応です。

バージョンの公開は単一DBトランザクションで行います。コード・vars・選択したSecretsの参照・active版・公開設定が揃って反映され、失敗した配備は稼働中の設定を変えません。初回のアプリ作成は別操作で、失敗時に未公開の空アプリが残る場合があります。詳細と旧環境の移行手順は[デプロイ仕様](../docs/deployment.md)を参照してください。

## オンプレでの役割分担

基盤管理者がKubernetes上のHibana、管理APIのHTTPS、アプリ用DNS/TLS、テナントを用意します。アプリ開発者は配布された管理APIのURLとテナントの認証情報を設定して、同じ`hibana`コマンドで配備します。アプリの配備にkubeconfigやクラスタ管理権限は不要です。通常のdeploy・rollbackに必要なのはRead・Deployスコープです。Secretsの保存・利用許可はAdminスコープとAdminロールを持つテナント管理者が行います。トークンはテナント単位で、アプリ単位の権限制限はありません。

Wranglerを参考にするのは、この短い開発・配備の流れです。サーバーの契約はWASI HTTPに統一し、Hono・JS/TS・Go・Rustのどれも共通の制限と認証を通します。ビルド工程と実行工程の信頼境界、本番前の検証項目は[セキュリティ境界](../docs/security.md)を参照してください。
