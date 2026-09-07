# Kubernetes deployment

HibanaはControl PlaneとWasmtime Workerを各2 Pod配置します。アプリケーションごとにPodを作成する方式ではありません。JSは開発者のCLIでWasmにビルドするため、クラスタ内コンパイラーは不要です。

## 隔離したローカル環境

Docker、kind、kubectl、Python/PyYAMLを使用します。次のスクリプトは専用kubeconfigと`kind-hibana-dev`だけを使用し、現在のcontextを変更しません。

```bash
bash scripts/k8s-local.sh up
bash scripts/k8s-local.sh forward
```

別ターミナルで：

```bash
npm ci --prefix sdk
bash scripts/k8s-local-smoke.sh
```

APIは`127.0.0.1:18080`、アプリHTTPは`127.0.0.1:18084`です。`Host: hello-hono.smoke.hibana.local`でサンプルを呼び出します。開発用認証情報は`.local/kubernetes/sdk.env`に生成されます。既存の資格情報を自動ローテーションしません。

## マニフェスト

| ディレクトリ | 用途 |
|---|---|
| `base` | CP/Worker、Service、PDB、NetworkPolicy、設定。依存サービスは別途用意 |
| `migration` | 所有者権限だけを渡す一回限りのマイグレーションJob |
| `local` | 専用kind向けイメージ・設定 |
| `local/dependencies` | 開発用PG/Redis/MinIO。単一Pod・一時ストレージ |
| `persistent-dependencies` | 開発依存のPVC化。HAではない |
| `hardened` | Worker用の管理者指定RuntimeClassとノードプール |

外部公開は管理API 8080とアプリHTTP 8083を別Ingressへ振り分けます。8081はWorker向け内部API、8084はWorker実行用内部HTTPです。NetworkPolicyのnamespaceラベルと、DNS/TLS/Ingressのホストルールをサイトごとに設定してください。

本番ではイメージを固定digestへ差し替え、Secret参照、依存サービス接続先、監視、TLS、HA、復元試験を用意します。CNIがNetworkPolicyを強制することを確認してください。[本番導入条件](../../docs/on-prem-production.md)に未完の運用要件を記載しています。

旧版からのアップグレードでは、停止した旧機能のWasmを再ビルドし、廃止したcompilerリソースを削除します。`k8s-local.sh up`は専用開発クラスタ内の既知compilerリソースを削除します。DB/ストレージの旧データは消しません。

```bash
python3 scripts/check-kubernetes.py
python3 scripts/k8s-local-rollout.py
```

rolloutのスクリプトは専用kind内のPod更新を伴う試験です。本番クラスタでは実行しません。

更新中の配置分散には`matchLabelKeys: [pod-template-hash]`を使用し、旧版Podの数によって新版が同じノードへ偏ることを防ぎます。対象クラスタで当該機能を有効にしてください（[Kubernetes公式仕様](https://kubernetes.io/docs/concepts/scheduling-eviction/topology-spread-constraints/)）。

HTTP MVPへの更新前に旧非同期受付を止め、ジョブを完了させてください。新コードはNATSに接続しません。`k8s-local.sh up`は専用開発クラスタの旧NATS Deployment/Serviceを削除します。PVCとDBの履歴データは保持します。本番の旧リソース整理はサイトの更新手順で行います。
