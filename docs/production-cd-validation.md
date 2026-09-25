# rc.13 本番デリバリーの検証

2026-09-25、KAGOYAの2GB×3台へ`0.2.0-rc.13`を自動反映しました。配布物・本番のソースは`404d057b843a2936d1cb566c0cabf7a399e90939`です。実行手順と失敗時の対応は[本番CD](production-cd.md)を参照してください。

| 確認 | 結果 |
| --- | --- |
| リリース対象コミットのCI | [全ジョブ成功](https://github.com/yukiharada1228/hibana/actions/runs/36132623139)。Rust・HTTP・OIDC/SAML・ブラウザ・CLI新規導入/更新・Wasm拡張・JS/Rust/Go・rollbackを含む |
| 依存監査 / VPS IaC | [Security](https://github.com/yukiharada1228/hibana/actions/runs/36132623154) / [IaC](https://github.com/yukiharada1228/hibana/actions/runs/36132623218) 成功 |
| 配布 | [Release](https://github.com/yukiharada1228/hibana/releases/tag/v0.2.0-rc.13)に10配布物・SHA256SUMS・本番許可マーカー。4種類のPC用ランタイムと2 CPU向けのコンテナを検証 |
| npm | `@yukiharada1228/hibana@0.2.0-rc.13`を`next`へ公開。CIと管理者Macの両方で公開レジストリから取得し、CLIの版を確認 |
| 本番許可 | [release workflow](https://github.com/yukiharada1228/hibana/actions/runs/36132655076)の最終ジョブが12:27:24 UTCにマーカー発行 |
| 自動適用 | VPSのsystemd timerが12:30:54 UTCに起動。12:30:56に適用開始、12:32:22に検査完了。手動のdeploy実行ではない |
| バックアップ | Hibana/Keycloak DBと設定・SecretsをVaultで暗号化。DBの`pg_restore --list`、暗号化後の復号一致を検証。713,025 bytesの暗号化コピーを管理者Macにも保存・復号確認 |
| DB | `m20260925_000014_compiled_retention`適用済み |
| 稼働イメージ | Control Plane・Workerは`hibana-platform:0.2.0-rc.13-linux-amd64`、Consoleは`hibana-console:0.2.0-rc.13-linux-amd64`。各1 PodがAvailable、3ノードReady |
| 公開疎通 | Console・API readyz・OIDC discovery・`hello.demo.apps.hibana.cloud`がHTTP 200。helloの本文も一致 |
| 既存データ | helloの公開版`ver_580a6988471a4bb3975ba6551df41de1`を保持。対象WasmのhashがDBの保護関数に含まれることを確認 |
| 新Workerのキャッシュ | Garage復元1回、続くメモリhit 1回、再コンパイル0回。Workerのコンテナ再起動0回、未完了実行0件 |
| 追加リソース | 追加VPSなし。更新サービスの最大メモリ約185 MiB。SSH接続元の制限は維持 |

今回、配布用ビルドが通し試験より先に完了したため、最初の公開ジョブはCI待ちの段階で停止しました。CI成功後、同じタグ・同じ成果物で公開ジョブだけを再実行し成功しています。タグと配布物は上書きしていません。今後の公開用に`a49ca87`で最大30分のCI待ち合わせを追加し、待機・未実行・失敗・異なるSHAを扱う単体試験と、実際のGitHub CI待ち→成功の遷移を確認しました。この追加変更の[通常CIも全ジョブ成功](https://github.com/yukiharada1228/hibana/actions/runs/36134015492)しています。

新しいCD処理は、誤ったrepository/tag/commit/schema、古い・未承認・draftリリース、過去/別系統commit、バックアップ失敗、Ansible失敗、疎通失敗、未完了更新後の再実行停止をローカルの6テストで検証しています。

更新中の約2秒間隔のAPI readiness観測114回では、404が1回、3秒タイムアウトが1回ありました。これはアプリ全体の停止時間の測定ではありません。単一Worker構成の更新を無停止とは扱いません。また、上のキャッシュカウンター確認は速度ベンチマークではありません。応答速度の既存測定は[rc.12の検証記録](vps-production-cache-validation.md)を参照してください。

秘密値・DBの中身・生のインストールログは公開リポジトリに含めていません。バックアップはDB・設定用であり、Garageの災害復旧バックアップとは別です。
