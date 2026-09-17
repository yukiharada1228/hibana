# @hibana/dns

ホスト名のアドレス解決だけを行う Rust／Wasm 部品です。TCP ソケットを作れません。

`lookup(host)` でクエリを作り、`next()` を非同期タイマーでポーリングします。返り値は `{address, done}`、未準備では address が undefined、done が false です。done まで各候補を取得できます。最大32クエリ、ホスト名253バイト。終了時は `close()` と dispose を呼びます。通信権限は管理者が設定します。

0.5.0 の任意拡張です。`hibana.json` で `"extensions": { "@hibana/dns": "./vendor/hibana-dns-0.5.0.tgz" }` を指定します。[配布・導入・ビルド手順](../README.md)。
