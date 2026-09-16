# @hibana/postgres-tcp

TLS を同梱しない PostgreSQL の TCP 専用アダプターです。共通の `postgres-core` に Pool・TCP・MD5・SCRAM を組み合わせたプリセットです。MD5／SCRAM 認証・型変換・クエリ・Pool・Drizzle に対応します。

最小構成を作る場合は、[postgres-core の個別選択](../postgres-core/README.md)で TCP と接続先に必要な認証部品を選び、Pool は必要な場合に追加してください。このプリセットでは Pool・MD5・SCRAM がすべて含まれます。

`hibana.json` に `extensions: ["@hibana/postgres-tcp"]` を指定すると `pg` をこの版へ解決します。`@hibana/postgres` と同時には有効化できません。`ssl` を有効にした設定や接続 URL は、接続開始前に `ERR_PG_TLS_UNAVAILABLE` で拒否します。TLS へ自動で切り替わることはありません。TLS が必要なら個別構成の通信部品を `postgres-transport-tls` に変更するか、`@hibana/postgres` プリセットを使用してください。

0.7.4 の任意拡張です。`hibana.json` で `extensions: ["@hibana/postgres-tcp"]` を指定します。[配布・導入・ビルド手順](../README.md)。
