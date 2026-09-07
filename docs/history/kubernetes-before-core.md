> 旧版の検証履歴です。以下のWrangler/Bindingは現行実装から削除されています。

# Kubernetes対応の検証記録

2026-09-07。1台のDocker Desktop上でkind v0.33.0、Kubernetes v1.37.0（Linux arm64）を使用。
構成はcontrol-plane node×1、worker node×2。Hibana Control Plane×2、Worker×2を別々のworker nodeへ配置した。

## Rust/Wasm・直接HTTP・CLI統一の検証（2026-09-07）

サーバーはRust + Wasmtime + StarlingMonkey入りWasmのまま、独自`hibana` CLIを削除した。migration 0023とCP/Workerイメージを専用kindへ適用。以下は後述の旧CLI/配送経路の記録を更新する追加検証である。

| 検査 | 結果 |
|---|---|
| 公式Wrangler 4.129.0 | 初回/再配備、公開ルート表示、Secret put/list/delete、versions/deployments list、ID指定/省略rollback、deleteが成功 |
| 削除後の同名再配備 | 新しいcomponentとして配備が成功。削除前に保存したSecretが注入されないことを実応答で確認 |
| HTTP入口の分離 | アプリの`/healthz`と`/client/v4/accounts`が管理APIと衝突せず、アプリの応答を返す |
| SSE / waitUntil | SSE先頭チャンクを処理完了前に受信。800msの処理が残る間、正常EOFを待つことを確認 |
| NATS停止中の直接HTTP | NATSプロセスをSIGSTOPし、アプリ応答とwaitUntil、結果DB確定まで成功。finallyでSIGCONT |
| all-bindings回帰 | 削除したCLIに代わる固定native Wasm fixtureで9 passed, 0 failed |
| Workerローリング更新 | DO連続30件が各回+1で成功し、旧Worker全置換後のKV/R2/D1も成功 |
| 永続配送回帰 | 使い捨てPG/NATSで8シナリオ成功。直接HTTPの非キュー配送、署名付き引換、入力固定、結果の二重計上防止、完了後の再引換拒否、tenant境界を追加 |
| Rust通常テスト | 349件成功。共有Engine上の一方のStoreのtimeoutが、別Storeを期限前に中断しない実Wasmtime回帰を含む |
| 内部コンパイラーテスト | 2件成功。削除した独自CLI用テストは廃止し、公式Wranglerの実コマンド試験で配備を確認 |
| clippy / rustfmt / RLS lint / 配置契約 | 成功 |
| persistent-dependencies / hardened | renderと静的契約検査が成功。現在のkindへの適用、PVC移行、VM隔離の実証は行っていない |

再現:

```sh
HIBANA_TEST_NATS_OUTAGE=1 HIBANA_TEST_DELETE=1 python3 scripts/k8s-wrangler-smoke.py
```

NATS停止試験は専用kindで単独実行する。CIには`HIBANA_TEST_API_URL` / `HIBANA_TEST_APP_URL`とテスト用tenant認証を明示する公式Wrangler試験を追加した。このendpoint指定モードも、専用kindへのport-forwardを使って実行し成功した。外部endpointを指定した実行では、kindの障害注入を禁止する。リモートCIの実行結果はまだない。

公開HTTP本文は直接ストリーミングするため、executionレコードには本文を保存しない。途中失敗で既に送信したヘッダや外部副作用は取り消せず、処理全体のexactly-onceも保証しない。WebSocket・Node互換・Workers全機能・物理HAは未検証/未対応。[互換性の境界](wrangler-compatibility.md)と[配送保証](durable-delivery.md)を参照。

## 公式Wrangler APIの追加検証（2026-09-07）

0022 migrationとcompiler×2を追加し、公式npmパッケージ`wrangler@4.129.0`を改変せず使用した。接続先は専用kindの`/client/v4`、トークンはHibana発行。Cloudflareへのデプロイは行っていない。

| 検査 | 結果 |
|---|---|
| 初回`wrangler deploy` | 成功。Honoをサーバ側でコンパイルし、Kubernetes Workerの実行結果で文字列/JSON varsを確認 |
| `wrangler secret put` | 成功。Secretが実行時に注入されることを確認 |
| 同じWorkerへの再`deploy` | versions/deployments API経由で成功。応答内容が更新され、Secretも維持 |
| `wrangler versions list` | 成功 |
| `wrangler rollback VERSION_ID --yes` | 成功。旧コードに含まれるvarsの復元を実際の応答で確認 |
| 非対応`nodejs_compat` | コマンド失敗。active versionが変わらず、旧応答を維持 |
| 他tenantのaccount ID | 403 |
| read-only tokenで設定取得 | 403 |
| Rust通常テスト / SDKテスト | 346件 / 31件成功 |
| 既存の永続配送回帰 | 使い捨てDB/NATSで再実行し成功 |
| 共有ビルド処理の既存バインディング回帰 | legacy all-bindingsをkindで再配備し、9 passed, 0 failed |
| clippy / rustfmt / RLS lint / 配置契約 | 成功。compilerへのSecret/SA token未配布と、egress許可の混入がないことも静的検査 |

コンパイラー初回試験では、非root UIDのpasswd登録がないとWizerの既定設定を解決できない問題を検出した。イメージへ専用ユーザーを登録した後、同じ公式CLI試験が成功した。

再現は`python3 scripts/k8s-wrangler-smoke.py`。Node 24と`cd sdk && npm ci`、既存smoke tenantが必要。未対応API・runtimeの差分・公開URLの設定制約は[互換性契約](wrangler-compatibility.md)を参照。compilerのCNI強制・侵入試験、Wrangler全機能の確認は含まない。

## Outbox・永続結果配送の追加検証（2026-09-07）

同じ専用kindクラスタへ0021 migrationと新CP/Workerイメージを適用した。

| 検査 | 結果 |
|---|---|
| Rust CP / shared / worker | 通常344件成功。旧publish_backpressureのテスト1件を経路廃止に伴い削除。外部サービス依存の試験は別途実行 |
| 専用PostgreSQL / NATSでの配送回帰 | 1統合テスト内の下記7シナリオすべて成功 |
| Kubernetes障害試験 | NATSプロセス停止中に3件を202で受付、pending確認、CPを0 Podへ縮小して全旧Podの削除待ち、NATS再開・CP復元後に同じ3実行IDすべてsucceeded |
| Kubernetes上のall-bindings smoke再実行 | 9 passed, 0 failed。Queueに毎回異なる値を送信し、Alarm記録を予約時にリセットして、過去の成功を誤認しないよう修正 |
| 新配送経路でのWorkerローリング更新 | DO連続30件が各回+1で成功。全旧Worker Podの置換と、その後のKV/R2/D1の成功を確認 |
| clippy / rustfmt / RLS lint / Kustomize配置契約 | 成功 |

配送回帰の内容:

1. Outbox INSERT拒否時にexecution、Cron次回時刻、DO Alarm消込がすべてrollbackする。
2. NATSがpublishを拒否してもOutboxを保持する。冪等受付、未送信ジョブのsweep除外、テナント間RLS/FK分離を確認。
3. CP状態を再作成して同じジョブを配信。NATS保存ACK後・DB記録前の再試行で重複保存を抑止し、URLとトークンを送信時に発行する。
4. CP subscriber不在でも結果を保存。利用量のDB書込を拒否すると終端化もrollbackし、commit後ACK前の再処理で二重計上しない。
5. chainの一部の子を保存した後に親Outboxの消込を失敗させ、再試行しても同じ子を再作成しない。
6. 実際のdurable consumerでDB処理失敗→NAK→復旧後ACKを確認。Workerからの結果再publishは行わない。
7. failed通知もsubscriber不在から復旧して終端化する。

Kubernetes故障注入は専用スクリプトがNATSのSIGSTOP/SIGCONTとCPのscaleを実行した。依存Podを削除せず、終了時にCP×2・Worker×2へ戻した。これはプロセス再起動と一時的な配送不能の検証であり、NATSディスク喪失や物理HAの試験ではない。

## 初期配置時の結果（配送変更前、同日）

| 検査 | 結果 |
|---|---|
| Linux runtimeイメージのrelease build | 成功 |
| migration専用Job → 通常Pod起動 | 成功。通常Podはmigration資格情報を持たない |
| Kustomize base / local / migration / dependencies | renderと配置契約の検査成功 |
| baseのKubernetes server-side dry-run | 成功。既存のローカル依存Podに対するrestricted policy警告は想定内 |
| SDK CLI | 29 tests成功。未対応フラグ・設定の事前拒否、JSONC、切替順序、npm bin symlinkを含む |
| Rust CP / shared / worker | 通常テスト345件成功。既存の外部サービス依存chaosテスト22件とD1専用テストは通常実行ではignore |
| D1保存失敗の実DB回帰 | 別途実行して1件成功。snapshotファイル消失・PGの保存拒否を注入し、最後の保存データを保持 |
| clippy / rustfmt / RLS lint | 成功 |
| Kubernetes上のall-bindings smoke | 9 passed, 0 failed。HTTP、KV、R2、D1、DO、Queue送受信、Alarm |
| Workerローリング再起動 | DO連続30リクエスト成功、各回+1で継続。旧Podが全て置換されたことを確認 |
| Worker置換後のKV / R2 / D1 | 成功 |

ローリング試験では、スケール縮小のクールダウン90秒を未処理ジョブのNAK遅延にも使うと、HTTPの50秒締切を超える問題を検出した。`WORKER_DRAIN_NAK_DELAY_SECS`を分離し、配置定義では6秒に設定した後、同じ試験が成功した。既定値は従来のクールダウン設定を引き継ぐ。

## 再現

[配置手順](../deploy/kubernetes/README.md)に従って`k8s-local.sh up`と`forward`を起動した後:

```sh
bash scripts/k8s-local-smoke.sh
python3 scripts/k8s-local-rollout.py
python3 scripts/check-kubernetes.py
bash scripts/test-d1-persistence.sh
bash scripts/test-durable-delivery.sh
python3 scripts/k8s-local-delivery.py  # 他の試験と並行しない。終了後にforwardを張り直す
```

GitHub Actionsには配置定義・Linux image build・D1回帰・配送回帰を追加したが、リモートCIの実行結果はまだない。

## この検証に含まれないこと

- 複数物理サーバ、データセンター/電源障害。kindの3 nodeは同じDockerホスト上にある。
- PostgreSQL/NATS/Redis/S3のHA・バックアップ復元。ローカル依存Podは単一インスタンスかつ使い捨て。
- 本番CNIによるNetworkPolicy強制。NATSの認証接続は確認したが、社内CA/mTLS接続は未検証。
- Control Planeを入れ替えながらのHTTP連続処理、複数Worker同時喪失、負荷・長時間試験。
- 長時間の配送backlog・ストリーム満杯・物理ディスク喪失、外部副作用直後の停止でのアプリの冪等性。
- リリース全体の原子的切替・Workers完全互換。

本番合格の宣言には使用しない。[本番化計画](on-prem-production.md)の受入試験を別途満たす必要がある。
