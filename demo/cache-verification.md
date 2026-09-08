# Workerごとのキャッシュと応答時間の確認

2026-09-08、事前コンパイル導入前のチェックアウトをビルドした専用kindクラスタ `hibana-cache-check` で検証しました。動画と同じWasm成果物を使用しています。これは改善前の記録です。改善後は[事前コンパイルの検証](preparation-verification.md)を参照してください。

**同じWorkerへの2回目以降の呼び出しでコンパイル済みコードのメモリキャッシュが使われ、処理時間が短くなることを確認しました。** 新しいWasmインスタンスを作る方針でも、コンパイル結果は再利用されます。

## 実測結果

| 呼び出し | 担当Pod | キャッシュ | Worker内の記録時間 | HTTP全体の時間 | count |
|---|---|---|---:|---:|---:|
| 1 | A | ミス・成果物取得とコンパイル | 9,707 ms | 9,776.775 ms | 1 |
| 2 | A | メモリのLRUにヒット | 5 ms | 21.540 ms | 1 |
| 3 | B | ミス・成果物取得とコンパイル | 9,945 ms | 9,965.924 ms | 1 |
| 4 | B | メモリのLRUにヒット | 3 ms | 21.378 ms | 1 |
| 5 | A | メモリのLRUにヒット | 4 ms | 17.874 ms | 1 |
| 6 | B | メモリのLRUにヒット | 2 ms | 9.863 ms | 1 |
| 7 | A | メモリのLRUにヒット | 1 ms | 8.303 ms | 1 |
| 8 | B | メモリのLRUにヒット | 1 ms | 8.793 ms | 1 |

- A: `hibana-worker-6ddbcdddf-57rjc`
- B: `hibana-worker-6ddbcdddf-86wqj`
- 全8件でHTTP 200、実行履歴は `succeeded`、応答は `{"message":"Hello, Wasm!","count":1}`。
- 最後のPod別カウンターは、いずれも `miss=1, lru=3, cwasm=0`。計測前はすべて0。
- Worker内の時間はDBの `wall_time_ms`。実行状態更新・成果物準備・実行を含み、通信全体やインスタンス生成単独の時間ではありません。
- HTTP全体の時間はcurlの `time_total`。CLIプロセスの起動時間や、前後のメトリクス取得時間は含みません。

## 確認方法と証拠

1. 専用クラスタで、動画とSHA-256が一致するWasmを `hibana deploy` で配備。
2. Podごとの `/metrics` を、loopback限定の `kubectl port-forward` で取得。
3. 各HTTPリクエストの前後で `wasmtime_component_cache_misses_total` と `wasmtime_component_cache_hits_total{tier="lru"|"cwasm"}` を比較。各回で変化するPodとカウンターが1個だけであることを検証。
4. DBの実行IDと各Podの実行ログを照合し、メトリクスで特定したPodと一致することを全8件で確認。
5. 前後のPod UIDが同一であることを確認。
6. `hibana delete hello-hono --yes` 後にアプリ一覧が空であることを確認し、`hibana platform uninstall --cluster hibana-cache-check --yes` で撤去。専用クラスタのノードが0個であることも確認。

元データは [public/cache-verification.json](public/cache-verification.json) に保存しています。JSONにはメトリクスの全出力、Podログ、実行ID、使用イメージのID、主要ソースファイルのハッシュも保存しています。[scripts/verify-cache.py](scripts/verify-cache.py)は改善後の動作を検証するよう更新され、この改善前JSONは上書きしません。

使用したWasmのSHA-256:

```text
7b0f88cf64ef53e8c58e52ee3a9325929162a5f338ff1244f5073f50e9dd7ddd
```

## この計測時のコードから確認できること

- [artifacts.rs](../crates/worker/src/artifacts.rs): キャッシュの探索順はプロセス内LRU → ローカルのコンパイル済みファイル → 取得とコンパイル。キーはWasmのSHA-256です。
- [runtime/mod.rs](../crates/worker/src/runtime/mod.rs): リクエストごとにWASI context、Store、Wasmインスタンスを生成します。コンパイル済みComponentは引数として受け取り、再利用します。
- [dispatch.rs](../crates/control-plane/src/dispatch.rs): Control Planeの各プロセスが独立したカウンターで転送先を回します。Control Planeも複数Podのため、外から見た順番が常にA→B→Aになるわけではありません。
- [worker.yaml](../deploy/kubernetes/base/worker.yaml): ディスクキャッシュもPodの `emptyDir` です。Podを作り直すと失われます。

今回は「1回目が遅く、2回目が速く、3回目が再び遅い」という順番でした。速くなる条件は呼び出し回数ではなく、**担当Workerに当該成果物のキャッシュがあるか**です。

## この検証で確定していないこと

動画の元の3リクエストには担当Podとキャッシュ差分の記録がないため、その時点のA→B→Aという割り当てを後から確定することはできません。今回の結果は、同じ成果物を使った改善前実装の追加検証です。

成果物のダウンロード、コンパイル、インスタンス生成それぞれの単独時間は計測していません。また、8リクエストだけの機能確認であり、代表的な性能値や分位点を示すベンチマークではありません。実行済みインスタンスの再利用方式との比較も含みません。

最初の計測試行ではKubernetes APIのPod proxy経由でのメトリクス取得がタイムアウトしました。その環境を撤去し、取得経路をport-forwardに変更して、上記の8リクエストを新しいクラスタで計測しています。
