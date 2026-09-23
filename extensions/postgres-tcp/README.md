# @hibana/postgres-tcp

TLS を同梱しない PostgreSQL の TCP 専用アダプターです。共通の `postgres-core` に Pool・TCP・MD5・SCRAM を組み合わせたプリセットです。MD5／SCRAM 認証・型変換・クエリ・Pool・Drizzle に対応します。

最小構成を作る場合は、[postgres-core の個別選択](../postgres-core/README.md)で TCP と接続先に必要な認証部品を選び、Pool は必要な場合に追加してください。このプリセットでは Pool・MD5・SCRAM がすべて含まれます。

`hibana.json` でこの拡張を選ぶと `pg` をこの版へ解決します。`@hibana/postgres` と同時には有効化できません。`ssl` を有効にした設定や接続 URL は、接続開始前に `ERR_PG_TLS_UNAVAILABLE` で拒否します。TLS へ自動で切り替わることはありません。TLS が必要なら個別構成の通信部品を `postgres-transport-tls` に変更するか、`@hibana/postgres` プリセットを使用してください。

依存同梱版を `vendor/` に置き、次を `hibana.json` に設定します。アプリの `package.json` への追加は不要です。

```json
"extensions": {
  "@hibana/postgres-tcp": "./vendor/hibana-postgres-tcp-0.7.5-bundle.tgz"
}
```

`hibana build` が取得と `hibana-lock.json` の生成を行います。lock と tarball を保存し、CI では `hibana build --frozen-lockfile` を使います。[配布・導入・ビルド手順](../README.md)。
