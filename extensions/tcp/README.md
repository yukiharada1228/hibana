# @hibana/tcp

数値 IP アドレスへの TCP 接続だけを提供する Rust／Wasm 部品です。DNS・TLS・Node API を含みません。

`connect(address, port)` でソケットを作り、`status()`・`read()`・`write()` を非同期タイマーでポーリングします。各呼び出しは非ブロッキング、読み書きは最大16 KiB、ソケットは32個です。`read()` の未準備は undefined、EOF は空の Uint8Array です。終了時は `close()` とリソースの dispose を呼びます。ホスト名を使う場合は別途 `@hibana/dns` で解決してください。Node の net API が必要なら `@hibana/node-net` を使います。

0.5.0 の任意拡張です。`hibana.json` で `extensions: ["@hibana/tcp"]` を指定します。[配布・導入・ビルド手順](../README.md)。
