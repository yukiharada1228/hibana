# レビュー修正の検証（2026-09-08）

対象は基盤停止、ローカル再ビルド中の停止、失敗アップロードの回収、公開中の成果物のキャッシュ保持です。修正はソース候補に含まれ、公開済みv0.1.0の検証記録とは別です。

## 再現と結果

| 対象 | 検証内容 | 結果 |
| --- | --- | --- |
| CPの終了 | S3への保存中にSIGTERM。内部APIへ接続し、その後Worker準備と公開を完了 | 内部APIが応答し続け、アップロード201の後にCPが終了コード0で停止 |
| devの競合 | 実ファイル監視で再ビルドを起こし、旧ランタイムの終了待ち中に親へSIGTERM | 旧ランタイムの終了を待ち、新しいランタイムを起動せず終了 |
| 失敗アップロード | 重複版、Worker不在、署名ポリシー変更、保存中のアプリ削除 | 未登録のS3オブジェクトと予約を回収し、登録済み成果物を保持 |
| 回収の再試行 | PUT後の中断を模した期限切れ予約、S3 DELETE失敗、CP再起動 | 未回収の記録を保持して再試行。DB登録済みの版は論理削除後も保持 |
| 保持対象の判定 | 実PostgreSQLで公開版・実行中の版・未公開予約を照会 | Workerの制限付きDBロールで必要なハッシュを取得。実行終了と予約期限切れで保護を解除 |
| キャッシュ容量 | 256個の保護ファイルに件数・バイト容量の圧力をかける | 新規保存を拒否し、既存ファイルを保持。保護解除後は保存可能 |
| 再公開 | ディスク成果物だけを削除し、メモリに残るComponentを準備対象にする | メモリへのヒットだけで公開準備成功としない |
| 独立CLI | tarballをソースツリー外へインストールし、HonoをWasmへ変換して実行 | HTTP 200、Ctrl+Cで正常終了し、HTTPポートが閉じることを確認 |

容量試験は保存アルゴリズムの回帰テストであり、256個のHonoアプリを動かす負荷試験ではありません。ディスク保持とメモリからの再読込は別に検証しています。

## 実行したチェック

- `cargo test --workspace --locked`：272件成功。外部DBが必要な2件はHTTP試験で別途実行。
- キャッシュ再公開の追加修正後に`cargo test --locked -p hibana-worker`：38件成功。
- `cargo clippy --workspace --all-targets --locked -- -D warnings`。追加修正後はWorker全targetも再確認。
- `npm test --prefix sdk`：35件成功。
- `python3 sdk/platform/test_kubernetes.py`：43件成功。
- `python3 scripts/test_resilience.py`：18件成功。
- アーキテクチャ検査、Kubernetesマニフェスト検査、RLS lint、差分の空白検査。
- `bash scripts/test-http.sh`：使い捨てPostgreSQL/Redis、実CP/Worker、S3のHTTPテストサーバーで検証。
- `npm run test:package --prefix sdk`：配布用tarballの独立インストール、16件のCLI検証、実Honoビルド・Wasmtime実行・停止。

## 適用条件

同じ候補からCLIと基盤イメージを用意し、`platform install`でマイグレーション`0029`を適用します。CPのS3資格情報には未登録オブジェクト回収用の`DeleteObject`権限が必要です。停止待ち・再開時の準備に失敗した場合は受付と保存設定を保持し、原因解消後に同じコマンドを再実行します。

## Kubernetesでの実動作

専用kindクラスタ`hibana-review-fixes`、CP 2 Pod・Worker 2 Pod、実PostgreSQL/Redis/MinIOで確認しました。Honoは実際にコンパイルしたWasm Componentとして動作し、外部HTTP先のみ専用Dockerネットワーク内の試験サーバーです。

```sh
docker build -t hibana-platform:review-fixes .
python3 scripts/acceptance/kubernetes.py --seconds 30 \
  --cluster hibana-review-fixes --image hibana-platform:review-fixes
```

検証イメージID：`sha256:ef080fbde6f494a3c09ee10de66939ae79061b2f3acf07274af1f3ec04a2a6d8`。

| 操作 | 実測結果 |
| --- | --- |
| CLIから基盤導入、Honoデプロイ・更新・rollback | 成功。異なるWasm成果物と版ごとの設定を確認 |
| 30秒のHTTP動作確認 | 946要求、全件200、失敗0。観測時間30.21秒 |
| DB/S3/鍵のバックアップ | 別DBへの復元、実Secrets復号、保存成果物のハッシュ検証に成功 |
| 8秒待機する実行の途中でCLIからstop | 新規受付を閉じた状態でHTTP 200と正しい本文を返し、executionの`succeeded`保存後に停止 |
| stop完了後 | CP・Workerとも0 Pod。PostgreSQL/Redis/MinIOは稼働を維持 |
| start、uninstall、保持DBへの再install | 成功。全アプリ準備後に受付を再開し、最初の10要求は全件200 |
| CLIからアプリ削除 | Read+Deploy権限では管理者向け一括削除を拒否。管理者の削除後は一覧0件、公開URLは404 |
| CLIから基盤uninstall | 管理対象のDeploymentと導入記録を削除。外部依存を保持 |
| 試験終了後 | 専用クラスタ・上流コンテナ・ネットワークを削除。後片付けエラー0 |

元のレポートは`.local/mvp-acceptance/20260908T055642Z/report.json`に保存しています。最初の試行では試験側が`stop`へ不要な`--yes`を渡して中断しました。上記はその修正後の成功結果です。この30秒試験は動作確認であり、オンプレ本番のHA・長時間負荷試験の代替ではありません。既存の利用中クラスタと公開済みGitHub Releaseは更新していません。

## 追加修正：版切り替えと削除の競合

追加レビューでは、同じ版への公開切り替えと削除が両方成功し、`active_version_id`が削除済み版を指す不整合を実CP・Worker・PostgreSQLで再現しました。修正前は切り替え200／削除204でした。

版の削除と公開切り替えが、公開トランザクション内で同じcomponent行をロックするようにしました。ロック取得後のSQLで版の状態を読み直し、公開対象が未削除で同じテナント・アプリに属することを確認します。コンパイル・Worker準備中にはこのロックを保持しません。rollbackの既存ロックとHTTP受付の共有ロックも同じ行で競合します。

`scripts/test-version-lifecycle.mjs`をHTTP試験へ追加しました。使い捨てPostgreSQLの行ロックと実際のロック待ちを使い、両方のHTTP要求が処理中になった状態から実行順序を制御します。単なる連続呼び出しによる試験ではありません。

| 競合の順序 | 公開・rollback | 版削除 | 公開アプリ |
| --- | --- | --- | --- |
| 公開切り替えが先 | 200 | 409 | 新しい版で200 |
| 削除が先、その後に公開切り替え | 404 | 204 | それまでの版で200 |
| rollbackが先 | 200 | 409 | rollback先の版で200 |
| 削除が先、その後にrollback | 409 | 204 | それまでの版で200 |

4通りすべて成功し、公開版・previous版の参照、版ごとの設定、削除状態と実HTTP本文を確認しました。rollback用のprevious版が削除409で保護されることも確認済みです。DB層では削除済み版と別アプリの版への切り替えを拒否するケースも追加しました。

追加修正後の検証：CP単体テスト191件成功、`bash scripts/test-http.sh`成功、CP全targetのClippy（警告をエラー扱い）、アーキテクチャ検査、RLS lint成功。上記Kubernetes試験はこの追加修正前の記録であり、追加修正自体はネイティブCP・Workerと使い捨てDBのHTTP試験で確認しています。本追加修正に新しいDBマイグレーションはありません。

## 追加修正：負荷中の停止、導入失敗からの復旧、削除失敗の分離

受付判定中のDB読み取りと停止状態確認をCP内の公平な読み書きロックで同期し、受付済みの処理だけを停止待ちに数えます。閉鎖前のDB応答が遅れて到着しても停止確認を追い越さず、閉鎖後の503応答は停止待ちを増やしません。

実CPに128並列でアクセスしながら停止状態を20回ずつ確認しました。S3待機中のアップロードがある間は`active_requests: 1`を維持し（503応答16,037件）、その完了とCP再起動後はすべて`active_requests: 0`でした（503応答15,315件）。実行件数はどちらも0です。これは受付と停止判定の回帰試験で、処理性能のベンチマークではありません。

CP不在での停止は保存状態を変更せず、撤去はKubernetesの終了猶予とforeground削除でCPのPodを先に除去します。CPが実行中の場合のdrain失敗は引き続き撤去を止めます。未完了の停止・再開で残ったownerは修復インストールに引き継ぎ、準備成功後に解除します。ローカルkindも、CP導入前の停止・撤去と、停止ノードを再開してからの修復に対応します。

マイグレーション`0030_artifact_cleanup_retry.sql`で、回収の再試行時刻を成果物の保持期限と分離しました。実DBとS3のHTTPテストサーバーで、バッチ上限20件を超える25件のDELETEを403にして検証しています。別テナントの回収と次バッチの回収が進み、失敗した記録を残してもキャッシュ保持期限は延びません。障害解除後、すべての未登録オブジェクトを回収し、登録済みの成果物は保持しました。

この追加修正後に、Rust workspace 273件（外部DBの2件はHTTP試験で別途成功）、CLI 35件、基盤操作65件、運用補助18件、workspace全targetのClippy、アーキテクチャ・RLS・Kubernetesマニフェスト検査を確認しています。適用には、このソースから作成した基盤イメージとCLIを使い、`platform install`で`0030`までマイグレーションを適用します。

独立CLIのtarball検証16件も成功しました。12,392,607バイトのHono Wasm Componentを生成し、チェックサム検証でインストールしたWasmtimeランタイムを自動検出してHTTP応答を確認しています。Ctrl+C後にランタイムが終了し、HTTPの待受が閉じることも確認しました。

実Kubernetesの追加検証は、`python3 scripts/test-platform-install.py --image hibana-platform:review-recovery`で実施しました。専用kindクラスタ`hibana-install-check-ff121c1d`、Kubernetes v1.37.0、外部依存を模した別namespaceのPostgreSQL・Redis・MinIOを使っています。

| 障害・操作 | 結果 |
| --- | --- |
| Workerの起動コマンドを壊して更新 | rollout失敗を記録し、既存状態を保持 |
| 壊れたWorkerを含む基盤をCLIでstop、その後start | stop成功。start失敗後も復旧用ownerを保持 |
| Worker設定を修正してinstall | 修復、準備、受付再開に成功し、停止途中の記録を解除 |
| 正常な基盤をuninstallして、CPの起動を壊してinstall | CP起動失敗を正しく記録 |
| CP不在でstop | 停止状態を誤って保存せず、再導入を妨げない |
| CP不在の環境をCLIでuninstall | CP/Workerと導入記録を除去。namespaceと外部依存3 Deploymentを保持 |
| CP設定を修正してinstall、その後さらに撤去・再導入 | すべて成功。実ポート転送先のreadinessでDB・共有ストア正常を確認 |
| 試験終了後 | 専用kindクラスタを削除 |

イメージIDは`sha256:da85cf67c6f8085cfb4cd717e8d2481eff1b0d18232b8f1b6d3b3a9eff094e21`です。結果は`.local/platform-install/hibana-install-check-ff121c1d/report.json`に保存しています。単一ノードの障害回帰試験であり、オンプレ本番のHA検証とは別です。

## 2026-09-10: 初回の通信制限・操作競合・CP起動失敗時の停止

NamespaceやTLS Secretを先に用意した初回導入でも、マイグレーションとCP/Workerの作成前にNetworkPolicyを適用するよう修正しました。既存のワークロードやPodがある更新では、従来の通信許可を保持する移行処理を継続します。Namespaceマニフェストをoverlayから省いた構成と、rollout失敗後も通信制限を保持するケースを回帰テストに含めています。

既存Kubernetesへの`install`・`start`・`stop`・`uninstall`を、予約済みConfigMap `hibana-platform-operation`で排他制御します。取得と解放は作成競合・`resourceVersion`の条件付き更新で保護し、同時停止による受付制御ownerの上書きを防ぎます。ロック取得後に導入状態を再確認し、撤去処理から呼ぶ停止は同じロックを使います。長い操作が期限切れで競合しないよう自動解放期限は設けていません。強制終了・API応答喪失後の確認手順と、ConfigMapの追加権限`get`・`create`・`update`は[基盤管理ガイド](../deploy/kubernetes/README.md)に記載しています。

drainでは未起動・停止済みコンテナを省略し、稼働中のCPは未Ready・終了中も含めて確認します。Pod UIDだけでなくコンテナの状態変化も検出して再確認し、稼働中CPからの応答がない場合や実行中処理が残る場合は停止しません。

この修正後に、基盤操作96件、CLI 35件、運用補助18件、独立パッケージ16件が成功しました。競合した新規ロック作成と既存ロック更新、操作中の別CLI、古いownerからの解放拒否、取得応答喪失、割り込み時の解放、PendingからRunningへ変わるPodを検証しています。パッケージからHonoの12,392,541バイトのWasm Componentを生成し、ローカルWasmtimeでのHTTP応答とCtrl+C後の待受終了も確認しました。npm公開は実施していません。

実Kubernetesの試験は次のコマンドで実行しました。

```bash
python3 scripts/test-platform-install.py --image hibana-platform:review-operations
```

専用kindクラスタ`hibana-install-check-0e3df6f0`、Kubernetes v1.37.0、別namespaceのPostgreSQL・Redis・MinIOを使用しています。イメージIDは`sha256:d3ed709a3eef6b2131a13f99109bcf1668fa10ff1539246ed67cbab26ae44b50`です。

| 実行条件 | 結果 |
| --- | --- |
| 作成済みNamespaceへの初回導入 | マイグレーション中、CP/Worker作成前のNetworkPolicyを確認 |
| 導入中に別CLIからstop・start・uninstall | すべて拒否し、ロックownerと停止情報を保持 |
| 正常な基盤のstop・start | 成功。解放済みロックの再取得も確認 |
| 正常CPと起動に失敗したCPが混在 | stop成功。壊れた設定でのstart失敗後もownerを保持し、設定修正後のinstallで受付再開 |
| CPが一度も起動できない導入 | stopが停止情報を誤記録せず、uninstallと再導入が成功 |
| 正常CPと起動に失敗したCPが混在したままuninstall | 撤去成功。さらに再導入し、実HTTPのreadinessでDB・ストア正常を確認 |
| 更新中・更新後の通信 | 旧Worker終了までのDBアクセス維持、最終ポリシーでのWorker通信障害検出・修復も成功 |
| 後片付け | 専用クラスタ削除済み |

全結果は`.local/platform-install/hibana-install-check-0e3df6f0/report.json`に保存しています。単一ノードの機能・障害回帰試験であり、巨大オンプレクラスタでの負荷・HA・実サイトのDNS/TLS/RBACを検証したものではありません。
