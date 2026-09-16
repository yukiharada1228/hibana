# @hibana/postgres-auth-md5

PostgreSQL の MD5 認証だけを提供する0.6.0の JS 部品です。`@hibana/md5` と Buffer に依存し、SCRAM・乱数・NFKC・通信を含みません。

`import { md5 } from '@hibana/postgres-auth-md5'` し、`createPostgres` の `authentication: { md5 }` に渡します。MD5 が必要な既存 DB の互換用です。[構成・導入手順](../postgres-core/README.md)。
