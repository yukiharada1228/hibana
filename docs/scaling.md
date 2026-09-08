# Hibana のスケールと過負荷制御

HibanaはWorker Podを増減してHTTP実行枠を増やします。アプリごとにDeploymentを作る方式ではありません。基盤管理者がKubernetesを運用し、アプリ開発者は従来どおり`hibana deploy`を使います。

2・4・8 Podの測定値とHPAの実機確認は[検証記録](scaling-validation.md)に記載しています。

## 受付と振り分け

1. Control PlaneがRedisのレート制限と、PostgreSQL上のテナント同時実行数を確認し、バージョンを固定した`pending`レコードを作成する。
2. Headless Service `hibana-worker-discovery`のDNSでreadyなWorkerを取得し、開始先を順番に変える。内部HTTPクライアントは接続を再利用する。Kubernetes APIへのアクセス権限は不要。
3. Workerは同時実行枠を確保し、署名トークンをControl Planeで交換する。DBから承認済み設定を取得し、メモリを予約した後、`pending → running`を条件付き更新する。この更新に成功したWorkerだけが実行する。
4. Workerが**実行前の容量不足**を内部ヘッダー付き503で返した場合だけ、別のWorkerへ送る。異なる接続先を最大8件試す。ゲストの同名ヘッダーはWorkerで削除するため、アプリの503では再送しない。
5. 転送に失敗した場合は`pending`のままの実行だけを`failed`にし、受付枠を返す。実行開始との競合はDBの条件付き更新で決着させる。未実行の拒否は利用量に加算しない。

通信切断・タイムアウトを理由にゲストを自動再実行しません。`running`の実行は副作用が発生した可能性があり、受付枠も直ちには返しません。Workerの結果保存、または既存reaperによる回収を待ちます。既定のstuck判定は1800秒であり、実行中にPodが失われた場合の即時復旧は未実装です。DB/Redis障害で枠の返却に失敗した場合もreaperの再同期が必要です。

HTTPのheadless接続ではPod IPへ直接送ります。HTTPSで`WORKER_HTTP_URL`を指定した場合は証明書検証用のホスト名を維持し、接続先のロードバランサーで分散します。内部TLS・mTLSやサービスメッシュの設定は導入先で用意してください。

## Workerの予算

| 設定 | 既定値 | 制限するもの |
|---|---:|---|
| `WORKER_MAX_CONCURRENCY` | 8 | 受付準備から結果保存までの同時リクエスト数 |
| `WORKER_GUEST_MEMORY_BUDGET_MIB` | 2048 | 承認済み線形メモリ上限の予約合計。実行ごとにMiB単位で切り上げる |
| `WORKER_MAX_COMPILATIONS` | 1 | ダウンロードからコンパイルまでの同時準備数 |
| `WORKER_DB_MAX_CONNECTIONS` | 8 | WorkerプロセスごとのDB接続数 |
| Kubernetes Worker memory limit | 4 GiB | cgroupによるプロセス全体の上限 |

例えば1 GiB上限の関数は1 Podに2件、256 MiB上限なら8件までです。実際に使用した量ではなく、許可した最大量を実行前に予約します。

この2 GiBはPodのRSS全体を制限するものではありません。コンパイル、コードキャッシュ、ホストのHTTPバッファ、WASIリソースなどに余白が必要です。コンパイルは認証情報を渡さない別プロセスで行い、Linuxでは仮想メモリ1 GiB・CPU時間60秒、親側でも実時間60秒を制限します。これはPodを別にした隔離ではありません。

Wasmダウンロードは32 MiB・30秒、コンパイル枠と同じ成果物の準備待ちはそれぞれ30秒で打ち切ります。呼出し側がキャンセルされたら子プロセスをkillしてwaitし、終了するまでコンパイル枠を保持します。同じ成果物の重複準備を固定数のロックで抑え、失敗したSHAごとのロックを無制限に保持しません。メモリLRUは64件かつシリアライズ量256 MiB、ディスクキャッシュは2 GiBを上限に古い成果物を退避します。RSS全体の制限ではありません。Control Planeのアップロード上限を32 MiBより上げてもWorkerの受入上限は増えません。

## 任意の自動スケール

Metrics Server等が提供する`metrics.k8s.io`と、ノードの空き容量を確認してから追加します。

```bash
kubectl --context YOUR_CONTEXT get --raw /apis/metrics.k8s.io/v1beta1/namespaces/hibana/pods
kubectl --context YOUR_CONTEXT apply -k deploy/kubernetes/autoscaling
kubectl --context YOUR_CONTEXT -n hibana get hpa hibana-worker
```

この追加マニフェストはWorkerのHPAだけを作成します。2〜8 Pod、CPU requestに対する平均使用率70%、増加は最大2 Pod/60秒、縮小は300秒の安定化後に最大1 Pod/60秒です。既定の500m requestでは、70%は1 Podあたり350mに相当します。これは初期設定であり、本番の最適値を保証しません。GitOpsでDeploymentを継続適用する場合は、HPA対象の`spec.replicas`を宣言から外し、手動scaleとの競合を避けます。[HPAの公式仕様](https://kubernetes.io/docs/concepts/workloads/autoscaling/horizontal-pod-autoscale/)

CPU指標だけでは外部I/O待ちやメモリ予約枯渇に応じた増設はできません。その必要がある運用では、Prometheus Adapter等を用意し、実行数・予約量を追加指標として調整します。MVPに独自のスケーラーやキューは追加していません。HPAによるノード自体の増設や、scale-to-zeroも対象外です。

DB接続数もPod数に比例します。CPは1 Pod最大10、Workerは既定8なので、CP 2 + Worker 8で最大84接続です。両Deploymentのsurge各1を含めると102接続になり、さらにマイグレーション・監視・管理用の枠が必要です。開発用PostgreSQLの既定100接続を本番の設計値にしないでください。最大Pod数とDB予算をセットで決めます。

## 監視する指標

- `wasmtime_inflight_executions`：実行・準備中のリクエスト。
- `hibana_worker_guest_memory_reserved_bytes` / `hibana_worker_guest_memory_budget_bytes`：予約量と上限。
- `hibana_worker_capacity_rejections_total{reason="concurrency|memory|draining"}`：実行前の拒否。
- `hibana_worker_active_compilations`、キャッシュhit/miss：cold startとコンパイル集中。
- CPの受付拒否、HTTP 429/503/502、p95/p99、DB pool待ち・接続数、Pod OOM/再起動、一時ストレージ消費。

readyは過負荷の指標にしません。全Podがbusyでもreadyな接続先を保持し、明示的な503を返します。readyのドレイン判定、Deploymentの段階更新、PDBの役割を混同しないでください。

## 検証の再現

以下はこのcheckoutが所有する`kind-hibana`だけを操作します。試験用テナントを作り、終了時に停止し、元のWorker数に戻します。アプリ負荷はCP両Podへの個別port-forwardに均等に送ります。

```bash
hibana platform install
set -a
source .local/kubernetes-hibana/sdk.env
set +a
node scripts/k8s-local-scale.mjs
node scripts/k8s-local-scale.mjs --capacity
# Metrics Serverを導入した専用kindでのみ実行
node scripts/k8s-local-scale.mjs --hpa
```

通常モードはcold/通常/過負荷/メモリ予約/復帰/ゲスト503を検証します。`--capacity`は200msの待機を含むRust HTTP関数を960回、同時数をPod数×6として測ります。CPU飽和のベンチマークではありません。`--hpa`は100msのCPU処理を使い、自動的に4 Pod以上がreadyになることを確認します。テスト用HPAは終了時に削除します。`.local/scale-test/`のJSONには認証情報を含めません。

これらは短時間・単一物理ホストの受入試験です。長時間負荷、Hono/JS/Goの大きい成果物のcold start、実CNI、ノード障害、DB/Redis/S3の切替と復元は[本番導入条件](on-prem-production.md)として別途評価します。

コンパイルの制限、混合soak・復元・障害試験は[隔離・復元・障害試験](resilience.md)を参照してください。
