# Hono + 拡張パッケージ

Hibana 本体を変更せず、`node:buffer` と SHA-256 用の `node:crypto` を任意のパッケージから追加する例です。`extension/`が配布者のソース、`src/`が利用者のHonoアプリです。JSとRust製部品を一つのアプリWasmへ合成します。サンプルのパッケージは公開していません。

リポジトリ内で配布者側のビルドを一度行い、ローカル依存として導入します。

```sh
# リポジトリのルートから
cargo install wac-cli --version 0.11.0 --locked --no-default-features --features wit
rustup target add wasm32-wasip2
npm ci --prefix sdk
npm run build --prefix sdk/examples/hono-extensions/extension
npm ci --prefix sdk/examples/hono-extensions
cd sdk/examples/hono-extensions
npm run dev
```

`http://localhost:8787/?text=abc` は SHA-256 と Base64 を返します。

```json
{
  "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  "base64": "YWJj"
}
```

利用者の設定は`"extensions": ["@hibana-example/node-compat"]`だけです。通常の`npm run build`・`npm run dev`・`npm run deploy -- --version 1.0.0`が宣言ファイルを読み、WIT生成・JSのバンドル・Component合成を行います。生成物は`.hibana/build/app.wasm`です。

パッケージとして渡す場合は、配布者が`sdk/examples/hono-extensions/extension`へ移動して`npm pack`を実行します。tarballには完成済みWasm・WIT・JSが含まれ、利用者は`npm install ./hibana-example-node-compat-0.1.0.tgz`で導入できます。利用者側にRustは不要です。Wasmの合成にはWACが必要です。

`extension/compat/crypto.mjs`はSHA-256、UTF-8文字列またはUint8Arrayの入力、hex出力だけを提供します。Node.js全体の互換実装ではありません。拡張を更新するときは配布者が新しい版をビルド・配布し、利用者が依存とlockfileを更新します。ローカルのRustソースを編集した場合は上記の拡張ビルドを再実行してから`dev`を起動し直してください。

`scripts/test-application-extensions.mjs`は実際のtarballを別ディレクトリへ導入し、通常のCLIビルド、配備用成果物、Wasmtime上のHTTP応答、未同梱部品の拒否を確認します。

仕組みと権限の境界は [アプリ拡張](../../../docs/application-extensions.md)を参照してください。
