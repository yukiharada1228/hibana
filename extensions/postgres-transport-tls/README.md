# @hibana/postgres-transport-tls

PostgreSQL の SSLRequest による TLS 接続を提供する0.7.3の JS 部品です。TCP 部品と `@hibana/node-tls` を組み合わせます。認証方式や channel binding 用の証明書ハッシュは含みません。

`import { tcpTls } from '@hibana/postgres-transport-tls'` し、`createPostgres` の `transport: tcpTls` に渡します。Client／Pool の `ssl` や URL の `sslmode=require` で TLS を有効にします。`ssl:false` の TCP 接続にも対応します。TLS の証明書・ホスト名検証は必須です。[構成・導入手順](../postgres-core/README.md)。
