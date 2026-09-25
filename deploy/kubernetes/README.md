# Kubernetes deployment

このソースの基盤DBは空DB用の新しい初期スキーマです。旧DBへの自動更新は拒否します。[DBの作成と切替](../../docs/database.md)を確認し、既存の稼働DBとは別の検証先を指定してください。

HibanaはControl PlaneとWasmtime Workerを各2 Pod配置します。アプリケーションごとにPodを作成する方式ではありません。JSは開発者のCLIでWasmにビルドするため、クラスタ内コンパイラーは不要です。

[起動・停止・削除の実機検証結果](../../docs/lifecycle-validation.md)を記録しています。

[レビュー修正後の検証](../../docs/review-fixes-validation.md)では、Hono実行中の停止、成果物の回収・保持、再導入まで確認しています。

[基盤導入CLIの検証](../../docs/cli-platform-validation.md)では、初回導入・設定更新・dry-run・rollout失敗後の再実行を確認しています。

## CLI で基盤を管理する

一般開発者は単体CLIからHTTPS管理APIを操作します。[CLI配布とオンプレ接続](../../docs/remote-cli.md)を参照してください。既存Kubernetesの基盤管理にはNode.js 24以上・kubectl・Python 3とPyYAMLを使います。以下のローカル基盤開発だけは追加でDocker・kind・基盤チェックアウトが必要です。

remote overlayには[コンソールのDeployment・Service・Ingress・NetworkPolicy](console/)を含めています。ブラウザで画面を開き、CLIは同じホストの`/api`へ接続できます。コンソールイメージ、社内DNS、TLS証明書の設定は[導入手順](../../docs/console.md)を参照してください。停止操作はアプリの実行基盤を停止し、コンソールの静的配信は残します。基盤停止中の管理操作は失敗として表示されます。

```bash
npm ci --prefix sdk
# hibana が未インストールの場合は node sdk/src/cli.mjs を使う
node sdk/src/cli.mjs platform install --source .
```

以降の`hibana`は`node /path/to/checkout/sdk/src/cli.mjs`でも実行できます。ローカル基盤も事前に[外部OIDCの必須設定](../../docs/authentication.md#ローカル基盤開発)を環境変数へ設定してください。`hibana platform install --source .`でクラスタ作成、イメージのビルド、依存サービス、DBマイグレーション、起動確認を行います。`HIBANA_ADMIN_OIDC_SUBJECT`を指定した場合は初期テナントも作成します。更新時も同じコマンドです。

ローカルkindのIdP宛て通信許可には`HIBANA_OIDC_EGRESS_CIDRS`（実際の宛先CIDR、カンマ区切り）が必要です。TCPポートは`HIBANA_OIDC_EGRESS_PORTS`で指定し、初回の既定は443です。通信設定はクラスタ別の`oidc-egress.json`に保存され、再導入時に再利用します。

| コマンド | 動作 |
|---|---|
| `hibana platform install --source .` | 専用kind環境を作成・更新して起動 |
| `hibana platform stop --source .` | 新規受付を閉じ、処理完了後に全kindノードを停止。データ・資格情報を保持 |
| `hibana platform start --source .` | 保存した環境を再開し、公開アプリの準備後に受付を再開 |
| `hibana platform status --source .` | ノードの停止状態、Pod・Serviceを確認 |
| `hibana platform uninstall --source . --yes` | 専用kindクラスタとそのデータ・資格情報を削除 |
| `hibana platform test --source .` | `HIBANA_TOKEN`を使ってサンプル配備・HTTP・Secrets・rollbackを検証 |

クラスタ名の既定値は`hibana`です。`--cluster NAME`で変更でき、状態は`.local/kubernetes-NAME/`に保存します。管理APIは`http://127.0.0.1:18080`、アプリHTTPは`http://127.0.0.1:18084`です。ホストポートは共通なので、この構成のクラスタは同時に1つだけ起動してください。専用kubeconfigを使い、普段のcontextは変更しません。

ローカルの`install`・`start`・`stop`・`uninstall`は、クラスタの管理ディレクトリ内の`operation.lock/`で排他制御します。同じ環境への別の操作は、停止情報やノードを変更する前にエラーになります。Kubernetesが停止中でも有効で、`status`と`--dry-run`はロックを取得しません。

通常終了・エラー・Ctrl+C・SIGTERMではロックを解放します。強制終了などで残った場合は、エラーに表示されたディレクトリ内のJSONで操作名・PID・開始日時を確認してください。元のCLIとその子コマンドが終了したことを確認してから、`operation.lock/`だけを削除して再実行します。子コマンドが動き続けている可能性があるため、自動失効はさせません。`maintenance.json`や資格情報は削除しないでください。

```bash
set -a
source .local/kubernetes-hibana/sdk.env
set +a
hibana login
hibana deploy -c sdk/examples/hono/hibana.json
hibana delete hello-hono --yes
hibana delete --all --all-tenants --dry-run
hibana delete --all --all-tenants --yes
```

`--all-tenants`は`BOOTSTRAP_ADMIN_TOKEN`を使い、停止中のテナントや負荷試験用のアプリも対象にします。アプリ削除は公開URLを無効化する論理削除です。実行履歴とWasm成果物をその場では削除せず、同じ名前で新しくデプロイできます。rc.10以降ではアプリ削除後も[実行履歴の保存期間](../../docs/database.md#実行履歴の保存期間)に従って清掃します。実行中のリクエストがある場合は409で拒否するため、トラフィックを止めてから再実行します。

Kustomizeがリソース定義、CLI内のPython処理が「依存サービス → マイグレーション完了 → CP/Worker」の実行順序を担当します。通常のPodにはマイグレーション資格情報を渡しません。イメージと環境設定が同じ再実行ではCP/Workerを強制再起動しません。接続にはNodePortとkindのポート公開を使い、常駐スクリプトやport-forwardは不要です。

### 既存のKubernetesへ配備する

本番のログインにはOIDC対応の認証基盤を使用します。既存の社内IdPへ接続するか、Keycloakを用意してください。[OIDC設定と移行手順](../../docs/authentication.md)に従い、クライアント登録・Control Planeからの通信許可・ユーザー紐付けを行います。旧パスワード方式は削除しました。ローカルkindも外部OIDC設定が必要で、Keycloakの構築・運用は導入先で行います。

社内IdPがSAMLのみの場合は、[Keycloakを仲介するSAML接続手順](../../docs/saml-keycloak.md)を使います。HibanaのOIDC接続先は仲介Keycloakに設定します。

`hibana platform init my-site`でサイト用Kustomize overlayを生成できます。生成されたREADMEに沿って外部PostgreSQL・Redis・S3の接続情報、DNS・TLS・IngressClass、外部依存への通信許可を設定します。署名・暗号化・bootstrap用のキーは自動生成します。秘密値の`.env`ファイルは0600で保存し、`.gitignore`に含めます。キーは安全な場所へバックアップしてください。既存の設定ディレクトリは上書きしません。

Keycloakも同じクラスタで運用する場合、`0.2.0-rc.6`以降の`platform init my-site --with-keycloak`で専用namespace・PostgreSQL PVC・HTTPS Ingress・OIDCクライアントも生成できます。[Keycloakの導入・既存ユーザー移行手順](../keycloak/kubernetes/README.md)に従い、生成した`identity/`を先に個別配備します。既存クラスタ向けのHibana停止・撤去操作の対象には入りません。

CLIのインストール先に基盤のソースは不要です。既存クラスタと配布済みイメージを使います。

```bash
hibana platform init my-site
# my-site/README.mdに沿って設定を入力してから検証
hibana platform install --kubeconfig /path/config --context staging --overlay my-site --image registry.example.com/hibana/platform:VERSION --dry-run
hibana platform install --kubeconfig /path/config --context staging --overlay my-site --image registry.example.com/hibana/platform:VERSION
hibana platform stop --kubeconfig /path/config --context staging
hibana platform start --kubeconfig /path/config --context staging
hibana platform uninstall --kubeconfig /path/config --context staging --yes
```

対象は`hibana` namespace内のHibanaリソースです。`install`が管理対象を記録し、`stop`はCP/Workerを0 Podにして元のreplica数と管理対象HPAを保存、`start`で復元します。外部管理のHPAがある場合は停止前に拒否します。GitOpsなど別のコントローラーと同時に同じDeploymentを管理しないでください。

`--image`は、overlayに指定済みのタグやdigestに優先して、`hibana-control-plane`の`control-plane`、`hibana-worker`の`worker`、`hibana-migrate`の`migrate`コンテナへ適用します。これらのリソース名・コンテナ名を維持してください。Console・sidecar・initContainer・依存サービスには、それぞれのマニフェストで指定したイメージを使います。

既存Kubernetesの`uninstall`は記録されたリソースだけを撤去し、クラスタ・namespace・PVC・外部依存を残します。ローカルkindの`uninstall`はクラスタ内のデータも削除します。

既存Kubernetesへの`install`・`start`・`stop`・`uninstall`は、namespace内の予約済みConfigMap `hibana-platform-operation`で排他制御します。別PCから同時に実行しても、後から来た操作は停止情報やDeploymentを変更する前にエラーになります。ロックの作成と取得・解放にはKubernetesの作成競合と`resourceVersion`による更新競合検出を使います。必要な権限はConfigMapの`get`・`create`・`update`です。`status`と`--dry-run`はロックを取得しません。空のロック用ConfigMapは`uninstall`後も保持します。

通常終了・エラー・Ctrl+C・SIGTERMではロックを解放します。強制終了、PC停止、API応答の喪失などでロックが残った場合は、元のCLIとそのkubectlプロセスが終了したことを確認してから、以下のコマンドで調査・解除し、同じ操作を再実行してください。長時間の導入や通信断で別のCLIが割り込まないよう、自動的な有効期限は設けていません。解除しても保存済みの停止ownerやreplica数は変わりません。

```bash
kubectl --kubeconfig /path/config --context staging -n hibana get configmap hibana-platform-operation -o yaml
# 元の操作プロセスが終了している場合だけ実行
kubectl --kubeconfig /path/config --context staging -n hibana delete configmap hibana-platform-operation
```

### 導入前検査と差分

リモートの`install`はKubernetesへの接続、必要なRBAC権限、未入力の設定、必須環境変数、接続URL、Secret・ConfigMap・ServiceAccount・PVC・IngressClass・TLSの参照を検査します。マイグレーション用のSecretも対象です。既存リソースの所有権と、未完了のマイグレーションJobを確認してから変更を開始します。

`install --dry-run`は同じ検査を実行し、作成・更新・再作成・変更なし・保持するリソースを表示します。既存namespaceではKubernetesのサーバー側dry-run結果を実リソースと比較し、変更するフィールドのパスを示します。Secretや設定の値は表示しません。接続先クラスタへのアクセス権限が必要です。

自動で再作成するJobは、成功済みの`hibana-migrate`だけです。追加したJobは通常の更新として検証し、イメージなど変更できないフィールドの差分は適用前にエラーにします。環境変数の`optional: true`は参照先やキーを省略できますが、Hibanaの必須設定は必要です。ボリューム用Secretはバイナリも利用できます。Secret全体ではなく、環境変数やTLSで必要なキーを文字列として検証します。

Deploymentの環境変数は`envFrom`と個別のSecret・ConfigMapキー参照を合わせて変更検知します。`data`と`stringData`はKubernetesと同じ優先順位で読み取り、initContainerも対象にします。参照値の変更でPodを更新し、個別参照で使っていないキーやBase64の改行だけの変更では更新しません。

Secret・ConfigMapのボリュームも、`install`時にマウント対象のファイル内容を比較してPodを更新します。バイナリ値、projected volume、`items`のキー選択、`subPath`、initContainerのマウント、クラスタ側で管理する参照先も対象です。`subPathExpr`ではボリューム全体の変更を検知します。外部でSecretを更新した場合は、同じ`install`を再実行してPodへ反映してください。

新規namespaceでは、namespace自体をdry-runで検証し、namespacedリソースのサーバー側検証は実導入時のnamespace作成後に行います。`--dry-run`ではこの未検証範囲を表示し、namespaceを作成しません。Admissionで参照する別リソースなど、実際に存在してからでなければ検証できない条件もあります。DBの疎通はマイグレーション時と最終NetworkPolicyの適用後に確認します。後者では全Control Plane・Workerから新規接続でDB、ストア、相手のServiceと各Podへの通信を確認し、Control PlaneからはRedisも確認します。S3はHTTP/TLSの到達性を確認するため、読み書き権限や実アプリの配備は導入後にアプリを配備して確認してください。この検査には`pods/exec`の`get`・`create`権限と、`--maintenance check`に対応したHibanaプラットフォームイメージが必要です。接続検査はPod内の環境変数を使い、診断結果には接続先URLや資格情報を含めません。

`stop`・`start`・`uninstall`のリモートpreviewは現在の管理対象や保存済みreplica数を読み、予定する操作を表示します。これらは操作計画であり、書き込み権限や停止処理の成功を事前に保証するものではありません。ローカルの`install --source . --dry-run`はツール・Docker・マニフェストを検査して構築手順を表示します。イメージのビルドや資格情報の生成は行わないため、それらの差分は表示しません。

### 更新・失敗時の再開

まだワークロードやPodがない初回導入では、NamespaceやTLS Secretを先に作成済みでも、最終NetworkPolicyをマイグレーション・CP・Workerの作成前に適用します。起動途中で失敗しても通信制限を保持します。実際の通信制御にはNetworkPolicy対応CNIが必要です。

停止時は稼働中の各CPの受付済み処理と、DBに記録された実行件数を確認します。Pendingや起動失敗でコンテナが動いていないPodへのexecは省略しますが、稼働中の未Ready・終了中コンテナは確認対象です。確認前後でPodやコンテナの状態が変わった場合は再確認し、応答するCPがない状態を処理完了とは判定しません。

更新も同じ`platform install`を使います。リモート導入では設定適用、マイグレーション、Podのrollout、旧Podの終了待ち、最終NetworkPolicyの適用、依存先への接続確認、公開中アプリのWasm準備確認、管理APIの確認という進行段階を記録します。アプリの準備に失敗した場合も導入失敗となり、成功とは表示しません。通常更新の準備確認は受付を閉じずに行います。失敗時は調査コマンドと、同じ対象への再実行コマンドを表示します。`platform status`でも直近の導入状態を確認できます。

WorkerはDB・実行エンジン・HTTPの初期化後にReadyになります。公開アプリ全件をPod内に常駐させず、ローカルキャッシュにないWorkerは初回アクセスで共有キャッシュまたは元Wasmから復元します。Podの段階更新後には初回アクセスの復元時間が発生します。`WORKER_CACHE_DISK_MIB`は既定2048、範囲1〜2048です。準備用Serviceと`WORKER_PREPARATION_URL`は公開前のコード検証とメンテナンス確認に使用します。復帰時の準備確認は各アプリの実行可能性を確認し、全件の同時常駐は要求しません。


標準のRollingUpdate構成では、計画的なPod終了時に`preStop`で35秒待ち、古いheadless ServiceのDNS応答（30秒TTL）を使う接続にも応答します。その後SIGTERMで通常のdrainへ進みます。既定の終了猶予90秒にはこの待機時間も含まれます。DNSのTTLを長くする環境では両方の猶予を調整してください。新旧Podを並行起動する空き容量が前提です。最小VPS用のRecreate構成ではこの待機を外します。強制終了・OOM・ノード障害による通信断を隠す仕組みではありません。

共有コンパイルキャッシュを有効にするには、Control PlaneとすべてのWorkerが参照する`hibana-runtime` Secretへ`COMPILED_CACHE_KEY`（32バイトの鍵を64桁のhexにした値）を設定します。ローカル管理CLIとVPS用IaCは、保存済みの署名用seedから別用途の鍵を導出して設定し、既存の資格情報は維持します。独自overlayではランダムな専用鍵も使えます。未設定なら従来どおり各Workerでコンパイルします。S3権限には既存のGet/Put/Deleteに加え、キャッシュprefixのListBucketが必要です。鍵の変更や実行環境の互換性変更では共有キャッシュが使えなくなり、必要な成果物を再コンパイルします。既存のローカルキャッシュは自動で共有先へコピーせず、新たなコンパイル時に保存します。容量と認証の詳細は[アーキテクチャ](../../docs/architecture.md)を参照してください。

DBなどの接続先とNetworkPolicyを同時に変更するときは、マイグレーション前に新しい接続先への一時的な許可を追加します。既存の通信許可はrollout完了に加えて旧Podの終了まで保持し、その後に最終ポリシーを適用して一時ルールを削除します。旧Podが終了しない場合は通信許可を残して停止します。最終ポリシー適用後も全Control Plane・Workerで新規接続の成功が継続するまで導入完了にしません。失敗時は設定を直して同じ`install`を再実行できます。すでに通信制限されているPodの範囲にだけ一時ルールを追加するため、selectorの変更で別のPodを途中から制限しません。この処理にはNetworkPolicyの一覧取得と一時ルールの作成・更新・削除権限が必要で、dry-runで事前検査します。残った一時ルールは導入記録に保持し、再実行の完了時または`uninstall`で回収します。`hibana-install-network-`で始まるNetworkPolicy名はこの処理用の予約名です。

起動していないCPへの`stop`は導入記録を停止中に変更しません。停止・再開の途中で失敗した場合は、修正したoverlayとイメージで`install`を再実行して復旧できます。受付制御のownerは修復とアプリ準備が完了するまで保持します。CP不在の環境を撤去する場合も`uninstall`を使えます。PodはKubernetesの終了猶予に従って削除し、外部依存とPVCは保持します。ローカルkindでは導入途中でも`stop`・`uninstall`が使え、停止したノードを`install`が再開してから不足するワークロードを構築します。

失敗したアップロードの回収は、オブジェクト単位で再試行します。マイグレーション`0030`で削除再試行の時刻を追加し、回収対象の保持期限とは分けて管理します。S3の一部のキーで削除に失敗しても、他のテナントや次のバッチの回収を続けます。

マイグレーションが失敗・実行中の場合、Jobを自動削除せず停止します。原因を調査し、失敗したJobを明示的に削除してから再実行してください。DBマイグレーションの自動巻き戻しは行いません。更新前にはDB・オブジェクトストレージ・鍵のバックアップを用意し、復元手順は[運用ガイド](../../docs/on-prem-production.md)を参照してください。

管理用Ingressがある場合は、操作しているMac等から管理APIの疎通を確認し、成功後にAPI URLとログイン・配備のコマンドを表示します。専用APIホストは`/readyz`、コンソールのホストは`/api/readyz`を検査します。両方を設定した場合は両方を確認し、いずれかに接続できなければ導入失敗として記録します。コンソールだけを公開する構成でも検査し、CLIの接続先には`https://コンソールのホスト/api`を案内します。どちらの管理ホストも事前のTLS・信頼するプロキシの設定検査の対象です。

社内CAはPythonの`SSL_CERT_FILE`で設定できます。管理用Ingressがない構成でも全Podの依存先への接続を確認してから、port-forwardによる接続手順を表示します。アプリのログインには管理者が用意したテナントアカウントが必要です。

### 導入CLIの回帰テスト

```bash
python3 sdk/platform/test_kubernetes.py
npm test --prefix sdk
python3 scripts/test-platform-install.py --image hibana-platform:review-recovery
```

実機テストにはDocker・NetworkPolicyを強制するkind（v0.24以降）・kubectl・PyYAMLと、ローカルに保存済みのHibanaイメージ、`postgres:16`、`redis:7-alpine`、`quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z`、`quay.io/minio/mc:RELEASE.2025-08-13T08-35-41Z`、`node:24-bookworm-slim`が必要です。依存イメージは`local/dependencies`のマニフェストと同じタグを使います。`--node-image IMAGE`でキャッシュ済みkindノードイメージも指定できます。

テストは専用の単一ノードクラスタとkubeconfigを作成し、`platform init`で生成した設定から初回導入・更新・変更できないJobの拒否・rollout失敗後の再実行を確認します。任意の環境変数参照、バイナリSecretの実マウント、管理APIのreadinessも検証し、終了時に専用クラスタを削除します。結果は`.local/platform-install/`配下の`report.json`に記録します。外部依存は別namespaceのPostgreSQL・Redis・MinIOで再現し、オンプレ環境固有のDNS・TLS・Ingressや複数ノードの可用性はこのテストの対象外です。

個別のSecretキーだけを変更した後のPod再起動と、DB接続先のIP変更も検証します。DBは同じPostgreSQLへ中継する2つのIPを用意し、切り替え前の新IPへの通信拒否、移行中の両IPへの疎通、rollout完了後も終了中の旧WorkerからDBに接続できること、旧Pod終了後の旧IPへの通信拒否を確認します。IngressなしでWorkerだけの通信を遮断し、Control Planeのreadinessが正常でも導入が失敗することと、設定修正後の復旧も検査します。DBデータの移行やレプリケーションの試験は含みません。

### 旧hibana-dev環境

旧環境の状態は互換性のため`.local/kubernetes/`を使います。`hibana platform stop|start|status --source . --cluster hibana-dev`で操作できます。作成済みkindのポート公開設定は変更できないため、古い設定への`install`は既存データを変更せずエラーにします。新しい`hibana`クラスタを作るか、バックアップ後に明示的に旧クラスタを撤去してください。

ローカルの停止・再開では、実行中のControl Planeのバイナリを起動せずに検査し、メンテナンス操作への対応を確認します。未対応の旧版では、停止にDockerの60秒の終了猶予を使い、再開にAPI・ノード・ワークロードの復帰確認を使います。この経路は受付停止・実行のdrain・アプリの事前準備を行わないため、それらが必要なら新しい検証クラスタへ移行してください。検査時の通信エラー、権限エラー、旧新版の混在では旧版用の経路へ切り替えず、操作を中断します。古いCLIの失敗などで残った`maintenance.json`は旧版操作中に削除せず、対応版へ更新した後の再開で同じownerを使って解除します。

## マニフェスト

| ディレクトリ | 用途 |
|---|---|
| `base` | CP/Worker、Service、PDB、NetworkPolicy、設定。依存サービスは別途用意 |
| `migration` | 所有者権限だけを渡す一回限りのマイグレーションJob |
| `remote` | 既存オンプレ向け管理API・アプリのTLS Ingress例。DNS/TLS/IngressClassはサイトで設定 |
| `local` | 専用kind向けイメージ・設定 |
| `local/dependencies` | 開発用PG/Redis/MinIO。単一Pod・一時ストレージ |
| `persistent-dependencies` | 開発依存のPVC化。HAではない |
| `hardened` | Worker用の管理者指定RuntimeClassとノードプール |
| `autoscaling` | 既存配備に追加するWorker HPA。Metrics Serverが必要。2〜8 Pod |

外部公開は管理API 8080とアプリHTTP 8083を別Ingressへ振り分けます。8081はWorker向け内部API、8084はWorker実行用内部HTTPです。NetworkPolicyのnamespaceラベルと、DNS/TLS/Ingressのホストルールをサイトごとに設定してください。

本番ではイメージを固定digestへ差し替え、Secret参照、依存サービス接続先、監視、TLS、HA、復元試験を用意します。CNIがNetworkPolicyを強制することを確認してください。[本番導入条件](../../docs/on-prem-production.md)に未完の運用要件を記載しています。

Workerはheadless Serviceで発見し、実行前の過負荷拒否時だけ別Podへ振り分けます。Podごとの線形メモリ予約は2 GiB、同時コンパイルは1件です。[スケール設定・負荷試験](../../docs/scaling.md)にHPAの導入方法、DB接続予算、制限の範囲を記載しています。

旧版からのアップグレードでは、停止した旧機能のWasmを再ビルドし、廃止したcompilerリソースをサイトの更新手順で削除します。通常の起動処理に旧版のリソース削除は含めません。

```bash
python3 scripts/check-kubernetes.py
python3 scripts/k8s-local-rollout.py
```

rolloutのスクリプトは専用kind内のPod更新を伴う試験です。本番クラスタでは実行しません。

更新中の配置分散には`matchLabelKeys: [pod-template-hash]`を使用し、旧版Podの数によって新版が同じノードへ偏ることを防ぎます。対象クラスタで当該機能を有効にしてください（[Kubernetes公式仕様](https://kubernetes.io/docs/concepts/scheduling-eviction/topology-spread-constraints/)）。

HTTP MVPへの更新前に旧非同期受付を止め、ジョブを完了させてください。新コードはNATSに接続しません。旧NATSリソースの整理はサイトの更新手順で行い、PVCとDBの履歴データの扱いを別途決めます。

新しい専用kind環境のPostgreSQL/MinIOはPVCを利用します。既存のemptyDir環境は自動で置き換えません。[データを保護したPVC移行・バックアップ・障害試験](../../docs/resilience.md)を参照してください。
