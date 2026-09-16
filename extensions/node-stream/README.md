# @hibana/node-stream

Node 形式の Stream を提供する0.7.0の JS 拡張です。必要な API を機能別の入口から import できます。Buffer・events・process 拡張を利用し、Wasm 部品・通信権限は追加しません。

## 必要な機能だけを使う

```js
import { PassThrough } from "@hibana/node-stream/passthrough";
const stream = new PassThrough();
```

この構成に `pipeline`・`compose`・追加の演算子は入りません。`PassThrough` の基底クラスである Transform と、読み書き・終了処理の共通実装は含みます。

| import 先 | 名前付き export | 役割・必須依存 |
|---|---|---|
| `@hibana/node-stream/readable` | `Readable` | 読み取り。Writable・Duplex は含まない |
| `@hibana/node-stream/writable` | `Writable` | 書き込み。Readable・Duplex は含まない |
| `@hibana/node-stream/duplex` | `Duplex` | 双方向の読み書き。Socket はこの入口を使用 |
| `@hibana/node-stream/transform` | `Transform` | 入出力の変換。Duplex に依存し、PassThrough は含まない |
| `@hibana/node-stream/passthrough` | `PassThrough` | 入力をそのまま出力。Transform に依存 |
| `@hibana/node-stream/pipeline` | `pipeline` | 複数の処理の接続と終了管理。関数・非同期イテレーターにも対応するため PassThrough を使用。compose は含まない |
| `@hibana/node-stream/compose` | `compose` | 処理列を Duplex にまとめる。pipeline に依存 |
| `@hibana/node-stream/finished` | `finished` | 完了・エラーの監視。Readable／Writable／Duplex の実装は含まない |

各入口には同じ関数・クラスの default export もあります。`pipeline` と `finished` は `node:stream` と同じ callback 形式です。`finished` が返す関数で監視用リスナーを解除できます。

0.6.1から、アプリの共通ファイルで複数の機能を再 export しても、使わない機能別入口はビルド時に除去されます。

```js
// streams.mjs
export { Readable } from "@hibana/node-stream/readable";
export { compose } from "@hibana/node-stream/compose";

// app.mjs
import { Readable } from "./streams.mjs";
```

この例では `compose`・`pipeline`・`PassThrough`・`Transform` は入りません。通常の `node:stream` を同じ共通ファイルから読み込むと、その初期化とまとめ API は残ります。

0.7.0では Readable／Writable から Duplex への型判定用の参照を切り離しました。単方向の Stream だけを使う場合、反対側の Stream と Duplex は読み込みません。Duplex を読み込むと内部の型参照にそのコンストラクターを設定し、上流と同じ `instanceof` 判定を使います。Duplex の読み書き別の objectMode・highWaterMark と、サブクラスの動作を維持します。

全入口は同じ生成済みモジュールとコンストラクターを使うため、別の入口で作った Stream も `pipe`・`pipeline`・`compose` で接続できます。バッファ・イベント・終了処理など、各機能に必要な共通処理は引き続き含みます。

`hibana.json` に `extensions: ["@hibana/node-stream"]` を指定します。Node TCP／TLS・PostgreSQL の依存として既に有効な場合は、追加の設定は不要です。未公開のため、必要な依存 tarball も一緒にインストールしてください。[配布・インストール・ビルド手順](../README.md)。

## 通常の Stream API をまとめて使う

従来の入口も維持します。

```js
import { PassThrough, pipeline } from "node:stream";
```

`node:stream`・`stream`・`@hibana/node-stream` は通常の API をまとめて読み込みます。機能を限定する場合は、上記の機能別入口を使ってください。`.map()`・`.filter()` など Readable の追加演算子を使う場合も、通常の入口を import します。`extensions` に書いただけでは、これらのコードは読み込みません。

## 実装と検証

`readable-stream` 4.7.0 を固定して利用します。作者用の `build.mjs` がブラウザ API から到達する上流モジュールだけを `dist/` に生成し、Readable・Writable・Duplex の3ファイルの型参照を移植します。想定した版やソースが変わるとビルドを停止します。上流の `node_modules` は変更しません。

配布物には生成済みの共有実装と上流ライセンスを含め、Node 専用入口や別コピーの `readable-stream` パッケージは含めません。利用者のインストール時にパッチやビルドフックを実行する必要はありません。作者は `npm run build --prefix extensions --workspace @hibana/node-stream` で生成でき、通常の `npm pack` でも生成します。

バイト列変換の初期化は `src/bytes.mjs` で共有します。`package.json` の `sideEffects` では、この初期化と通常の入口 `src/index.mjs` を保持し、未使用の機能別入口だけを除去可能にしています。パッケージ全体を `sideEffects: false` にはしません。

`scripts/test-extension-boundaries.mjs` で、配布 tarball から各入口を個別にバンドルし、必須依存と不要な機能の非同梱を検査します。共通ファイルから再 export する場合も8つの入口を検査し、出力に残るモジュールを確認します。Readable／Writable 単独の Wasm 実行、Duplex の後からの読み込みと継承・型判定・読み書き別の設定、通常の入口との併用、backpressure、エラー時の解放も検証します。バンドルへの非同梱と、最終 Wasm のサイズ削減は別の検証です。
