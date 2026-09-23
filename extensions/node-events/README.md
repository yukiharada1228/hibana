# @hibana/node-events

npm events の EventEmitter を提供します。`node:events` を解決します。Wasm 部品・通信権限は不要です。

```js
import { EventEmitter } from "node:events";
const events = new EventEmitter();
```

`hibana.json` に `"extensions": { "@hibana/node-events": "./vendor/hibana-node-events-0.5.0.tgz" }` を指定します。未公開の依存部品がある場合は、その tarball の取得元も `hibana.json` に指定します。アプリの `package.json` への拡張追加は不要です。[配布・インストール・ビルド手順](../README.md)。
