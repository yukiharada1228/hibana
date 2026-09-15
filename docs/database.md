# Hibana 基盤DB

Hibanaの配備管理・実行制御用DBです。ゲストアプリにSQLやORMを提供する機能ではありません。CP・WorkerのDBアクセスはRustのSeaORMを使い、Wasmtimeの実行モジュールはDBから独立しています。

## 構成の判断

| 保存先 | 責務 | 維持する理由 |
|---|---|---|
| PostgreSQL | テナント、認証、配備、実行、暗号化Secrets、使用量 | 複数CP/Workerの条件付き更新、トランザクション、RLSを一か所で保証する |
| Redis | 受付制限、一回限りのトークン、ログイン制限 | 全インスタンスで共有する短命な状態を管理する |
| S3 / MinIO | Wasm Component | 大きな成果物をメタデータDBから分離する |

SQLiteへの集約は、既存の複数CP/Worker構成に別の共有・調停機能を要求するため採用しません。PostgreSQLを追加のマイクロサービスへ分割する必要もありません。ローカルの`hibana dev`はこれらの基盤サービスを使わずに動作します。

## 初期スキーマ

| テーブル | 内容 |
|---|---|
| `tenants` | テナント、状態、クォータ、署名必須ポリシー |
| `users` | テナント内ユーザーと認証用ハッシュ |
| `api_tokens` | トークンハッシュ、スコープ、有効期限 |
| `audit_logs` | 追記専用の監査記録 |
| `components` | アプリ、公開設定、active・previous版の参照 |
| `component_versions` | 成果物、ハッシュ、実行制限、許可設定 |
| `executions` | 受付時に固定した版、実行状態、結果・利用量のメタデータ |
| `usage_rollups` | テナント・日付・アプリごとの利用量 |
| `function_secrets` | SecretのID、名前、現行世代、配備への利用許可 |
| `function_secret_versions` | 追記専用の暗号化された値と鍵の世代 |
| `component_signing_keys` | テナントのComponent署名検証鍵 |
| `version_configs` | 配備時に固定したvars |
| `version_secret_bindings` | 配備時に許可されたSecretのID参照 |
| `artifact_reservations` | アップロード中・失敗した成果物の回収記録 |
| `platform_maintenance` | 基盤全体の受付停止と操作owner |

この15テーブルに、SeaORM管理の`seaql_migrations`が加わります。業務データの初期投入は行わず、メンテナンス状態の1行だけ作成します。テナント・管理者は既存のbootstrap APIで作成します。

旧Storage、Durable Objects、Queue、Cron、非同期配送のテーブルは作りません。Canary、chain、非同期idempotencyの未使用列も除外しています。重複した索引を整理し、版参照・ユーザー参照・使用量の参照をテナント込みの複合外部キーで保護します。Secretやアプリの名前を再利用しても、過去の配備先IDは付け替わりません。

## ORMとマイグレーションの境界

- `crates/database/src/entities/`: 現在のテーブルを表すSeaORMモデル。
- `crates/database/src/queries.rs`: 版の同一性とSecretの世代選択に使う共通クエリ。
- `crates/database/src/postgres.rs`: 接続設定、トランザクション内のテナント設定、限定されたDB関数呼び出し。
- `crates/control-plane/src/db/`と`crates/worker/src/repository.rs`: ORMで読む・保存するリポジトリ。
- `migrations/src/`: バージョンを固定したスキーマ変更。モデルからの自動同期は使わない。

テーブル・CHECK・外部キー・索引はSeaORM Migration / SeaQueryで定義します。PostgreSQL固有のRLS、ロール・権限、SECURITY DEFINER関数は初期マイグレーションに同梱したSQLで維持します。SQLの実行箇所はこのDDLとテスト用fixtureに限定し、通常の読み書きはEntity・クエリビルダーを使用します。

初期版は`m20260915_000001_platform`です。以後は新しい番号のRustファイルを追加し、`Migrator::migrations()`へ登録してモデルも更新します。適用済みのマイグレーションと同梱ファイルは編集しません。SeaORMの履歴は適用済みバージョンの記録であり、ファイルのチェックサム照合ではないため、この規則はコードレビューでも確認します。

## 空DBでの検証

以下の自動試験は新しいPostgreSQL・Redisコンテナをランダムなloopbackポートで作成し、終了時に削除します。既存の`.env`・DB・Kubernetesには接続しません。DockerとRust、Node.jsが必要です。

```sh
bash scripts/test-http.sh
```

検証用DBを保持する場合は、別の空のPostgreSQLデータベースを用意して所有者接続を明示します。

```sh
MIGRATION_DATABASE_URL='postgres://OWNER:PASSWORD@HOST:PORT/hibana_verify' \
  cargo run --locked -p hibana-control-plane -- --migrate-only
```

所有者にはDDLとRLSを迂回するDB関数の管理権限が必要です。`faas_app`を事前作成していなければロール作成権限も必要です。ローカル検証用の既定パスワードは`faas_app`です。共有・本番環境では管理者が専用の認証情報でロールを用意してください。既存ロールのパスワードはマイグレーションでは変更しません。

検証CP・Workerだけに、新DBの非特権ロールの`DATABASE_URL`を渡します。自動マイグレーションを無効にして起動する場合も、初期版の適用を読み取り専用で検査します。ランタイムは所有者やSUPERUSER/BYPASSRLSで起動できません。

初期マイグレーションは空の`public`スキーマ専用です。旧`_sqlx_migrations`があるDBや別のテーブルがあるDBは、変更する前に拒否します。同時適用はPostgreSQLのトランザクションアドバイザリロックで直列化し、失敗時はスキーマと履歴をまとめてロールバックします。再実行しても適用済み版は実行しません。

## 既存DBの切替

今回の方針は**新しい空DBで検証し、既存の稼働DBは後で切り替える**です。旧SQLマイグレーション0001–0030はソースから削除しました。過去の履歴はGitに残っています。新しい初期版を既存DBへ適用したり、既存ボリュームを削除したりする操作は行いません。

後日切り替える際は、受付停止とdrain、既存DB・S3・暗号鍵のバックアップ、新DBへのテナント作成・アプリ再配備・Secretsの再設定、CP/Workerの接続先変更を一つの保守作業として扱います。既存データの自動コピーは実装していません。新DBは空から始めるので、過去の実行履歴は旧DBに残ります。新しいWasmの保存先は既存成果物と衝突しないよう分離し、復帰先の旧DB・旧バイナリ・鍵を保管してください。

## 今回の検証結果（2026-09-15）

- Rust workspace: 225件成功。外部DBを必要とする2件はHTTP試験で別途成功。
- 空DBへの適用、同時・再適用、旧DBと無関係な既存テーブルへの変更拒否を確認。
- 接続再利用時のテナント設定の消去、複合外部キー、RLS、追記専用権限を確認。
- 実CP・Workerで配備、HTTP実行、vars・Secrets、rollback、公開と削除の競合、再起動後の成果物回収を確認。S3はHTTPスタブ。
- Clippy全target、rustfmt、アーキテクチャ・RLS・Kubernetes定義の検査が成功。
- 依存監査は脆弱性指摘なし。既存の`spin`のyanked警告2件は引き続き表示。

これらは隔離したローカル環境の結果です。既存Kubernetesへのデプロイと稼働DBの切替は行っていません。
