# @hibana/sha224

`sha224` のダイジェスト計算だけを行う Rust／Wasm 部品です。`digest(Uint8Array)` が Uint8Array を返します。入力は最大1 MiB。他のハッシュ・乱数・鍵導出・通信・Node API を含みません。

0.5.0 の任意拡張です。`hibana.json` で `"extensions": { "@hibana/sha224": "./vendor/hibana-sha224-0.5.0.tgz" }` を指定します。[配布・導入・ビルド手順](../README.md)。
