# @hibana/node-buffer

Buffer のバイト列操作を npm buffer のブラウザ実装で提供します。`node:buffer` とグローバル `Buffer` を提供します。Wasm 部品・通信権限は不要です。

```js
import { Buffer } from "node:buffer";
const bytes = Buffer.from("hello", "utf8");
```

`hibana.json` に `extensions: ["@hibana/node-buffer"]` を指定します。未公開のため、必要な依存 tarball も一緒にインストールしてください。[配布・インストール・ビルド手順](../README.md)。
