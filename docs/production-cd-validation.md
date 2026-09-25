# rc.13 本番デリバリーの検証

2026-09-25、KAGOYAの2GB×3台へ`0.2.0-rc.13`を自動反映しました。配布物・本番のソースは`404d057b843a2936d1cb566c0cabf7a399e90939`です。実行手順と失敗時の対応は[本番CD](production-cd.md)を参照してください。

現在はGitHub Actionsが更新を開始し、完了まで待つ構成です。以下の初回反映ではVPSのtimerを使いましたが、その後に削除し、末尾のActions直接実行で置き換えを検証しています。

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

## GitHub Actionsからの直接実行

CDの変更`8df9a3b08d3b978a86882705e7a4a1931bc267b0`をpushし、`Deploy production`を`develop`から実行しました。対象は公開済みの`v0.2.0-rc.13`です。タグ・npm・配布物を再発行せず、Actionsから同じ版を再配備しています。

| 確認 | 結果 |
| --- | --- |
| Actions | [実行36138341235](https://github.com/yukiharada1228/hibana/actions/runs/36138341235)成功。対象SHAのCI・公開マーカー検証、VPS更新、外部疎通確認を完了 |
| CD変更のCI | `8df9a3b`の[通常CI](https://github.com/yukiharada1228/hibana/actions/runs/36137650188)・[Security](https://github.com/yukiharada1228/hibana/actions/runs/36137649771)・[VPS IaC](https://github.com/yukiharada1228/hibana/actions/runs/36137650003)がすべて成功 |
| 更新時間 | ActionsのVPS更新ステップは13:01:20〜13:02:26 UTC。VPSの開始記録は13:01:24、完了記録は13:02:26。約66秒はバックアップと配備・検査全体の時間であり、アプリ停止時間ではない |
| 専用接続 | KAGOYAの`hibana-cp-actions`をCPだけに適用。TCP 2222を公開し、22番の管理元IP制限と他2台のグループを維持 |
| 操作制限 | 専用鍵による実SSH接続で`id`の実行とポート転送を拒否。root helperの別コマンド実行拒否とsystemd終了コード7の伝播も確認 |
| 定期実行の撤去 | `hibana-cd.timer`は`LoadState=not-found`・`ActiveState=inactive`。専用SSHサービスだけが待ち受け、更新時に一時サービスを起動 |
| バックアップ | 708,683 bytesの暗号化DB・設定バックアップを作成。管理者Macにもコピーし、復号したarchiveに両DB dumpが存在することを確認 |
| 本番状態 | 3ノードReady、3つのDeploymentがrc.13で各1 Pod Available。未完了の更新記録なし |
| 公開疎通・既存データ | Actionsと管理者Macの双方からAPI・helloのHTTP 200を確認。Console・OIDC discoveryも200。helloの公開版とコンパイル結果の保護を保持 |
| リソース | VPS追加なし。更新サービスの最大メモリ約191 MiB |

今後はRelease workflowのnpm検証と公開マーカー発行に続き、同じデプロイworkflowを呼び出します。今回の実環境検証は既存リリースを指定した手動起動です。新規タグからの連続実行とは区別します。入力検証・接続先検証・失敗の伝播・成功状態の照合は専用4テストで検証し、CD本体6テスト・IaCレンダー5テスト・workflow構文検証も通過しています。
