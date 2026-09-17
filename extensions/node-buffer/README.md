# @hibana/node-buffer

Buffer のバイト列操作を npm buffer のブラウザ実装で提供します。`node:buffer` とグローバル `Buffer` を提供します。Wasm 部品・通信権限は不要です。

```js
import { Buffer } from "node:buffer";
const bytes = Buffer.from("hello", "utf8");
```

`hibana.json` に `"extensions": { "@hibana/node-buffer": "./vendor/hibana-node-buffer-0.5.0.tgz" }` を指定します。未公開の依存部品がある場合は、その tarball の取得元も `hibana.json` に指定します。アプリの `package.json` への拡張追加は不要です。[配布・インストール・ビルド手順](../README.md)。
