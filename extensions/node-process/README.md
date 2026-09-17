# @hibana/node-process

npm process のブラウザ用 shim を提供します。`node:process`、グローバル `process`・`global` を提供します。nextTick などの JS 補助用途であり、OS プロセス管理・ホストの環境変数の公開は行いません。アプリの設定・秘密情報は Hono の env から取得してください。

```js
import process from "node:process";
process.nextTick(() => {});
```

`hibana.json` に `"extensions": { "@hibana/node-process": "./vendor/hibana-node-process-0.5.0.tgz" }` を指定します。未公開の依存部品がある場合は、その tarball の取得元も `hibana.json` に指定します。アプリの `package.json` への拡張追加は不要です。[配布・インストール・ビルド手順](../README.md)。
