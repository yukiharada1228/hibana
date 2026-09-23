# @hibana/node-tls

`tls`・`node:tls` の外向き TLS クライアント・STARTTLS を提供します。net・buffer・TLS エンジン拡張に依存する JS アダプターです。

0.6.4では、STARTTLS に渡す元の TCP ソケットの `allowHalfOpen` を引き継ぎます。Node.js と同じく、TLS 側に異なる値を指定しても元の値が優先されます。元のソケットが `true` なら、相手の送信終了後も利用者が `end()` または `destroy()` するまで送信側を維持します。新しく TCP 接続を作る場合は TLS オプションの値を使います。ポートの型検証と引数エラー後の再試行は Node TCP 0.6.4 と共通です。

0.6.0では Node TCP 0.6.0 と Stream 0.5.1 の Duplex 専用入口を使い、任意のストリーム補助 API を自動で同梱しません。必要な場合は `@hibana/node-stream/transform` などの機能別入口、または通常の `node:stream` を import します。[Stream の選び方](../node-stream/README.md)。

別パッケージ `@hibana/tls` の Rust／Wasm の rustls が TLS セッションを所有し、JS が TCP 部品と暗号化バイト列を受け渡します。このパッケージ自体は Wasm を含みません。TLS の計算だけを使う場合は、TCP／Node API に依存しない `@hibana/tls` を選びます。公開 CA または明示した CA とホスト名の検証は必須です。検証無効化・クライアント証明書・サーバー・再ネゴシエーションは未対応です。

ALPN、検証済みの DER サーバー証明書、ソケット所有権を移す STARTTLS に対応します。同時 TLS セッション32個、各方向の受け渡し16 KiB、rustls 送信バッファ128 KiB。Node.js TLS の限定実装です。

ハンドシェイク完了前に相手が切断すると、`ECONNRESET` の `error` と `close` を通知し、TLS セッションと TCP ソケットを解放します。STARTTLS で渡した元のソケットも閉じます。

0.5.3 は `@hibana/node-net` 0.5.1 の終了処理を利用します。受信データがない接続では、`data` の購読や `resume()` を呼ばずに正常な TLS 終了通知を受けても `end`・`close` が届きます。受信済みデータがある場合は、バッファを維持して利用者の読み取りを待ちます。

STARTTLS の引数は、元の TCP ソケットを引き継ぐ前に検証します。`timeout` は有限の非負数を指定し、不正な値は `ERR_OUT_OF_RANGE` になります。引数エラーの場合は元のソケットを使い続けるか、正しい値で TLS 接続をやり直せます。`tls.connect({ socket, ... })` と `new tls.TLSSocket(socket, options)` の両方で同じ扱いです。

```js
import tls from "node:tls";
const socket = tls.connect({ host: "db.internal", port: 5432, servername: "db.internal" });
```

`hibana.json` に `"extensions": { "@hibana/node-tls": "./vendor/hibana-node-tls-0.6.4.tgz" }` を指定します。未公開の依存部品がある場合は、その tarball の取得元も `hibana.json` に指定します。アプリの `package.json` への拡張追加は不要です。[配布・インストール・ビルド手順](../README.md)。
