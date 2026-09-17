# @hibana/tls

TLS セッションの状態・暗号化・復号・証明書検証だけを行う Rust／Wasm 部品です。TCP・DNS・Node API に依存しません。

`createClient({serverName, caPem, alpn})` の返すセッションへ、利用者が `receive()` で暗号化バイト列を渡し、`takeOutput()` で送信データを取得します。平文は `read()`・`write()` で受け渡します。`status().secure` が true になるまで証明書は取得できません。CA・ホスト名の検証は必須です。最大32セッション、受け渡し16 KiB、送信バッファ128 KiB。API 全体は配布物の `wit/world.wit` を参照してください。Node 形式の TLS 通信は `@hibana/node-tls` が担当します。

受信元が切断したら `receiveEof()` を呼びます。ハンドシェイク完了前の切断は `ECONNRESET` で失敗し、そのセッションでは処理を続行できません。成功・失敗にかかわらず `close()` で解放してください。

0.5.1 の任意拡張です。WIT のインターフェースは0.5.0を維持します。`hibana.json` で `"extensions": { "@hibana/tls": "./vendor/hibana-tls-0.5.1.tgz" }` を指定します。[配布・導入・ビルド手順](../README.md)。
