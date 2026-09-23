# @hibana/pbkdf2-sha256

PBKDF2-HMAC-SHA-256 による32バイトの鍵導出だけを提供します。`derive(password, salt, iterations)` に Uint8Array と反復回数を渡します。入力は各1 MiB、反復は1〜100000回。HMAC／SHA-256 はこの計算に必要な内部実装です。

0.5.0 の任意拡張です。`hibana.json` で `"extensions": { "@hibana/pbkdf2-sha256": "./vendor/hibana-pbkdf2-sha256-0.5.0.tgz" }` を指定します。[配布・導入・ビルド手順](../README.md)。
