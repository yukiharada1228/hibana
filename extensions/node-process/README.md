# @hibana/node-process

npm process のブラウザ用 shim を提供します。`node:process`、グローバル `process`・`global` を提供します。nextTick などの JS 補助用途であり、OS プロセス管理・ホストの環境変数の公開は行いません。アプリの設定・秘密情報は Hono の env から取得してください。

```js
import process from "node:process";
process.nextTick(() => {});
```

`hibana.json` に `extensions: ["@hibana/node-process"]` を指定します。未公開のため、必要な依存 tarball も一緒にインストールしてください。[配布・インストール・ビルド手順](../README.md)。
