# イントラネットの Hibana コンソール

コンソールは Kubernetes 上の独立した Deployment で配信します。開発者の PC では CLI のビルド・アップロードとブラウザの画面操作を行い、配備済みアプリの実行・配信は Control Plane と Worker が担当します。PC の電源を切っても、基盤が稼働していれば配備済みアプリは動き続けます。

## 利用できる操作

- テナントのアカウントでログイン・ログアウト。
- アプリ一覧、名前での絞り込み、配信 URL の確認。
- バージョン一覧、確認ダイアログを経由した切り戻し。
- 現在のバージョンの環境変数を参照。変更は CLI で新しいバージョンを配備する。
- UTC 日付の期間を指定した利用量集計。
- 管理権限を持つアカウントによるアプリ削除。
- 接続中の基盤に合わせた CLI の接続・配備コマンドを表示。

アプリ一覧は画面の「更新」、ブラウザに戻った時、表示中の30秒間隔で再取得します。CLI と画面は同じ API とテナントの状態を使います。コンソールは基盤管理用の kubeconfig、DB・Redis・S3 の資格情報を保持しません。

## Kubernetes への導入

候補版 `0.2.0-rc.1` の CLI・Control Plane と同じソースから作ったコンソールを使用してください。以前の公開版には新しい `/auth/session`・`/auth/logout` API がありません。

`deploy/kubernetes/remote` および `hibana platform init` が生成するサイト設定にはコンソールを含めています。既存のサイト設定では `console/` 一式をコピーし、サイトの `kustomization.yaml` の `resources` に `console` を追加します。

1. コンソールのイメージを作成し、Kubernetes から取得できる社内レジストリに配布する。
2. `console/kustomization.yaml` のイメージを配布先の固定タグまたは digest に設定する。
3. `console/ingress.yaml` のホスト名、IngressClass、TLS Secret を設定する。例は `hibana.example.internal`。
4. 社内 DNS を Ingress のアドレスへ向け、ブラウザと CLI が信頼する証明書を用意する。
5. 管理 API と同様に Ingress controller の namespace へ `hibana.io/ingress=true` ラベルを設定し、サイトの overlay を導入する。

イメージの作成例:

```sh
docker build -t registry.example.internal/hibana/console:0.2.0-rc.1 console
docker push registry.example.internal/hibana/console:0.2.0-rc.1
```

アプリ配信 URL は既定で `https://<app>.<tenant>.<INGRESS_BASE_DOMAIN>/` です。標準以外のポートを使う場合、ビルド時の `--build-arg VITE_APP_ORIGIN=https://apps.example.internal:8443` で配信元を指定します。ホスト名は基盤の `INGRESS_BASE_DOMAIN` と一致させます。ローカル検証では `INGRESS_BASE_DOMAIN=localhost` と `--build-arg VITE_APP_ORIGIN=http://localhost:28084` を組み合わせると、コンソールから `http://<app>.<tenant>.localhost:28084/` を開けます。HTTP を許可するのはこの localhost 構成だけです。値は静的ファイルに埋め込まれるため、変更時はイメージを再ビルドします。

`hibana platform install --image ...` で指定するのは Control Plane・Worker のイメージです。コンソールのイメージはサイトの `console/kustomization.yaml` で別に管理します。リリースワークフローは Linux amd64 / arm64 のコンソールイメージを tar として出力し、既存のリリース用チェックサムに含めます。コンソールを除外したいサイトは `resources` の `console` を省略できます。

ブラウザと CLI の接続例:

```text
ブラウザ        https://hibana.example.internal/
CLI 管理 API    https://hibana.example.internal/api
アプリ          https://hello.team.apps.example.internal/
```

```sh
hibana login --profile intranet \
  --url https://hibana.example.internal/api \
  --tenant team --email developer@example.internal \
  --ingress-domain apps.example.internal
# Password: と表示されたらパスワードを入力して Enter（入力文字は非表示）
hibana deploy --profile intranet --version 1.0.0
```

この入力方式には最新の候補版 CLI が必要です。スクリプトや以前の CLI では、ログインコマンドの末尾に `--password-stdin < /secure/login-password.txt` を追加し、パスワードだけを保存したファイルを指定します。`--password-stdin` だけを付けても対話入力にはなりません。

既存の専用管理 URL（例 `https://api.example.internal`）も使えます。コンソールの `/api/` は Nginx が接頭辞を取り除いて `http://hibana-api:8080/` へ転送します。外側の Ingress では `/api` を書き換えず、そのままコンソール Service に渡してください。アプリのホストは既存のアプリ用 Service へ接続します。

外側の Ingress にも 33 MiB 以上のアップロード許可と125秒以上の応答待ちを設定してください。コンソール内のプロキシは33 MiB・125秒に設定しています。基盤のWasm上限を変更する場合は両方のプロキシの制限も合わせます。

コンソールの Pod は非 root・読み取り専用ファイルシステムで動作し、書き込みは容量制限付き `/tmp` だけです。コンソール固有の NetworkPolicy は管理 API の8080番への通信を許可し、名前解決には基盤共通の DNS 設定を使います。追加した Pod から Worker の内部 API を直接呼び出しません。

依存サービスへの共通 NetworkPolicy はコンソールを対象から除外しています。既存サイトへコンソールを追加する場合、独自のDB・Redis・S3向けegressルールにも`app.kubernetes.io/name NotIn [hibana-console]`を追加してください。NetworkPolicyの許可は合算されるため、サイト側の広いルールがコンソールにも適用されないようにします。

## 認証と画面の状態

CLI とブラウザはそれぞれ既存の `/auth/login` でトークンを発行します。ブラウザはトークンをページ内メモリにだけ保持し、localStorage・sessionStorage・Cookie には保存しません。ページを再読み込みした場合は再ログインします。有効期限は既存 API と同じ12時間です。

「ログアウト」は `/auth/logout` で現在のトークンを失効させます。別の PC・CLI のトークンは失効しません。タブを閉じるだけではサーバー側の失効は行わず、そのトークンは期限まで残ります。パスワード・トークンを URL や画面のコマンドに埋め込みません。

ブラウザからの API は同じオリジンへの Bearer 認証です。権限は画面と API の双方で確認し、Read のみのアカウントには切り戻し・環境変数・削除の操作を表示しません。認証失効時は画面上のテナントデータを破棄してログイン画面へ戻ります。イントラネットでも HTTPS が必要です。開発時の loopback HTTP のみ許可します。社内 CA はブラウザの信頼ストアと CLI の `NODE_EXTRA_CA_CERTS` に設定してください。

## 検証

```sh
npm ci --prefix console --ignore-scripts
npm run build --prefix console
npx --prefix console playwright install chromium
npm test --prefix console
python3 scripts/check-kubernetes.py
```

Playwright の通常テストはテスト用 API を使い、実 CLI からのアップロード、画面での切り戻しと CLI の読み戻し、権限別表示、エラー、失効、環境変数、削除確認、モバイル表示を検証します。テスト用 API は配布するイメージ・静的ファイルには含みません。

実 DB・実 Worker・配信用 Nginx の結合テスト:

```sh
docker build -t hibana-console:verification console
HIBANA_TEST_CONSOLE_IMAGE=hibana-console:verification bash scripts/test-http.sh
```

この試験は専用の一時 PostgreSQL・Redis・コンソールコンテナと空 DB を使用します。Control Plane と Wasmtime Worker はテストが起動し、S3 API は既存試験と同じテスト用オブジェクトサーバーです。CLI終了後のアプリ応答、ブラウザの切り戻し後の応答変更、CLIによる反映確認、ログアウト後の配信継続まで検証し、`.local/verification/console/` に画面と結果を保存します。物理的な別PC・実際のイントラネット DNS/TLS・Kubernetes への導入確認は、導入先で行います。
