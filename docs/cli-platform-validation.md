# 基盤導入CLIの検証

2026-09-10、Macから専用の単一ノードkindクラスタ（Kubernetes v1.37.0）を操作して検証しました。CLIは作業ツリーの実装を使用し、PostgreSQL・Redis・MinIOをHibanaとは別namespaceに配置しました。今回の追試では`hibana-platform:review-network-health`、イメージID`sha256:07b72831e531b7e0f67b27d4c62dfe1757be84724fe653c9a6d39652353e391d`を使用し、36項目が成功しました。

## 修正内容

- Jobの再作成対象を成功済みの`hibana-migrate`に統一しました。追加Jobの変更は通常のapplyとして事前検証し、変更できないPod templateは本適用前に拒否します。追加Jobの削除権限も要求しません。
- `optional: true`のSecret・ConfigMap参照を省略できるようにしました。参照先やキーがない場合は`envFrom`の値を保持し、Hibanaの必須環境変数は引き続き検査します。
- Secretの存在・キー確認と文字列検証を分けました。ボリューム用のバイナリ、参照していないバイナリキーを許容し、`stringData`による上書きを優先します。環境変数やTLSとして使用するキーは文字列として検証します。
- 実機検証で見つかった、Kustomizeが長いBase64値に挿入する改行の誤拒否も修正しました。CR/LFを含む有効な値を読み込め、サーバー保存後の値と比較して余分な差分を表示しません。不正なBase64は引き続き拒否します。
- Pod更新の判定を、`envFrom`と個別のSecret・ConfigMapキー参照を解決した値に統一しました。`data`と`stringData`を混在させたSecret、initContainer、任意キーの追加・削除にも対応します。個別参照で使用しないキー、上書きされる値、同じ値のBase64表現の違いでは余分な更新を起こしません。
- DB接続先とNetworkPolicyの同時変更では、マイグレーション前に新しい許可を追加します。既存の許可はrollout完了と旧Podの終了まで保持し、最終ポリシーの適用後に一時ルールを回収します。一時ルールの権限とAdmissionも事前検査し、失敗時は導入記録に残して再実行・撤去時に回収します。
- `kubectl rollout status`の成功後も終了中の旧Podを待ちます。終了待ちがタイムアウトした場合は通信許可を残して停止し、導入記録に`old Pod termination`を保存します。追加Deploymentのrolloutも待ちます。
- 最終NetworkPolicyの適用と一時ルールの回収後に、全Control Plane・Workerで新規接続を検査します。DB、S3のHTTP/TLS、相手のService・各PodへのTCP、Control PlaneからのRedisが対象です。Podの入れ替わりや終了開始があれば結果を無効にし、連続した成功を待ちます。Ingressのない構成でも実施し、失敗時は`dependency verification`として導入未完了にします。
- 検査用の`--maintenance check`はPod内の環境変数を使い、診断結果には接続先URLや資格情報を含めません。必要な`pods/exec`権限は変更開始前に確認します。新しいPythonモジュールをnpmパッケージにも同梱します。

通信ルールの組み合わせは、[NetworkPolicyの加算的な許可](https://kubernetes.io/docs/concepts/services-networking/network-policies/)と[LabelSelectorの条件](https://kubernetes.io/docs/concepts/overview/working-with-objects/labels/)に従います。selectorや`policyTypes`が変わる場合も、移行前または移行後に許可される通信だけを移行中に許可することを、ラベル・方向・ポートの組み合わせで検査しました。

## 結果

| 検証 | 結果 |
| --- | --- |
| Python回帰テスト | 92件成功。旧Podの終了待ち、全replicaの新規接続、最終ポリシーの失敗・再実行を含む |
| Rust接続検査テスト | 3件成功。IPv4・IPv6のServiceと各PodへのTCP、不正な引数の拒否を検査。Control Planeのclippyも成功 |
| JavaScript CLIテスト | 35件成功 |
| npm配布パッケージ | ソースツリー外へのインストール、16件のCLIテスト、HonoのWasm Componentビルドが成功。新しいreadinessモジュールの同梱も確認 |
| 新規namespaceへのdry-run | 成功。namespaceが作成されないことを確認 |
| `platform init`で生成した設定から初回導入 | マイグレーションとControl Plane・Workerのrolloutが成功 |
| 任意参照とバイナリSecret | 任意参照を省略した状態で起動。マウントしたバイナリのバイト列と、個別に参照した文字列キーを実コンテナで確認 |
| 既存環境へのdry-run・設定更新 | マイグレーションJobだけを再作成。追加JobのUIDは保持 |
| 個別Secretキーの変更 | `data`と`stringData`を併用し、参照キーだけを変更。WorkerのPod UID更新とコンテナ内の新しい値を確認 |
| DB接続先IP・egress許可の同時変更 | 切り替え前は新IPを拒否、マイグレーション中は両IPに疎通、旧Pod終了後は旧IPを拒否。一時ルールも消去 |
| rollout後も旧Workerが終了中 | preStopで旧Workerを30秒残し、rollout成功後・旧Pod終了前にそのWorkerから旧DBへ新規接続できることを確認 |
| 最終ポリシーでWorkerだけ通信不可 | Ingressなしで検査。Control Planeの`/readyz`がHTTP 200でも`dependency verification`で失敗を記録 |
| 通信ルールを直して再実行 | 一時許可を再構築し、全Podの新規接続確認後に導入完了へ復旧。一時ルールを回収 |
| 追加Jobの変更できないtemplateの更新 | dry-runとinstallの両方がサーバー検証で拒否。導入記録と追加JobのUIDは不変 |
| Worker rolloutの意図的な失敗 | 失敗段階を保存。`platform status`が導入未完了を表示。旧・新DBの両IPへの通信許可を保持 |
| 設定を修正して同じinstallを再実行 | 完了状態に復旧。追加JobのUIDを保持し、一時ルールを回収。停止・再開が途中で失敗した状態からの復旧も確認 |
| CPが起動できない場合の撤去・再導入 | `stop`が停止状態を誤記録しないこと、`uninstall`後に外部依存を保持し再導入できることを確認 |
| 復旧後の管理API | port-forward経由の`/readyz`がHTTP 200、`db`と`store`が`ok` |
| 後片付け | 専用クラスタを削除 |

成功した追試の結果は`.local/platform-install/hibana-install-check-72e341d7/report.json`にあります。個別のCLI出力とサイト設定は同じ非公開ディレクトリに保存されています。先行検証の記録は`.local/platform-install/hibana-install-check-5772fe84/report.json`と`.local/platform-install/hibana-install-check-93dd899d/report.json`です。

## 再実行

```bash
python3 sdk/platform/test_kubernetes.py
cargo test -p hibana-control-plane dependency_probe
npm test --prefix sdk
HIBANA_PACKAGE_OFFLINE=1 npm run test:package --prefix sdk
python3 scripts/test-platform-install.py --image hibana-platform:review-network-health
```

[実機テストスクリプト](../scripts/test-platform-install.py)は専用クラスタとkubeconfigを毎回生成し、終了時にクラスタを削除します。既存のKubernetes contextは使用しません。必要なローカルイメージなどは[導入CLIの回帰テスト](../deploy/kubernetes/README.md#導入cliの回帰テスト)を参照してください。

単一ノードの検証用設定を使用しており、本番オンプレ環境のDNS・TLS・Ingress、実サイトのRBAC、複数ノードでの可用性は未検証です。DB接続先の2つのIPは同一PostgreSQLへの中継で、データ移行・レプリケーションの試験ではありません。今回の更新テストは設定更新を対象にしており、異なるHibanaリリース間のDB互換性や実アプリの配備は対象に含みません。npmパッケージ検証ではローカルWasmtime runtimeの実行を省略しています。

kindはIPv4構成です。接続検査のIPv6 URL正規化は、クラスタ用イメージのビルド後に追加し、MacのIPv6ループバックを使ったRust回帰テストで確認しました。S3の接続検査は署名なしのHEADによるHTTP/TLSの到達性確認であり、バケットの存在・IAM・オブジェクトの読み書き権限を保証するものではありません。
