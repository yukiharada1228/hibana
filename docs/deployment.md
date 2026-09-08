# デプロイ・環境変数・Secrets

HibanaのHTTP MVPは、Wasmコード・vars・使用するSecretの参照をバージョンごとに固定します。通常の開発者はRead・Deployスコープで`hibana deploy`と`hibana rollback`を実行できます。Kubernetes権限もテナントのAdmin権限も必要ありません。

## アプリの設定

```json
{
  "name": "my-api",
  "main": "src/index.ts",
  "vars": { "GREETING": "Hello Hibana" },
  "secrets": ["API_KEY"]
}
```

`vars`は平文です。`secrets`は使用する名前だけの配列で、省略時は空配列です。同じ名前を両方には指定できません。名前は英大文字・数字・アンダースコア、先頭は英大文字かアンダースコアで、64文字以内です。両者の合計は64件以内、varsの値は1件4KiB、キーと値の合計32KiB以内です。

初回は`secrets: []`でアプリを配備します。管理者がそのアプリにSecretを用意してから、設定に使用する名前を追加してください。

```sh
# テナント管理者の認証情報で実行
hibana secret put API_KEY < /path/to/value.txt
hibana secret allow-deploy API_KEY

# hibana.jsonのsecretsへAPI_KEYを追加し、開発者の認証情報で実行
hibana deploy
```

Secretを保存しただけでは利用許可になりません。AdminスコープとAdminロールで`allow-deploy`したものだけを、以後のDeploy権限による配備で選択できます。許可の単位はテナント内のアプリとSecretです。トークンそのものはテナント単位であり、同じテナントのDeploy権限保持者はそのアプリのコードを更新できます。

`hibana secret deny-deploy API_KEY`は今後の配備への許可を止めます。既存バージョンとその版へのrollbackは許可されたままです。`hibana secret delete API_KEY`は既存の版も含めて以後の注入を停止します。既に実行中のアプリが取得した値は回収できません。削除後に同名Secretを作り直しても、古い版の参照は新しいSecretへ付け替わりません。

## 公開とrollback

CLIは`POST /components/{id}/versions`のmultipartにWasm、`vars`、`secrets`、`activate=true`、`ingress=true`を一緒に送ります。サーバーは受信・隔離検証・S3保存を終えた後、短いDBトランザクションで次を実行します。

1. テナントとアプリの生存状態、署名ポリシーを再確認。
2. バージョンを作成し、選択したSecretの利用許可を確認。
3. varsとSecretのID参照を保存。
4. active版と公開設定を更新してcommit。

途中で失敗すれば、稼働中のコード・vars・Secret参照・公開設定は変更されません。受信とS3操作の間はDBトランザクションを保持しません。新規アプリの作成は別操作なので、初回配備に失敗すると未公開の空アプリが残る場合があります。S3保存後の失敗で未参照の成果物が残る場合もあります。S3とDBをまたぐ分散トランザクションではありません。

HTTP受付時にexecutionへバージョンIDを固定し、Workerはその版のvarsを読みます。受付後に別の版が公開されても、コードとvarsの組み合わせは変わりません。`rollback`はコード・vars・Secret参照を戻します。Secretの値は実行受付時点で有効な世代を使用するため、コードを戻してもSecretのローテーションや外部データは巻き戻りません。外向き通信の許可は従来どおり管理者による版ごとの承認です。

## 旧環境からの更新

`0028_version_environment.sql`は、旧`function_configs`の現在値を既存の全バージョンへコピーします。旧スキーマにはvarsの履歴がないため、過去の配備時点の値は復元できません。旧版で承認済みだったSecret参照は引き継ぎますが、新しい配備への利用許可は自動で付けません。

この更新はCP・Workerの混在稼働に対応しません。基盤管理者が以下を保守時間に実施します。

1. 新規受付を停止し、処理中の管理API操作とHTTP実行をdrainする。
2. WorkerとCPを停止し、DB・S3・鍵をバックアップする。
3. 所有者のDB接続で新しいCPの`--migrate-only`を実行する。
4. 新しいCP・Worker・CLIを揃えて起動し、既存HTTP、vars、Secrets、rollbackを確認して受付を再開する。

旧`PUT /config`・`DELETE /config/{key}`・版ごとのenv承認PUTは409を返します。varsとSecret参照の変更には新しいバージョンを配備してください。旧CLIも更新が必要です。移行前の構成へ戻す場合は旧バイナリだけを起動せず、取得したバックアップからの復元を含めて対応します。

## 検証

`bash scripts/test-http.sh`は使い捨てPostgreSQL・RedisとローカルCP/Workerを使用します。スキーマ移行、権限不足、同時配備、公開失敗、HTTP応答のvars/Secrets、rollback、Secret削除・再作成、RLS・複合FKを確認します。S3はこのテストではHTTPスタブです。実Kubernetesの更新・実S3の切替・長時間負荷は別の受入試験です。
