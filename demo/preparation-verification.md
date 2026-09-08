# 事前コンパイルによる初回HTTP応答の改善

2026-09-08、専用kindクラスタ`hibana-preparation-check`で動画と同一のHono Wasmを実行しました。**初回HTTP応答は改善前の9,776.775 msから34.024 msになりました。** コンパイルをデプロイ時に済ませる変更であり、コンパイル自体の速度改善ではありません。

## 実測結果

| 項目 | 改善前 | 改善後 |
|---|---:|---:|
| Worker Aへの初回HTTP | 9,776.775 ms | 34.024 ms |
| Worker Bへの初回HTTP | 9,965.924 ms | 27.096 ms |
| 初回HTTP時のキャッシュ | 両Workerともミス | 両WorkerともLRUヒット |
| 各HTTP応答のcount | 1 | 1 |

改善後の`hibana deploy --version 1.0.0`は11.946秒でした。この時間に2 Workerでの事前コンパイル、アップロード、公開処理を含みます。ビルド済みWasmを指定したため、HonoソースからWasmへの変換時間は含みません。

| 検証区間 | HTTP件数 | HTTP全体の最小〜最大 |
|---|---:|---:|
| デプロイ直後 | 8 | 8.779〜34.024 ms |
| 追加Workerの準備待ち〜完了の観測 | 34 | 8.337〜56.968 ms |
| 追加Worker準備完了後 | 12 | 9.204〜58.829 ms |
| 交換Workerの準備待ち〜完了の観測 | 32 | 8.339〜24.896 ms |
| 交換Worker準備完了後 | 12 | 6.859〜13.449 ms |

全98件がHTTP 200、`{"message":"Hello, Wasm!","count":1}`、DB上の実行状態は`succeeded`でした。追加Worker自身の初回HTTPは58.829 ms、交換Worker自身の初回HTTPは24.896 msでした。

計測値はcurlの`time_total`です。コマンド起動やメトリクス収集の時間は含みません。Worker内の`wall_time_ms`は別途元データに保存しています。これはローカル環境の機能検証であり、一般的なレイテンシやSLOを示す負荷ベンチマークではありません。

## 確認した動作

- デプロイ前は2 Workerともキャッシュカウンター0。デプロイ完了時には各Workerが1回コンパイル済みで、HTTPキャッシュヒット数と実行履歴件数は0でした。準備でアプリのhandlerを呼んでいません。
- 最初の8件は前後のPod別メトリクスを比較し、毎回1 WorkerのLRUヒットだけが増加することを確認しました。
- Workerを2→3へ増やし、続いて既存Pod 1個を削除して交換しました。新PodのReadyを確認してから、通常のHTTPを送りながらキャッシュ生成を観測しました。手動の`/prepare`呼び出しやウォームアップ用のアプリ実行は行っていません。
- 追加Podでは33回、交換Podでは31回、ネイティブキャッシュがまだ存在しない観測がありました。その間も既存Workerが応答しました。新Podはバックグラウンド準備後に実行を担当しました。
- 全98件の実行IDをPodログと照合し、担当WorkerとWasmのSHA-256を確認しました。各Workerのコンパイル回数は1回でした。
- `hibana delete hello-hono --yes`後にアプリ一覧が空であることを確認し、`hibana platform uninstall --cluster hibana-preparation-check --yes`で専用クラスタを撤去しました。Dockerのクラスタノード数は0です。一時アプリとログイン情報も削除しました。

## 実装と検証の範囲

準備に失敗したデプロイは503になり、旧版の公開設定を維持します。これは一時的なPostgreSQLとWorkerを使う`bash scripts/test-http.sh`でも確認しました。署名付き準備トークンと実行トークンの分離、準備済みComponentの保持、準備前の明示拒否に限った転送、通信エラーやゲスト503の非再送はRustテストで確認しています。`cargo clippy --locked -p hibana-worker -p hibana-control-plane -p hibana-shared --all-targets -- -D warnings`も通過しました。

KubernetesのReadyはプロセスの準備状態です。アプリごとの未準備判定はWorkerのHTTP受付で行います。全候補が未準備なら準備完了まで503となり、全Worker同時交換やキャッシュ容量超過での無停止を保証しません。今回の交換試験は新PodのReady確認後にHTTPを再開しており、Pod削除の瞬間の通信を含む無停止試験でもありません。詳細は[コード構成と事前コンパイル](../docs/architecture.md#公開前のコンパイル)にあります。

## 記録と再実行

- 改善後の生ログ・HTTP時間・Podメトリクス・実行ID・コードハッシュ: [public/preparation-verification.json](public/preparation-verification.json)
- 改善前の測定: [cache-verification.md](cache-verification.md)
- 現行の検証スクリプト: [scripts/verify-cache.py](scripts/verify-cache.py)

動画の元成果物`.local/demo-recording/hello-hono/.hibana/build/app.wasm`を保持したチェックアウトで実行します。専用クラスタはスクリプトの終了処理で削除されます。

```sh
hibana platform install --cluster hibana-preparation-check
python3 demo/scripts/verify-cache.py
```

比較に使用したWasmのSHA-256は両測定・動画で同一です。

```text
7b0f88cf64ef53e8c58e52ee3a9325929162a5f338ff1244f5073f50e9dd7ddd
```

比較対象の初版動画の実行記録は`public/history/recording-before-preparation.json`に保持しています。この検証後に動画を再撮影したため、現在の`public/recording.json`は別の実行記録です。この報告書と測定JSONは上記98件の記録を保持しています。
