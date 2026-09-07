# HTTP MVP 検証記録

2026-09-07、専用 `kind-hibana-dev` クラスタとローカルで検証しました。
Docker Desktop 一台の上に Kubernetes control-plane node 一台・worker node 二台を配置しています。
物理サーバーのHA試験ではありません。機能の判断基準は [MVPの範囲](mvp.md) に記載しています。

## アーキテクチャ整理後の再検証（2026-09-07）

[モジュールの責務分担](architecture.md)に沿って起動・HTTP・実行サービス・DB・成果物・ランタイムを分離しました。SQL・マイグレーション・公開 API を維持し、HTTP のリクエスト型とストリーム経路を共通化しています。不正な base64 本文は、空の本文へ変換せず実行エラーにします。

| 検査 | 結果 |
|---|---|
| Rust通常テスト | 255件成功。HTTP符号化2件、ストリームの分割・完了待ち・保存失敗・切断を確認する3件を追加し、廃止した出力変換用の1件を除去 |
| PostgreSQL HTTP回帰 | 使い捨てDBの1件成功。受付失敗・署名・テナント分離・FORCE RLS・結果CAS・利用量・rollbackを確認 |
| 品質・境界 | Clippy全target・警告エラー化、rustfmt、RLS lint、Kubernetes構成、追加した依存方向の静的検査が成功 |
| CLI単体 | 10件成功 |
| Hono local dev | Secrets・ヘッダー偽装拒否・バイナリPOST・ストリーム・監視再ビルドが成功 |
| TS / JS / Rust / Go | 新規テンプレートのlocal dev、prebuilt build、CLI deployが成功。環境変数・96 KiBバイナリ・HEAD・404、Goの監視再ビルドも確認 |
| Linux / Kubernetes | Linuxイメージを専用kindへ更新し、マイグレーションJob完了。CP/Worker各2 Podが別worker nodeで稼働、再起動0回 |
| Hono Kubernetes smoke | CLI配備、Secrets登録・再配備、内部ヘッダー隔離、SSE、廃止APIの404、直前/指定版rollbackが成功 |

今回の依存変更は、既にlockfileに存在するHTTP型ライブラリをshared crateから直接使うことだけです。RustSec/npmの監査結果は以下のセキュリティ改善時の記録です。本番環境の隔離・可用性を検証済みとするものではありません。

## セキュリティ改善後の再検証（2026-09-07）

Wasmtimeを29.0.1からサポート中のLTS 36.0.14へ更新しました。不要な実行機能・旧TLSクライアントを除去し、S3 SDK等の監査で検出された依存も更新しています。実行の合計メモリ、テーブル要素、WASIハンドル数に上限を追加しました。[セキュリティ境界](security.md)に脅威・防御・残る受入条件を記録しています。

| 検査 | 結果 |
|---|---|
| Rust通常テスト | 251件成功。追加の4件は合計メモリ、失敗時の予約、合計テーブル要素、共有メモリ拒否の実Wasmtime検査 |
| PostgreSQL HTTP回帰 | 専用の使い捨てDBで1件成功。テナント分離・受付・内部認証・計量・rollbackを再確認 |
| 品質・構成 | Clippy全target・警告エラー化、rustfmt、RLS lint、Kubernetes全overlayの静的検査成功 |
| CLI単体 | 10件成功 |
| Hono local dev | 新ランタイムでSecrets、ヘッダー偽装拒否、バイナリ、SSE、設定変更後の再ビルド成功 |
| TS / JS / Rust / Go | 新規プロジェクトからlocal dev・prebuilt build・CLI deployを実行。通常の `.js` 入力も追加して検証。全経路でHTTP、環境変数、96 KiBバイナリ、HEAD、404成功 |
| Linux / Kubernetes | 新Dockerイメージをビルドし専用kindへ展開。CP/Worker各2 Podに更新完了。全言語の配備とHTTP応答が成功 |
| Hono Kubernetes smoke | Secretsの登録・再配備・応答、内部ヘッダーの隔離、SSE、廃止APIの404、直前/指定版へのrollback成功 |
| RustSec | 条件付きRSA例外を検証した上でゲート成功。lockfile全体には未使用RSAの1件が残る。spinのyanked警告2件も表示を維持 |
| npm | lockfile全体の監査で既知脆弱性0件 |

新しい `Security dependencies` workflowを追加しました。上記は同じ検査のローカル実行結果であり、GitHub上のworkflow実行済みという意味ではありません。今回も本番CNI・物理障害・VM RuntimeClassは未検証です。

## HTTP専用化時の検証（改善前のWasmtime 29）

| 検査 | 結果 |
|---|---|
| Rust通常テスト | 247件成功。DB統合テスト1件は通常実行ではignoreし、専用ハーネスで別途成功 |
| CLI単体テスト | 10件成功。ビルド失敗時の成果物保持、引数の非シェル実行、テンプレート、設定と配備順序を検証 |
| Clippy・rustfmt・RLS lint | 成功 |
| Kubernetesの構成検査 | base/local/migration/hardened/PVC overlayの静的検査成功 |
| HTTP受付のDB統合試験 | Redis不通時の503、DB書き込み失敗時の予約解放、過少カウンタでもDB上限を守ること、active versionのみの選択、outbox不使用を確認 |
| HTTP内部認証・計量 | 署名・版・テナントの照合、旧ジョブの交換拒否、FORCE RLS、結果のCAS、二重計上防止、計量値の上限、テナント停止を確認 |
| バージョン操作 | 同じ版への切替がpreviousを壊さないこと、通常rollback、別componentへのrollback拒否を確認 |
| ローカルHono dev | 実Wasmtime、Secrets、内部ヘッダー偽装防止、バイナリPOST、SSE、変更後の再ビルドと再起動が成功 |
| JavaScript・Rust・Go | initで生成し、実Wasmtimeで環境変数、96 KiBバイナリPOST、HEAD、404を確認。Goのソース変更、go.mod保持、生成コードによる監視ループ防止も確認 |
| Kubernetesへの実配備 | Hono・素のJS/TS・Rust・Goを同じCLIとHTTP APIから配備して成功 |
| CLIのrollback | 直前の版と指定した版への復帰、復帰後のHTTP応答、再切替後のSecretsを確認 |
| 廃止したAPI | `/invoke`、`/uploads`、traffic、promote、従来のCron/trigger/Cloudflare APIが404 |
| 常駐構成 | NATS Deployment/Serviceを撤去。CP/Worker各2 Podで起動し、別のworker nodeへ配置 |
| Workerローリング更新 | 30回の連続HTTP成功。旧Pod全置換後のvars/Secretsとノード分散も成功 |
| マイグレーション | 0001–0023の内容を変更せず、0024の追加と既存DBへの適用を確認 |

JavaScript系はComponentizeJS/StarlingMonkey、Goは1.26.4とcomponentize-go v0.4.2、Rustはwasm32-wasip2、実行はWasmtime 29を使用しました。workerdやHono専用アダプターは使用していません。

## 再現

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
npm test --prefix sdk
bash scripts/rls-lint.sh
bash scripts/test-http.sh
node scripts/test-dev.mjs
python3 scripts/check-kubernetes.py
python3 scripts/check-architecture.py
```

[専用kindの手順](../deploy/kubernetes/README.md)で配備先とテスト用テナントを用意し、API・アプリのポート転送を起動した後に実行します。

```bash
# HIBANA_URL / HIBANA_TENANT / HIBANA_EMAIL / HIBANA_PASSWORDを設定
# GATEWAYはアプリHTTP、HIBANA_INGRESS_DOMAINは公開ホストのベースドメイン
node scripts/smoke.mjs
HIBANA_TEST_DEPLOY=1 node scripts/test-components.mjs
python3 scripts/k8s-local-rollout.py
```

`test-http.sh`は使い捨てのPostgreSQLを作成・停止します。NATSは起動しません。
`k8s-local-rollout.py`は専用kindのWorkerを更新します。リモートCI自体の実行結果はこの記録に含めません。

## 残る課題

- deployは複数のAPI操作で、共有varsを含む原子的な切替は未実装。承認失敗時は新コードを有効化しませんが、varsは更新される場合があります。
- rollbackはコードの参照先だけを戻します。共有vars、Secrets、外部DBは戻しません。
- 物理ノード喪失、PostgreSQL/Redis/S3のHA・復元、本番CNIによる隔離、VMベースRuntimeClass、長時間負荷は未検証です。
- 旧版からの今回の更新では受付を停止し、実行中のHTTP・非同期処理を完了させてください。新旧の内部契約を混在させた無停止更新は保証しません。

通常のWeb APIに必要な機能へ整理できました。追加機能より、デプロイの原子性と運用検証を優先します。
過去のbytes handler・非同期配送の検証は [整理前の履歴](history/kubernetes-before-http-mvp.md) です。
