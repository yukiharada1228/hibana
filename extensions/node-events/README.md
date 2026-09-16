# @hibana/node-events

npm events の EventEmitter を提供します。`node:events` を解決します。Wasm 部品・通信権限は不要です。

```js
import { EventEmitter } from "node:events";
const events = new EventEmitter();
```

`hibana.json` に `extensions: ["@hibana/node-events"]` を指定します。未公開のため、必要な依存 tarball も一緒にインストールしてください。[配布・インストール・ビルド手順](../README.md)。
