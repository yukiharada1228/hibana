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

## CI/CDレビュー後の再検証

`89949856ffa91a5b758f589db73adea96eb10ce7`と`69a118302c19287ec4753d614e973856697dd4fb`で次の指摘を修正し、再レビューしました。

| 指摘 | 修正 |
| --- | --- |
| 更新中にcontroller・接続鍵・設定を上書きできる | 配備の稼働確認、SSH受付停止、共通ロックとmaintenance印を導入。設定完了まで配備を拒否 |
| 部分的なDB移行後の再試行で古いコードを指定でき、更新前バックアップの参照も残らない | 失敗したコミットからの前進を検査。最初の更新前バックアップを`attempt.backup`に記録し、再試行中も保持 |
| バックアップがDB全量をメモリに保持し、データ増加で更新サービスの上限に達する | ディスクへの出力とGnuPG暗号化へ変更。復号・SHA-256照合後に保存。途中失敗では既存バックアップを保持 |
| ディスク出力への変更後、バックアップがCPの空き容量を使い切り得る | 各出力前に2GiBの余裕を確認。外部プロセスにはファイルサイズ上限を設定し、archive作成前にも必要量を確認 |
| 古いバックアップ削除の失敗を、本番配備の失敗として報告する | 成功済みの更新を維持し、整理失敗は警告として記録 |
| キャンセルされたReleaseでも後続の配備ジョブが開始し得る | 後続ジョブに`!cancelled()`を要求 |

ローカルではCD本体11・実tar/GnuPGバックアップ4・SSH境界4・IaCレンダー5・Release gate 6の計30テスト、Ansible構文検査、actionlintが成功しました。64MiBのDBフィクスチャに対してPython側のピーク割り当ては12MiB未満です。この数値は外部プロセスを含む総メモリではありません。実際の復号内容一致、異なる鍵と破損データの拒否、途中失敗、ロック競合、旧新両バックアップ形式の整理を検証しています。空き容量を模擬したテストではdump開始前・archive作成前の拒否と、子プロセスの出力が上限1KiBを超えないことを確認しました。

[Actions実行36141645627](https://github.com/yukiharada1228/hibana/actions/runs/36141645627)でrc.13を再配備し成功しました。VPS更新ステップは13:33:07〜13:34:16 UTCです。実行中に`cd.yml`を再適用すると、controllerやworker鍵への変更前に拒否され、Actions側の接続は維持されました。通常の設定適用ではmaintenance印が解除されることも確認しています。

容量制限を追加した`69a1183`もAnsibleで本番CDへ反映し、設置済みcontrollerのSHA-256がレビュー済みファイルと一致することを確認しました。この追加分はLinuxの[VPS IaC](https://github.com/yukiharada1228/hibana/actions/runs/36143023956)でも容量不足・ファイルサイズ制限の回帰テストが成功しています。[Security](https://github.com/yukiharada1228/hibana/actions/runs/36143023946)と[通常CIの全4ジョブ](https://github.com/yukiharada1228/hibana/actions/runs/36143024231)も成功しました。

新しい176,264 bytesの暗号化バックアップを管理者Macへコピーして復号し、7ファイルと両DB dumpを確認しました。helloは前回の検証後に作り直されていたため、今回の更新直前バックアップに入った現行アプリ`cmp_50856ab2f27c4cc2a23e8213d5fa6649`・公開版`ver_33853310e1fb424b99179e1d42a5a161`・Wasm hashを現在のDBと比較し、一致を確認しました。公開版のコンパイル結果は保護対象です。3ノードReady、3 Deploymentがrc.13でAvailable、Console・API・OIDC・helloはHTTP 200、未完了実行は0件でした。

再レビュー時点で、このCI/CD変更範囲に未解決の指摘はありません。これは新規タグからの全リリース連続実行や、サイト全体の災害復旧を検証したという意味ではありません。
