# @hibana/postgres-auth-scram

SCRAM-SHA-256／PLUS を提供する0.6.0の JS 部品です。pg 8.23.0 のプロトコル・サーバー証明検証を再利用し、乱数・SHA-256・HMAC・PBKDF2・NFKC を独立した Wasm 部品へ委譲します。MD5・TCP・TLS を含みません。

`scramSha256({ certificateDigests: { 'SHA-256': digest } })` の結果を `createPostgres` の `authentication.scram` に渡します。証明書ハッシュはこの部品から自動で追加せず、利用者が必要な関数を import して渡します。[構成・導入・不足時の動作](../postgres-core/README.md)。
