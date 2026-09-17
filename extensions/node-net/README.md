# @hibana/node-net

`net`・`node:net` の外向き TCP クライアントを提供します。本パッケージは JS のみで、`@hibana/tcp`・`@hibana/dns` の独立した Wasm 部品を Node の Duplex に接続します。TLS・暗号処理・Unicode 正規化を含みません。stream・buffer 拡張にも依存します。IP 宛ての TCP だけを Node API なしで使う場合は `@hibana/tcp` のみを選べます。

接続・読み書きは非同期で、1回の Wasm 呼び出しは最大16 KiB、同時ソケットは32個です。backpressure・タイムアウト・接続中断・keep-alive を扱います。サーバー、IPC、UDP、setNoDelay は未対応です。`@hibana/node-net/socket` は TLS などの上位トランスポート向け共通実装です。Node.js 全体との互換性は提供しません。

0.6.4では、`port` に数値または数値文字列を指定し、boolean・配列・オブジェクトなどは変換前に拒否します。接続時の `timeout`・keep-alive 待機時間も状態変更より先に検証します。引数エラーならソケットと接続コールバックは変更せず、引数を直して同じソケットで接続できます。

0.6.0から、Stream の依存を `@hibana/node-stream/duplex` に限定します。Socket だけのアプリには `Transform`・`PassThrough`・`pipeline`・`compose`・追加の演算子を入れません。これらを使うアプリは `@hibana/node-stream/passthrough` などの機能別入口を import してください。通常の API をまとめて使う場合は `node:stream` を import します。Socket の `.map()`・`.filter()` なども `import "node:stream"` で追加します。接続・バッファリング・半閉鎖・終了処理は従来と同じ Duplex 実装です。[Stream の選び方](../node-stream/README.md)。

0.5.1 では、`data` の購読や `resume()` がなくても、受信バッファが空のまま相手が切断すれば `end`・`close` を通知します。接続時にバッファリングを開始し、highWaterMark に達したら読み取りを止めます。受信済みデータは読み捨てず、利用者が読み取ってから終了します。`allowHalfOpen: true` では `end` の後も送信側を開いたままにするため、利用者が `end()` または `destroy()` で閉じます。この動作は共通 Socket を使う TLS／STARTTLS にも適用します。

```js
import net from "node:net";
const socket = net.connect({ host: "db.internal", port: 5432 });
```

`hibana.json` に `"extensions": { "@hibana/node-net": "./vendor/hibana-node-net-0.6.4.tgz" }` を指定します。未公開の依存部品がある場合は、その tarball の取得元も `hibana.json` に指定します。アプリの `package.json` への拡張追加は不要です。[配布・インストール・ビルド手順](../README.md)。
