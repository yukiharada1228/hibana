# @hibana/postgres-transport-tcp

PostgreSQL 向けの TCP を提供する JS 部品です。`@hibana/node-net` の Socket を利用し、pg の TCP_NODELAY ヒントを省略します。TLS・認証・証明書ハッシュを含みません。

`import { tcp } from '@hibana/postgres-transport-tcp'` し、`createPostgres` の `transport: tcp` に渡します。TLS を指定した接続設定は接続前に `ERR_PG_TLS_UNAVAILABLE` で拒否します。[構成・導入手順](../postgres-core/README.md)。
