# オンプレ本番導入条件

Kubernetesの上にHibanaを置く構成は、Workerの複数配置・ローリング更新・運用監視に適しています。KubernetesのPod管理と、HibanaのComponent/テナント/呼び出し管理を分担します。現在のマニフェストだけで物理障害へのHAが完成するわけではありません。

## 準備するもの

- 複数の物理ノード/障害ドメイン、CNIによるNetworkPolicy強制、Ingress、DNSとTLS。管理APIとアプリ公開口を分離する。
- PostgreSQLの永続化・レプリケーション・フェイルオーバー・バックアップ/PITR。マイグレーション専用ロールとRLS適用ランタイムロールを分離する。
- Redisは現在の接続方式に合うHAエンドポイントを用意する（Sentinel自動検出を実装済みとはしない）。
- Wasm保存先のS3互換ストレージの冗長化・バックアップ。署名鍵・Secrets暗号鍵・DB認証情報の保管、ローテーションと復元手順。
- Worker用ノードプール、非root・read-only rootfs・seccomp。信頼できないテナントを実行する場合は、検証済みVMベースRuntimeClassなどの追加隔離を導入する。
- Prometheus/ログ/OTelの収集、SLO・アラート・容量上限、障害復旧・キー喪失・ノード停止・ネットワーク分断の訓練。

`deploy/kubernetes/base`はCP/Worker各2 Pod、PDB、配置分散、リソース制限、段階更新、ネットワーク分離の土台です。`hardened`は管理者が用意する`hibana-sandbox` RuntimeClassを要求します。`persistent-dependencies`は開発依存サービスをPVC化するだけでHA構成ではありません。

## リリース前の確認

[セキュリティ境界と受入条件](security.md)も確認してください。Worker Pod単位のVM隔離は、同じWorker内のテナントごとのVM隔離ではありません。第三者テナントを相互に敵対する主体として収容する場合は、資格情報も含めて隔離単位を再設計・検証します。

本番の実ネットワークで、Pod再配置だけでなくノード喪失・DBフェイルオーバー・Redis切替・S3障害を試験してください。バックアップから別環境へ復元し、Wasm本体・メタデータ・Secretsを一緒に復元できることを確認します。イメージのdigest固定、脆弱性検査、更新とrollbackの手順も必要です。

HTTPはCP/DB/RedisとWorkerに依存します。関数の自動再実行は行いません。通信エラー時に関数の副作用が完了している場合があるため、重要な書き込みにはアプリケーション側の冪等性が必要です。MVPの範囲と復旧の境界は[HTTP MVP](mvp.md)を参照してください。
