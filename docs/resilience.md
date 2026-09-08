# 隔離・復元・障害試験

HibanaのHTTP MVPを運用するための手順です。ここで提供する操作スクリプトは、チェックアウト専用のkindだけを対象にします。実オンプレ環境へ自動で適用するものではありません。[手元での実測結果](resilience-validation.md)を別に記録しています。

## コンパイルとキャッシュ

Fleet Workerは、SHA-256を照合したWasmを別プロセスのWasmtimeでコンパイルします。子プロセスはTokio・DB・トレーシングの初期化より前に制限を設定します。

| 制御 | 既定値・動作 |
| --- | --- |
| 同時準備数 | `WORKER_MAX_COMPILATIONS=1` |
| 子の仮想メモリ | Linux `RLIMIT_AS`、`WORKER_COMPILER_MEMORY_MIB=1024` |
| CPU時間・実時間 | `RLIMIT_CPU`と親のdeadline、`WORKER_COMPILER_TIMEOUT_SECS=60` |
| 入力・出力 | Wasm 32 MiB、コンパイル済み成果物128 MiB |
| 認証情報 | 環境変数を消去し、Rayonのスレッド数1だけを渡す。子の標準エラーは破棄 |
| 異常終了・キャンセル | 子をkillしてwaitし、その後にコンパイル枠を解放 |
| Linuxの追加設定 | core dump禁止、通常ファイルへの書込みサイズ0、FD上限64、no-new-privileges、dumpable無効。設定失敗時はコンパイルを拒否 |
| メモリキャッシュ | 最大64件、シリアライズされた成果物の合計256 MiBまで。古い参照を解放 |
| ディスクキャッシュ | 最大2 GiB・256ファイル。書込み前に古い自前の`.cwasm`を削除。原子的に置換 |

キャッシュのバイト上限はプロセスのRSS上限ではありません。実行中のComponent参照やホスト資源は別に必要です。2 GiBのゲスト予約、コンパイラ、コードキャッシュなどを合わせてWorkerの4 GiB cgroup内に収まるか実負荷で確認します。コンパイラの上限・同時数を増やす場合はPod全体の予算も見直します。

これはコンパイルの資源枯渇・クラッシュの影響を狭める変更です。同じUID・Pod内の子プロセスであり、OSレベルでネットワークやファイルシステムを完全に分離したコンパイルサービスではありません。ネイティブコンパイラの脆弱性による脱出まで無害化したとはしません。macOSの開発ではLinuxのrlimit/prctlは適用されません。`hibana dev`のローカルコンパイルもFleetの子プロセス経路とは異なります。

## 永続化と復元

ローカル基盤の新規インストールはPostgreSQL/MinIOにPVCを使います。既存のemptyDir環境はそのまま保持し、暗黙にディスクを切り替えません。PVC化した環境を再起動してもemptyDirへ戻しません。Redisはローカル開発用の単一インスタンスのままです。

既存環境の移行はメンテナンス時間に行います。HPAが存在する場合、スクリプトは何も停止する前に拒否します。先にHPAを外す等の運用調整が必要です。

対応するControl Plane・Workerイメージとmigration `0027_maintenance_gate.sql`を先に適用します。停止時はDB上の共通ゲートで管理API・アプリの新規受付を503にし、各CPで受付済みリクエスト（アップロードを含む）が終了し、DBのpending/runningが0になるまで待ちます。内部のSecrets引き換え・結果保存APIは維持し、その後Worker→CPの順に停止します。再開はCP→Worker→全active版のWasm準備→受付再開です。ゲートはCPの再起動でも維持され、ランタイムDBロールには変更権限を与えません。別のメンテナンス操作との同時実行も拒否します。

`backup`と`persist`の自動復帰では、Pod Readyだけで受付を再開しません。bootstrap管理者で認証する内部APIが、Kubernetesから取得した全Worker IPとDNSの結果を照合し、各Workerへactive版を準備します。最後にコンパイルをしないHEAD確認を行い、準備中のキャッシュ追い出しも検出します。操作側はPod UID・再起動回数・Ready状態を再確認します。準備APIは1回240秒、操作全体は300秒が上限で、不足・失敗・所有者変更時はゲートを解除しません。active版がキャッシュ容量に収まらない場合も、容量の見直しが必要です。ゲストのHTTPハンドラーはこの準備では実行しません。

drainは180秒で打ち切ります。未完了の実行が残ればバックアップを始めず、通常のレプリカ数へ戻します。復元・レプリカ再開・アプリ準備が失敗した場合は受付を閉じたままにします。SIGKILL等で操作プロセスが消えた場合もゲートは自動解除しません。手動復旧では、作業の終了・データ整合性・全Workerのアプリ準備を確認した運用者が、DB管理権限で`platform_maintenance.owner`をNULLへ戻します。

```bash
# アプリを停止して整合したバックアップを取り、別DBへのpg_restoreと行数を検証。
# 終了時に元のCP/Workerレプリカ数へ戻す。
python3 scripts/k8s_resilience.py backup --backup .local/backups/before-maintenance

# バックアップ・別DBでの復元検証 → PVC化 → S3/DBの復元と照合 → アプリ再開。
# 既にPVCがある環境には適用しない。
python3 scripts/k8s_resilience.py persist --backup .local/backups/storage-migration

# バックアップ自体の欠落・破損をオフライン検出。
python3 scripts/k8s_resilience.py verify --backup .local/backups/storage-migration
```

保存するものは、PostgreSQLのcustom-format dump、S3バケットのオブジェクト全件、DB認証情報・署名鍵・Secrets暗号鍵、SHA-256と行数のmanifestです。バックアップ先のディレクトリは0700、ファイルは0600です。**平文の鍵を含む**ため、このローカルバックアップを暗号化された別障害ドメインへ保管し、リポジトリへ追加しません。チェックサムは破損検出であり、悪意あるバックアップの真正性を保証しません。

`persist`で復元に失敗した場合はアプリを停止したまま残します。バックアップ内の`replicas.json`に元の台数を保存します。部分復元を上書きで再試行せず、元の鍵を設定した**空のDB・空のバケット**を別途準備し、`restore`で復旧します。

```bash
# 対象にはバックアップと同じSecretを事前に設定する。アプリのreplicasは0が条件。
python3 scripts/k8s_resilience.py restore --backup .local/backups/storage-migration
# 成功後、動作を確認してから元のレプリカ数へ戻す。
```

format 2のバックアップでは`key-config.json`に`SECRETS_MASTER_KID`と`JOB_SIGNING_KID`も保存し、チェックサムを付けます。復元先にはSecretの鍵素材に加えて、この2つの設定も一致させます。元クラスタのDB/S3接続先はこのファイルから復元しません。鍵IDのない既存format 1は破損検査だけ可能で、そのままの復元は拒否します。旧バックアップを移行する場合は、取得時点の鍵IDを運用記録から確認する必要があります。推測した`k1`で補完しません。

対象はローカル標準構成の`hibana-config`→`hibana-runtime`→`hibana-control-plane`という環境設定の順序です。別の設定元や、バックアップ対象Secretと異なる鍵素材への上書きは、復元情報の取りこぼしを避けるため拒否します。

復元時はS3へ書き戻したオブジェクトを再取得してSHA-256を確認し、DBの主要テーブル・Secrets履歴・版ごとのvars/Secret参照の行数を照合します。旧バックアップの6テーブルの行数一覧も読み取り可能ですが、新規取得では設定の2テーブルを加えます。さらに、生存するSecretの現行世代をすべて実際に復号します。検証にはCPと同じ暗号実装を持つイメージをローカルDockerで起動し、ネットワークを無効化、鍵・暗号文は標準入力だけで渡し、平文は出力しません。このイメージには`--verify-backup-secrets`モードが必要です。旧世代はダンプに保持しますが、計画的なrekey後に鍵を廃棄済みの履歴まで復号可能とはしません。

再開後にはCLIのlogin・既存API・Secrets・rollbackも確認します。Redisのレート制限/ログイン失敗カウンタはこのバックアップに含みません。Redisの再作成はロックアウト状態も失うため、公開環境ではHA・永続化・障害中の認証制御を別途設計します。

PVCはHAではありません。kindのlocal-pathボリュームは特定の論理ノードに依存し、kindクラスタ削除時には失われます。本番には複数ノードで利用できる耐障害ストレージ、DBのWAL保管/PITR、別環境への復元訓練が必要です。

## 混合負荷

```bash
# 既存の専用kind-hibana用。SDK認証情報は画面に表示しない。
set -a
source .local/kubernetes-hibana/sdk.env
set +a
HIBANA_SOAK_SECONDS=7200 node scripts/k8s-local-scale.mjs --soak
# 2時間が安定したら24時間へ伸ばす。
HIBANA_SOAK_SECONDS=86400 node scripts/k8s-local-scale.mjs --soak
```

Hono・TypeScript・JavaScript・Go・Rustをテスト専用テナントへデプロイし、本文一致を含むHTTP検証を行います。GET、64 KiBのecho、一部の低速受信を混ぜ、10分ごとに一つの成果物を別SHAで再デプロイしてcold startも発生させます。短縮試験では最初の再デプロイを試験時間の半分で行います。負荷並列数は`HIBANA_SOAK_CONCURRENCY`、既定4です。

結果は`.local/scale-test/soak-*.jsonl`へ逐次保存します。リクエスト失敗・p95、DB接続数、Worker再起動、ディスクキャッシュ量、Metrics ServerがあればCPU/メモリを記録します。レポートは一定区間の集計にし、負荷生成器が全リクエストをメモリへ蓄積し続けないようにします。正常系の合格条件は、本文一致・失敗0・試験終了後pending/runningが0です。途中停止は合格にしません。RSS/CPUの傾向、p95の許容値、DBの増分は別途運用SLOで判定します。

テストテナントは終了時に停止します。実行履歴・配備履歴・Wasmは調査のため残すので、長期試験に必要なDB/S3容量を確保します。DBの履歴が増えることとメモリリークは別です。負荷中の障害試験では失敗が期待されるため、正常系soakの合否とは分けて扱います。

soakの測定中は終了時刻・SIGINT・SIGTERMをHTTP呼出し、再デプロイ、kubectlの共通キャンセルへ伝えます。終了境界で中断した呼出しは`canceled`に記録し、完了したリクエストの成功に含めません。DBのdrainは別の60秒枠で確認します。kubectlは通常30秒、rollout/waitは指定した待機時間に余裕を加えた上限を持ち、タイムアウト後は子プロセス群を終了・回収します。障害・復元スクリプトも通常30秒、rollout/waitは260秒、dump/restoreは600秒です。復旧操作には中断済みの測定用キャンセルを引き継ぎません。

## 依存サービス・ノードの中断試験

小さいレスポンスを返す読み取り専用のHTTP関数を先に用意します。

```bash
python3 scripts/k8s_resilience.py fault --target redis --seconds 20 --probe-host hello.smoke.hibana.local
python3 scripts/k8s_resilience.py fault --target postgres --seconds 20 --probe-host hello.smoke.hibana.local
python3 scripts/k8s_resilience.py fault --target minio --seconds 20 --probe-host hello.smoke.hibana.local
python3 scripts/k8s_resilience.py fault --target hibana-worker --seconds 60 --probe-host hello.smoke.hibana.local
```

依存サービスはcontainerdのタスクをpause/resumeし、ノード試験はkind workerのDockerコンテナをpause/unpauseします。データを消すPod置換やクラスタ再作成を使いません。`finally`で再開し、Pod Readyと同じHTTP本文への復旧を確認します。SIGTERM/SIGINTでも復旧を試みますが、SIGKILLやホスト自体の停止は回復処理を実行できないので、その場合はpause対象を手動で再開します。

結果は`.local/kubernetes-hibana/fault-*.json`へ記録します。HTTPの観測は一つのCPへのport-forwardなので、経路を含めて一時失敗します。これは外部ロードバランサーの自動迂回を検証する試験ではありません。ウォームな関数はMinIOへアクセスしないため、MinIO試験後には新規deploy/cold startも別に検証します。

| 依存先 | Hibana側の対応 | 本番環境で必要な試験 |
| --- | --- | --- |
| PostgreSQL | SQLx再接続、SQL10秒/ロック3秒/idle transaction15秒、TCP keepalive/user timeout | 同じ書込み先名でprimary昇格、接続の再確立、RLS・署名・実行記録・Secrets・rollbackの確認 |
| Redis | 接続/応答2秒、再接続2回・待機最大500ms、到達不能時は受付/ログインを拒否 | HAのprimary切替、ロックアウトとカウンタの保持、復旧後のDBとの整合。Sentinel直接検出は未実装 |
| S3 | SDK接続3秒/1試行10秒/全体30秒・最大2試行。Worker取得は30秒 | 冗長ストレージの障害・復旧、未キャッシュWasm取得、アップロード、ハッシュ一致 |

SQLのstatement timeoutはDBサーバー側です。ネットワーク全体の応答時間を単独で保証しません。ゲストを開始した可能性があるHTTP呼出しは自動再実行せず、結果不明の書込みはアプリ側の冪等性で扱います。Worker喪失後の孤立実行は既存reaper（既定1800秒）で回収します。

Secrets取得は`hibana_worker_control_plane_request_duration_seconds`で応答本文の受信まで計測します。`operation="job_env"`、`outcome`は`ok`・`timeout`・`connect`・`authorization`・`rate_limited`・`server_error`・`malformed_response`・`transport`・`unexpected_status`です。タイムアウトは本文受信中も同じ分類で、URL・トークン・応答本文はメトリクスやエラーへ含めません。Worker転送失敗のログにはDNS探索とHTTP送信の段階、送信失敗の分類と所要時間を残します。これらは原因調査のための観測であり、タイムアウトの延長やゲストの再実行は追加していません。

Secret注入では、実行行から認可に必要なID・状態・受付時刻だけを取得し、HTTP本文の再読込みを避けます。生存Secretと実行時点の世代を同じSQLスナップショットで確認し、世代がない場合は拒否します。値のキャッシュは導入せず、削除・版の参照・テナント分離を引き続き確認します。

アップロードの受信・検証・S3保存の間はDBトランザクションを保持しません。保存後の短いトランザクションでアプリの生存状態と署名ポリシーを再確認します。成果物は`<tenant>/versions/<version_id>.wasm`へ保存し、同名アプリの再作成や重複アップロードで既存の成果物を上書きしません。DBへの公開に失敗した場合、参照されないS3オブジェクトが残ることがあります。自動削除はまだ実装していません。Workerの設定解決は受付済みexecutionのversion_idとSHA-256に束縛します。

この手元の試験ではprimary/replica構成を新設していません。実際のDB昇格、Redis HA endpointの更新、複数台S3の切替、物理マシンの電源断・ネットワーク分断は、オンプレの複数障害ドメインで別途合格させる必要があります。
