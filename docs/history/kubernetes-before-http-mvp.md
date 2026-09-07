> HTTP専用MVPへ整理する前の検証履歴です。現行の機能範囲ではありません。

# Wasm core検証記録

2026-09-07、専用`kind-hibana-dev`クラスタで検証。Docker Desktop一台の上にKubernetes control-plane node一台・worker node二台を配置しています。物理サーバーのHA試験ではありません。

| 検査 | 結果 |
|---|---|
| Rust通常テスト | 330件成功。外部サービス依存試験20件は通常実行でignore |
| CLI単体テスト | 10件成功。設定検証、ビルド失敗時の成果物保持、引数の非シェル実行、JS依存なしの既存Componentビルド、配備順序/承認失敗、各テンプレートとinit上書き防止 |
| clippy・rustfmt・RLS lint | 成功 |
| Kubernetes配置契約 | base/local/migration/hardened/PVC overlayの静的検査成功 |
| Hono Componentビルド | 成功。生成プロジェクトの`npm run build`も成功 |
| `hibana dev` | 実WasmtimeでHono、ローカルSecrets、内部ヘッダーの偽装防止、バイナリPOST、SSE、ファイル変更後の再ビルド/再起動が成功 |
| kindへの反映 | CP/WorkerイメージとマイグレーションJob適用。compiler Deployment/Service/PDB/NetworkPolicyを削除 |
| 新CLIからのHono配備 | 初回/再配備、vars・Secrets・HTTP・バイナリPOST・SSEが成功 |
| 新CLIからのRust Component配備 | echoのWasmアップロードとNative invokeが成功 |
| JavaScript・Rust・GoのHTTPテンプレート | `init`から生成し、実Wasmtimeで環境変数・96 KiBバイナリPOST・HEAD・404が成功。既存Componentとしての再ビルドも成功 |
| Goのソース監視 | 変更後の応答切替、生成コードによる再ビルドループの防止、`go.mod`を保持することを確認 |
| 多言語HTTPのKubernetes配備 | 素のJavaScript・Rust・Goを同じCLI/APIから配備し、分散WorkerのHTTP応答と環境変数・バイナリPOST・HEAD・404が成功 |
| 削除API | Cron・trigger・Cloudflare APIの旧入口が404になることを確認 |
| 永続配送の障害試験 | 使い捨てPostgreSQL/NATSで7シナリオ成功。受理rollback、NATS停止/再送、再起動、結果保存障害、重複計上防止、署名付き直接HTTPを検証 |
| Workerローリング更新 | 30回の連続HTTP成功、旧Pod全置換後もvars/Secretsの応答と別ノードへの配置を確認 |

更新試験中に旧版Podの数によって新版のWorkerが同じノードへ偏る問題を確認し、CP/Workerの配置制約を`matchLabelKeys: [pod-template-hash]`で版ごとに分散する形へ修正しました。修正適用後の再試験で、旧Podの終了完了と最終的なノード分散も確認しました。

Honoは専用パッケージを削除し、`export default app`で同じ開発・配備試験を再実行しました。JSのFetch変換はCLI内に分離しています。Goは1.26.4とcomponentize-go v0.4.2、Rustは`wasm32-wasip2`、実行側は既存のWasmtime 29で検証しました。

再現コマンドは[配置手順](../deploy/kubernetes/README.md)を参照してください。スタンドアロンHonoの試験は`node scripts/test-dev.mjs`、素のJS・Rust・Goは`node scripts/test-components.mjs`です。後者に配備先の環境変数と`HIBANA_TEST_DEPLOY=1`を指定すると検証用テナントへの実配備まで実行します。CIにも登録していますが、リモートCIの実行結果はこの記録に含めません。

未検証: 物理ノード喪失、PostgreSQL/NATS/Redis/S3のHA・復元、本番CNIによる隔離、VMベースRuntimeClassの実適用、長時間負荷。`persistent-dependencies`と`hardened`はrender/静的検査までです。WASI Preview 1、任意WIT、WebSocket、Workers/Node.js完全互換は提供していません。

旧機能の検証履歴は`history/kubernetes-before-core.md`に分離しました。現行の対応機能を示すものではありません。
