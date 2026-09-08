# Hibana — X向けデモ動画

`hibana` で基盤構築 → Honoアプリのデプロイ → HTTP実行 → アプリ削除 → 基盤撤去までを実行した記録を、Remotionで90秒の動画に編集しています。

- 動画: `output/hibana-wasm-faas-x.mp4`
- サムネイル: `output/cover.png`
- 1920 × 1080、16:9、30 fps、H.264 / yuv420p
- 日本語字幕。無音（互換性のためAAC音声トラックを付加）

## 実行記録について

これは連続したデスクトップ録画ではなく、実際のコマンド出力を読みやすいターミナル表示・字幕・模式図に編集した動画です。待ち時間を短縮し、ログを抜粋しています。この旨は動画内にも常時表示しています。

2026-09-08に、事前コンパイル導入後の実装で再実行しました。`public/recording.json` に26コマンドの出力、終了コード、所要時間と検証結果を保存しています。認証情報は記録から除外しています。`src/video.tsx` は準備・実行・撤去まで確認済みの記録を読み込み、未完了の記録では書き出しを開始しません。

実際に確認したこと:

- 専用のkindクラスタ `hibana-demo` を `hibana platform install` で作成。
- `app.ts` のHonoアプリを `hibana deploy` でWasm Componentに変換・配備。
- 成果物の先頭8バイトは `00 61 73 6d 0d 00 01 00`。`wasi:http/incoming-handler@0.2.3` をexport。
- デプロイ完了後、2 Workerに当該成果物のコンパイル済みキャッシュが存在。その時点の実行履歴は0件で、ウォームアップ用にアプリを呼んでいません。Honoのビルドを含む配備は14.501秒でした。
- 3回のHTTPリクエストがすべて200、`{"message":"Hello, Wasm!","count":1}` を返却。
- HTTP全体の時間は218.709 ms／51.285 ms／73.795 ms。curlの`time_total`を記録し、動画は小数1桁で表示しています。
- 実行履歴3件のWasm SHA-256が手元の成果物と一致。各実行IDをWorkerログに照合し、実際の担当Podも動画に表示しています。前後で共通Worker Pod 2個および全DeploymentのUIDが同一。
- Worker実装は各リクエストに新しいWasmtime Storeとインスタンスを生成。動画の配置図はこの実行モデルを示す模式図で、各リクエストの特定Podへの割り当てを表してはいません。
- `hibana delete hello-hono --yes` 後はアプリ一覧が空になり、同じURLへのアクセスは404。
- `hibana platform uninstall --cluster hibana-demo --yes` 後はデモ用ノードが0個。

この動画は機能デモです。ビルド・事前コンパイルを含む配備所要時間とHTTP応答時間を分けて表示しています。3リクエストは一般的な性能値やSLOを示すベンチマークではありません。既存の`hibana-dev`クラスタは操作していません。

初版動画で使ったWasmについて、[改善前のキャッシュ検証](cache-verification.md)と[事前コンパイル導入後の検証](preparation-verification.md)も保存しています。初版の実行記録は`public/history/recording-before-preparation.json`に保持しています。

今回の再撮影はHonoソースからビルドし直しているため、WasmのSHA-256は初版と異なります。現在の動画は今回の記録だけを使い、以前の測定値を混ぜていません。今回のSHA-256は`90ef1c0c03e251ebcf4ece576761e2b76c3a37c81f8601c5b1d6710847e23384`です。

動画作成後に行ったHTTP経路の改善と、同じ動画用Wasm・同じリソース設定での比較は[応答時間の検証](latency-verification.md)に分けて保存しています。動画の数値は撮影時の実測値のままです。

その後、初回のメモリイメージ作成とSQLの準備も前倒ししました。[初回応答の追加検証](first-request-verification.md)では、Workerの交換後も含めて修正前後を比較しています。

## 動画を再生成する

Node.js 24以上を用意し、このディレクトリで実行します。

```sh
npm ci
npm run render
npm run still
```

macOSのChromeがあれば `remotion.config.ts` が使用します。それ以外の環境ではRemotionのブラウザ設定を使用します。日本語フォント（元の書き出しではHiragino Sans）も必要です。

編集プレビューは `npm run studio`、型確認は `npx tsc --noEmit`。動画の構成は `src/video.tsx`、サイズと尺は `src/index.tsx` で変更できます。`node scripts/preview.mjs` で各章の確認用静止画を作成できます。

## 実機操作を再記録する

[SDKのセットアップ](../sdk/README.md)と[ローカルKubernetesの前提条件](../deploy/kubernetes/README.md)を満たし、Dockerと `hibana` コマンドが使える状態で実行します。Python 3と `.local/bin/kind` も使用します。

```sh
npm run record
npm run render
```

記録スクリプトはデモ専用クラスタを実際に作成し、最後に削除します。既存の`hibana-demo`クラスタや`.local/demo-recording-current/hello-hono`がある場合は上書きせず停止します。成功時はWasmだけを`.local/demo-recording-current/app.wasm`に保存し、一時アプリ・依存パッケージ・ログイン情報を削除します。記録JSONと動画は新しい実行結果に置き換わります。

参考: [Remotionのrender CLI](https://www.remotion.dev/docs/cli/render)、[Xの動画ガイド](https://help.x.com/en/using-x/x-videos)。
