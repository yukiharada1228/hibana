# CLI lifecycle validation

2026-09-08、Docker Desktop上の専用kindとネイティブ開発環境で実施しました。物理ホストのHA検証は含みません。

## 実際の整理結果

- `hibana delete --all --all-tenants --yes`で既存kindの30アプリを削除。負荷試験用6テナントの17アプリと、smokeテナントの13アプリを含みます。
- 旧ネイティブ基盤の`sample`も`hibana delete --all --yes`で削除しました。
- APIの全テナント一覧が`[]`、DBの未削除アプリ数が0、削除したアプリの公開URLが404であることを確認しました。
- 削除は論理削除です。旧kindはアプリ履歴32件・実行履歴25,221件を保持し、全ノードの停止・再開前後で件数が一致しました。
- 旧ComposeのPostgreSQL・Redis・MinIOデータを停止中にHibana名のボリュームへコピーし、ファイル一致を確認。新しいComposeで起動して未削除0件・アプリ履歴44件を確認した後、停止しました。重複する旧ボリュームは削除済みです。
- 旧名のネイティブCP/Worker、Composeコンテナ、廃止済みNATSコンテナを停止・整理しました。他プロジェクトのコンテナは変更していません。

## ライフサイクル検証

| 操作 | 確認した結果 |
|---|---|
| `platform install --cluster hibana-cli-check` | クラスタ作成、PVC依存サービス、マイグレーション、CP/Worker、認証まで完了 |
| `platform test` | CLI配備、Hono HTTP、バイナリPOST、Secret、SSE、rollbackが成功 |
| `platform stop` → `start` | 停止中はHTTPに接続できず、再開後に同じ応答・Secretを確認。資格情報ファイルのSHA-256が不変 |
| `delete -c ... --yes` | 検証用アプリの一覧が空になり、公開URLが404 |
| 既存Kubernetesの`install` → `stop` → `start` | 明示contextへ配備し、CP/Workerを0 Podにしてから元の2 Podずつへ復帰 |
| 既存Kubernetesの`uninstall` | 管理対象のCP/Worker等だけを撤去。PostgreSQL・Redis・MinIOと2 PVCを保持 |
| kindの`uninstall --yes` | 一時クラスタ3ノードと専用資格情報を削除 |
| `hibana dev`のCtrl+C | CLIが終了コード0で終了し、同じポートを直ちに別プロセスで再利用可能 |

再開時に保存済みReady状態だけで完了を報告する問題を修正しました。再開したノード上のコンテナの起動時刻、現在のreadiness、必要replica数を確認してから完了します。初回ビルド中の終了シグナルでファイル監視だけが残る問題も回帰テストで保護しています。

## 自動検証

CLIの名前指定・設定ファイル・dry-run・管理者資格情報・実行中保護、基盤の所有権と撤去範囲、旧Ready状態の拒否をテストしています。隔離PostgreSQLで管理APIの認証、テナント境界、実行中の409、削除後の同名アプリ作成、履歴保持を確認しました。Rustのworkspaceテスト、Kubernetes契約、RLS lint、アーキテクチャ検査も実行しています。

旧kindは停止したまま保持しています。再開は`hibana platform start --cluster hibana-dev`、状態確認は`hibana platform status --cluster hibana-dev`です。旧kindの公開ポート設定に制約があるため、新規環境は`hibana platform install`で作成します。詳細は[基盤管理ガイド](../deploy/kubernetes/README.md)を参照してください。
