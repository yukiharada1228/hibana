# Kubernetes deployment

HibanaはControl PlaneとWasmtime Workerを各2 Pod配置します。アプリケーションごとにPodを作成する方式ではありません。JSは開発者のCLIでWasmにビルドするため、クラスタ内コンパイラーは不要です。

[起動・停止・削除の実機検証結果](../../docs/lifecycle-validation.md)を記録しています。

## CLI で基盤を管理する

一般開発者は単体CLIからHTTPS管理APIを操作します。[CLI配布とオンプレ接続](../../docs/remote-cli.md)を参照してください。既存Kubernetesの基盤管理にはNode.js 24以上・kubectl・Python 3とPyYAMLを使います。以下のローカル基盤開発だけは追加でDocker・kind・基盤チェックアウトが必要です。

```bash
npm ci --prefix sdk
# hibana が未インストールの場合は node sdk/src/cli.mjs を使う
node sdk/src/cli.mjs platform install --source .
```

以降の`hibana`は`node /path/to/checkout/sdk/src/cli.mjs`でも実行できます。ローカル基盤は`hibana platform install --source .`だけでクラスタ作成、イメージのビルド、依存サービス、DBマイグレーション、起動確認、開発用テナントの作成まで完了します。更新時も同じコマンドです。

| コマンド | 動作 |
|---|---|
| `hibana platform install --source .` | 専用kind環境を作成・更新して起動 |
| `hibana platform stop --source .` | 全kindノードを停止。DB・ストレージ・資格情報を保持 |
| `hibana platform start --source .` | 保存した環境を再開してReadyを確認 |
| `hibana platform status --source .` | ノードの停止状態、Pod・Serviceを確認 |
| `hibana platform uninstall --source . --yes` | 専用kindクラスタとそのデータ・資格情報を削除 |
| `hibana platform test --source .` | サンプル配備・HTTP・Secrets・rollbackの検証 |

クラスタ名の既定値は`hibana`です。`--cluster NAME`で変更でき、状態は`.local/kubernetes-NAME/`に保存します。管理APIは`http://127.0.0.1:18080`、アプリHTTPは`http://127.0.0.1:18084`です。ホストポートは共通なので、この構成のクラスタは同時に1つだけ起動してください。専用kubeconfigを使い、普段のcontextは変更しません。

```bash
set -a
source .local/kubernetes-hibana/sdk.env
set +a
hibana login
hibana deploy -c sdk/examples/hono/hibana.json
hibana delete hello-hono --yes
hibana delete --all --all-tenants --dry-run
hibana delete --all --all-tenants --yes
```

`--all-tenants`は`BOOTSTRAP_ADMIN_TOKEN`を使い、停止中のテナントや負荷試験用のアプリも対象にします。アプリ削除は公開URLを無効化する論理削除です。実行履歴とWasm成果物を保持し、同じ名前で新しくデプロイできます。実行中のリクエストがある場合は409で拒否するため、トラフィックを止めてから再実行します。

Kustomizeがリソース定義、CLI内のPython処理が「依存サービス → マイグレーション完了 → CP/Worker」の実行順序を担当します。通常のPodにはマイグレーション資格情報を渡しません。イメージと環境設定が同じ再実行ではCP/Workerを強制再起動しません。接続にはNodePortとkindのポート公開を使い、常駐スクリプトやport-forwardは不要です。

### 既存のKubernetesへ配備する

外部PostgreSQL・Redis・S3、接続Secret、サイトの設定を含むKustomize overlayと配布済みイメージを用意します。CLIのインストール先に基盤のソースは不要です。`remote/`に管理APIとテナント別アプリのTLS Ingress例を用意しています。既存クラスタではDockerビルド・kindクラスタ作成を行いません。

```bash
hibana platform install --kubeconfig /path/config --context staging \
  --overlay /path/hibana-overlay --image registry.example.com/hibana/platform@sha256:...
hibana platform stop --kubeconfig /path/config --context staging
hibana platform start --kubeconfig /path/config --context staging
hibana platform uninstall --kubeconfig /path/config --context staging --yes
```

対象は`hibana` namespace内のHibanaリソースです。`install`が管理対象を記録し、`stop`はCP/Workerを0 Podにして元のreplica数と管理対象HPAを保存、`start`で復元します。外部管理のHPAがある場合は停止前に拒否します。GitOpsなど別のコントローラーと同時に同じDeploymentを管理しないでください。

既存Kubernetesの`uninstall`は記録されたリソースだけを撤去し、クラスタ・namespace・PVC・外部依存を残します。ローカルkindの`uninstall`はクラスタ内のデータも削除します。`--dry-run`は操作先の表示だけで、サーバー側の検証や差分計算は行いません。

### 旧hibana-dev環境

旧環境の状態は互換性のため`.local/kubernetes/`を使います。`hibana platform stop|start|status --source . --cluster hibana-dev`で操作できます。作成済みkindのポート公開設定は変更できないため、古い設定への`install`は既存データを変更せずエラーにします。新しい`hibana`クラスタを作るか、バックアップ後に明示的に旧クラスタを撤去してください。

## マニフェスト

| ディレクトリ | 用途 |
|---|---|
| `base` | CP/Worker、Service、PDB、NetworkPolicy、設定。依存サービスは別途用意 |
| `migration` | 所有者権限だけを渡す一回限りのマイグレーションJob |
| `remote` | 既存オンプレ向け管理API・アプリのTLS Ingress例。DNS/TLS/IngressClassはサイトで設定 |
| `local` | 専用kind向けイメージ・設定 |
| `local/dependencies` | 開発用PG/Redis/MinIO。単一Pod・一時ストレージ |
| `persistent-dependencies` | 開発依存のPVC化。HAではない |
| `hardened` | Worker用の管理者指定RuntimeClassとノードプール |
| `autoscaling` | 既存配備に追加するWorker HPA。Metrics Serverが必要。2〜8 Pod |

外部公開は管理API 8080とアプリHTTP 8083を別Ingressへ振り分けます。8081はWorker向け内部API、8084はWorker実行用内部HTTPです。NetworkPolicyのnamespaceラベルと、DNS/TLS/Ingressのホストルールをサイトごとに設定してください。

本番ではイメージを固定digestへ差し替え、Secret参照、依存サービス接続先、監視、TLS、HA、復元試験を用意します。CNIがNetworkPolicyを強制することを確認してください。[本番導入条件](../../docs/on-prem-production.md)に未完の運用要件を記載しています。

Workerはheadless Serviceで発見し、実行前の過負荷拒否時だけ別Podへ振り分けます。Podごとの線形メモリ予約は2 GiB、同時コンパイルは1件です。[スケール設定・負荷試験](../../docs/scaling.md)にHPAの導入方法、DB接続予算、制限の範囲を記載しています。

旧版からのアップグレードでは、停止した旧機能のWasmを再ビルドし、廃止したcompilerリソースをサイトの更新手順で削除します。通常の起動処理に旧版のリソース削除は含めません。

```bash
python3 scripts/check-kubernetes.py
python3 scripts/k8s-local-rollout.py
```

rolloutのスクリプトは専用kind内のPod更新を伴う試験です。本番クラスタでは実行しません。

更新中の配置分散には`matchLabelKeys: [pod-template-hash]`を使用し、旧版Podの数によって新版が同じノードへ偏ることを防ぎます。対象クラスタで当該機能を有効にしてください（[Kubernetes公式仕様](https://kubernetes.io/docs/concepts/scheduling-eviction/topology-spread-constraints/)）。

HTTP MVPへの更新前に旧非同期受付を止め、ジョブを完了させてください。新コードはNATSに接続しません。旧NATSリソースの整理はサイトの更新手順で行い、PVCとDBの履歴データの扱いを別途決めます。

新しい専用kind環境のPostgreSQL/MinIOはPVCを利用します。既存のemptyDir環境は自動で置き換えません。[データを保護したPVC移行・バックアップ・障害試験](../../docs/resilience.md)を参照してください。
