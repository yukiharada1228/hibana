# @hibana/hmac-sha256

HMAC-SHA-256 だけを提供します。`sign(key, data)` に Uint8Array を渡します。鍵・入力はそれぞれ最大1 MiBです。内部の SHA-256 は計算に必要な実装であり、単独ハッシュ API や他のアルゴリズムは公開しません。

0.5.0 の任意拡張です。`hibana.json` で `"extensions": { "@hibana/hmac-sha256": "./vendor/hibana-hmac-sha256-0.5.0.tgz" }` を指定します。[配布・導入・ビルド手順](../README.md)。
