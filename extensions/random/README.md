# @hibana/random

WASI の安全な乱数だけを提供します。`bytes(length)` が Uint8Array を返します。1回の上限は4096バイト。ハッシュ・通信・Node API を含みません。

0.5.0 の任意拡張です。`hibana.json` で `extensions: ["@hibana/random"]` を指定します。[配布・導入・ビルド手順](../README.md)。
