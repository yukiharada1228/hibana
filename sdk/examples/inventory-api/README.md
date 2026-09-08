# 認証付き在庫照会API

Honoを使い、商品コードを受け取って外部の在庫サービスへ問い合わせる例です。Hibana専用アダプターは使いません。

`GET /health`は公開ヘルスチェックです。`GET /items/PEN-001`には`Authorization: Bearer ...`が必要です。認証後、設定した接続先の同じパスへ別の資格情報を付けて問い合わせ、`sku`・`name`・`available`だけを返します。

```json
{"sku":"PEN-001","name":"Hibana pen","available":42}
```

## 試用前の準備

リポジトリのルートで`npm ci --prefix sdk`、続いてこのディレクトリで`npm ci`を実行してください。Hibana CLIはローカルのSDKを参照します。`hibana.json`の`UPSTREAM_URL`を実際の在庫サービスのoriginに置き換えます。パス・クエリ・ユーザー名・パスワードを含めないでください。本番の資格情報を扱う接続先にはHTTPSを使います。

現在のHibanaでは、管理者が許可しても内部IP・loopback・メタデータIPへの通信は拒否されます。この例をそのまま社内のプライベートIPへ接続することはできません。接続要件が内部サービスだけの場合は、試用前に基盤の通信方針を検討してください。`hibana dev`も現在は外向き通信を拒否するため、ローカルでは認証・入力検証までを確認し、実通信は配備先で確認します。

## デプロイ

このディレクトリから`npx hibana`でCLIを実行します。管理者から受け取った管理APIのHTTPS URLを`HIBANA_URL`、開発者トークンを`HIBANA_TOKEN`に設定してください。最初は`hibana.json`を`"secrets": []`にして`npx hibana deploy --version bootstrap`を実行します。この状態の在庫照会は503となり、公開ヘルスチェックだけが動きます。

テナント管理者のトークンで2つのSecretを登録し、このアプリへの利用を許可します。値をコマンド引数やJSONへ書かず、標準入力で渡してください。

```sh
npx hibana secret put API_TOKEN < /secure/client-token.txt
npx hibana secret allow-deploy API_TOKEN
npx hibana secret put UPSTREAM_TOKEN < /secure/upstream-token.txt
npx hibana secret allow-deploy UPSTREAM_TOKEN
```

`secrets`を`["API_TOKEN", "UPSTREAM_TOKEN"]`に戻し、Read・Deployスコープの開発者トークンで`npx hibana deploy --version v1`を実行します。続いてテナント管理者が、その版の接続先を管理APIで承認します。`component_id`は`npx hibana list`で確認できます。

```http
PUT /components/<component_id>/versions/v1/capabilities/egress
Authorization: Bearer <テナント管理者トークン>
Content-Type: application/json

{"allow_outbound":["inventory.example.com:443"]}
```

承認前の在庫照会は502を返します。接続先の承認は版ごとなので、**更新時にも管理者の承認が必要**です。現在のCLIでは新しい版を公開してから承認する手順となるため、外向き通信が必要なアプリの無停止更新はこの手順では実現しません。試用では保守時間を決めて実施してください。承認済みの旧版への`npx hibana rollback --version v1`は、その版のコード・vars・Secret参照・通信許可を使用します。

## 確認すること

- Secret未設定は503、資格情報の欠落・不一致は401。
- 商品コードは英大文字・数字・ハイフンの32文字以内。無効な入力は400。
- 外部サービスの404は404、それ以外のエラー・リダイレクト・不正なJSONは502。
- 外部サービスの応答はアプリで16KiB以内を読み、必要な3項目だけ返す。HibanaのHTTPホスト層には別のバッファ上限があります。
- クライアントのヘッダーやクエリで接続先・外部サービスの資格情報を変更できない。
- Secretや外部サービスのエラー本文がHTTP応答・ログに出ない。

自動受入試験はリポジトリのルートで`python3 scripts/acceptance/kubernetes.py --seconds 7200`です。事前に`docker build --provenance=false -t hibana-platform:mvp-acceptance .`で候補イメージを作ります。新しい専用kind、PostgreSQL、Redis、MinIO、隔離した在庫サービスを使用し、試験後に専用クラスタを撤去します。一般の外部サービスへ負荷は送りません。結果とバックアップは`.local/mvp-acceptance/`へ保存され、鍵を含むため共有・コミットしません。
