# KAGOYA 2GB × 3台のHibana

月額上限2,310円のVPS 3台を前提にした、小規模・停止許容の構成です。通常のKubernetesをkubeadmで作ります。管理ノード1台、認証/APIノード1台、実行/DBノード1台であり、HAではありません。ローカルでの[容量測定](../docs/vps-capacity-validation.md)と[同時実行測定](../docs/vps-concurrency-validation.md)を初期設定の根拠にしています。

| 管理対象 | 定義 |
|---|---|
| VPS・事業者側ネットワーク | KAGOYA管理画面。公開API/providerを確認できていないため手動 |
| プライベートNIC・OS・containerd・kubeadm・Calico | `ansible/network.yml`、`ansible/bootstrap.yml` |
| DNS Aレコード | `terraform/dns/`。既存Cloudflare zoneに追加 |
| 永続ストレージ・Traefik・認証・Hibana | `ansible/deploy.yml`、`render.py`。既存Kustomizeと導入処理を再利用 |
| 秘密情報 | Ansible Vault。実行時だけ0600のファイル・Kubernetes Secretへ展開 |

対象は新しく用意した専用Ubuntu 24.04サーバーです。既存の別用途サーバーには適用しません。初期化済みの別クラスタは拒否し、`kubeadm reset`・PVC削除・自動DB復元は行いません。Kubernetesのバージョンが既存と違う場合も停止し、意図しないアップグレードを避けます。

新規VPSのS3互換ストレージはGarage 2.3.0です。[MinIO Communityは保守終了](https://github.com/minio/minio)となり、空のVMで既存イメージを取得できなかったため変更しました。Garageはイメージdigestを固定し、単一ノード・レプリケーション1で使います。既存ローカル環境のMinIOデータを変換・流用する処理は含みません。Hibanaが必要とするPut/Get/Deleteと署名付きURLの互換性をローカルVMで確認しました。

## 最初に管理画面で用意するもの

1. KAGOYAの2CPU・2GB・NVMeプランを3台。Ubuntu 24.04、公開鍵認証、固有のホスト名を指定します。
2. 3台を同じローカルネットワークへ接続し、OS上のプライベートNICに固定IPv4を設定します。`inventory.example.yml`のアドレスは例です。Pod CIDR・Service CIDR・LAN/VPNと重複しない範囲を選びます。
3. 事業者側のセキュリティグループで、管理者IPからのSSH、プライベートNIC上の3台間通信、認証/APIノードだけの公開TCP 80/443を許可します。6443・10250・etcd・DB・Redis・Garageは公開しません。Calicoはノード間UDP 4789のVXLANを使います。
4. SSHホスト鍵の指紋をKAGOYAコンソール等で確認して、操作PCのknown_hostsへ登録します。Ansibleはホスト鍵検証を無効にしません。
5. Cloudflareに`hibana.cloud`のzoneを作成します。ドメイン登録はお名前.comに残します。既存DNSレコード（MX/TXT/DNSSECを含む）を確認・引き継いだ上で、別途NS切替を行います。Terraformはzone・NS・既存の無関係なレコードを変更しません。

KAGOYAの[作成手順](https://support.kagoya.jp/vps/manual/)と[機能一覧](https://www.kagoya.jp/vps/function-plan/)を参照してください。これはサーバー3台の料金であり、外部バックアップなどの費用は含みません。

## 操作環境と設定

リポジトリのルートで実行します。Python 3.12以降、SSH、Terraform 1.11以降が必要です。検証にはkubectlも使います。

```sh
python3 -m venv .local/iac-venv
.local/iac-venv/bin/pip install -r infra/requirements.txt
mkdir -p .local/vps
cp infra/ansible/inventory.example.yml .local/vps/inventory.yml
cp infra/site.example.yml .local/vps/site.yml
.local/iac-venv/bin/python infra/init-secrets.py .local/vps/vault.yml
```

inventoryの公開IP・秘密鍵のパス・プライベートIP・管理者のSSH接続元CIDRを記入します。siteのノード名をinventoryと一致させ、管理者メールを設定します。`pod_cidr`はinventoryとsiteの両方を一致させます。Proxy用の2つの固定Pod IPは同じCIDR内、Ingress用Service IPはService CIDR内にします。Calicoが固定Pod IPを割り当て、Hibana/Keycloakはそのプロキシだけを信頼します。

Vaultの`cloudflare_dns_token`へ、このzoneに限定した **Zone:DNS:Edit / Zone:Zone:Read** トークンを設定します。証明書のDNS-01検証専用とし、Terraformの操作用トークンとは分けます。値はターミナルの引数やGitに置きません。

```sh
.local/iac-venv/bin/ansible-vault encrypt .local/vps/vault.yml
```

Vaultファイルとその復号パスワードは別々の安全な保管先にも保存します。再実行時は同じVaultを使います。`init-secrets.py`は既存ファイルを上書きしません。署名鍵・暗号鍵・DBパスワード・初期ユーザー情報の変更は、既存DBに反映されない変更やデータ喪失を防ぐためrendererが拒否します。ローテーションは各サービスの明示的な移行手順で行います。

## 構築順序

### 1. OSとKubernetes

KAGOYAのUbuntu 24.04テンプレートではSSHユーザーは`ubuntu`です。公開NICとプライベートNICを`ip -brief address`で識別し、inventoryの`private_interface`を実機に合わせます。次の設定は未使用のプライベートNICだけに固定IPv4を追加し、公開NICとデフォルトルートを保持します。別の設定方法で内部IPが設定済みの場合は、network.ymlを省略できます。

```sh
ANSIBLE_CONFIG=infra/ansible/ansible.cfg .local/iac-venv/bin/ansible-playbook \
  -i .local/vps/inventory.yml infra/ansible/network.yml

ANSIBLE_CONFIG=infra/ansible/ansible.cfg .local/iac-venv/bin/ansible-playbook \
  -i .local/vps/inventory.yml infra/ansible/bootstrap.yml
```

Kubernetes 1.36.5、Calico 3.32.2を固定しています。初回に3台を構築し、再実行では既存クラスタへ再参加しません。join tokenは15分で失効し、ログには出しません。ホストの入力通信は専用nftablesテーブルで制限し、Kubernetesが管理する転送ルール全体を消しません。

### 2. DNS

`terraform/dns/terraform.tfvars.example`を`terraform.tfvars`へコピーし、zone IDと**認証/APIノードの公開IPv4**を記入します。認証トークンは秘密情報管理ツール等から`CLOUDFLARE_API_TOKEN`環境変数へ設定します。

```sh
terraform -chdir=infra/terraform/dns init
terraform -chdir=infra/terraform/dns plan -out=changes.tfplan
terraform -chdir=infra/terraform/dns apply changes.tfplan
```

既存の同名レコードは勝手に上書きしません。管理対象にする場合は先に`terraform import 'cloudflare_dns_record.hibana["hibana.cloud"]' 'ZONE_ID/RECORD_ID'`等で取り込み、planを確認します。初期状態では操作PCにstateを保存します。state・planはGit対象外です。安全な場所へバックアップし、複数人/CIでapplyする前にロック付きremote backendへ移行してください。

作成する名前は`hibana.cloud`、`auth.hibana.cloud`、`*.local.apps.hibana.cloud`です。DNS-only（proxied=false）で直接VPSへ接続します。アプリURLは`hello.local.apps.hibana.cloud`のように2階層増えるため、`*.apps.hibana.cloud`一枚の証明書では対応できません。

メールを使わないドメインで、移行前にnull MX（`0 .`）とSPF（`v=spf1 -all`）がある場合は、`disable_domain_mail = true`でこの設定も引き継ぎます。既存メールサービスがあるドメインでは有効にせず、そのMX/TXTを保持します。Cloudflareの自動スキャンは、お名前.comの初期ワイルドカード応答を多数の個別レコードとして取り込むことがあるため、登録元の設定と照合します。

NSの切替と伝播を確認してから次へ進みます。Traefikの起動後に、公開IPへの80/443疎通とHTTPS証明書を確認します。

### 3. Hibanaと認証・HTTPS

DNSの反映待ちには、以下のコマンドへ`--tags images,prepare`を追加して、イメージ取得と秘密情報を含む設定の配置まで先行できます。証明書の発行とサービス起動は、NS反映後にタグ指定なしで実行します。

```sh
ANSIBLE_CONFIG=infra/ansible/ansible.cfg .local/iac-venv/bin/ansible-playbook \
  -i .local/vps/inventory.yml infra/ansible/deploy.yml \
  -e @.local/vps/site.yml -e @.local/vps/vault.yml --ask-vault-pass
```

GitHub ReleaseのCPUアーキテクチャ別イメージをSHA256SUMSで検証してcontainerdへ取り込みます。レジストリ用の追加サーバーは不要です。依存DB → Keycloak/Traefik → マイグレーション → Hibanaの順で待機し、最後にHTTPS接続を検証します。Hibanaの更新手順は既存の`platform install`実装を使い、失敗したmigration Jobは保持します。失敗時は管理ノードの`/opt/hibana/install.log`を確認してください。秘密情報を含む可能性があるため、そのまま共有しないでください。

TraefikがCloudflare DNS-01でLet's Encrypt証明書を取得・更新し、ACMEの状態はPVCへ保存します。PVCの再マウント時に`fsGroup`でファイル権限が変わるため、起動前のinit containerが既存データを保持したまま`acme.json`を`0600`へ戻します。稼働は1 Podで更新時に短い停止があります。公開リクエストの転送ヘッダーはTraefikの標準設定で扱い、任意クライアントの転送元ヘッダーを信用しません。認証issuerは内外とも`https://auth.hibana.cloud/realms/hibana`のまま、クラスタ内DNSだけIngress Serviceへ向けて折り返し通信を避けます。

初回のみ`owner`ユーザーと最初のテナントを作ります。初期パスワードはVaultの`owner_password`で、初回ログイン時に変更します。本人のプロフィール・パスワードを管理するため、`account`クライアントの`manage-account`と`view-profile`を明示的に付与します。Keycloakの一時管理者は`bootstrap-admin`、パスワードはVaultの`keycloak_admin`です。恒久管理者を作成・動作確認した後、一時管理者を削除してください。realm importは既存DBのユーザーを上書きしないため、既存ユーザーの権限修復は管理画面または管理APIで行います。

初回設定は `https://auth.hibana.cloud/realms/hibana/account/` から先に完了し、その後でCLIにログインします。認証中の各入力画面の有効時間（Login action timeout）は15分です。期限切れでログイン画面へ戻った場合、パスワード変更が未保存なら初期パスワード、保存済みなら新しいパスワードを使います。プロフィール入力や次画面への遷移に失敗しても、パスワード変更は保存されている場合があります。既存realmの有効時間はimportでは変更されないため、管理画面または管理APIから明示的に変更します。

```sh
hibana login --url https://hibana.cloud/api --tenant local
```

## 再適用・運用の境界

- 同じinventory/site/Vaultで同じplaybookを再実行します。DB・鍵・PVを作り直しません。Hibanaイメージの更新はsiteの`version`を変更してdeployを実行します。
- 操作元のGit checkoutも対象Hibanaバージョンに対応するものを使います。イメージだけを過去/未来の版に変更して、異なる版のマニフェストや導入処理と組み合わせません。
- Kubernetesの更新は[公式kubeadm upgrade手順](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/kubeadm-upgrade/)で、バックアップと計画停止を用意して行います。bootstrapのバージョン変更だけでは更新しません。
- テナント追加時はTerraformとsiteの`tenant_slugs`を一致させて、DNS・Ingress・証明書を追加します。2件目以降のテナントと所属ユーザーの作成は管理APIで行います。
- 平常時は単一レプリカです。Workerの同時実行8、ゲストメモリ予約合計1GiB、Pod上限1280MiB。256MiBアプリなら4件、128MiBなら8件が予約上の目安です。コンパイル・CPU負荷・アプリ実使用量を含む安定性は実VPSでも測定します。
- DBとオブジェクトはノードローカルのPVです。`Retain`は誤削除への保護であり、バックアップやノード障害からの自動復旧ではありません。PVの容量指定はディスククォータではないため、ディスク使用量・空き容量を監視します。
- この段階では監視スタック・外部バックアップ先の契約は含みません。公開運用前にDB/オブジェクト/鍵/ACME状態の外部バックアップと復元確認を行います。

### バックアップと復元

[既存のバックアップ・復元手順](../docs/on-prem-production.md)を使い、対象をこの構成へ合わせます。バックアップはVPSと別の障害領域へ暗号化して保管します。

1. Hibanaの`platform stop --kubeconfig ... --context kubernetes-admin@hibana-vps`で受付停止・実行完了を待ち、Keycloakも停止して書き込みを止めます。
2. Hibana用PostgreSQLとKeycloak用PostgreSQLをそれぞれ`pg_dump`で保存します。DBユーザー・所有者情報と対応するVaultを一組で保持します。
3. Garageを停止して実行ノードの`/var/lib/hibana/objects`全体（`meta`と`blocks`の両方）を保存します。Traefikも停止して認証/APIノードの`/var/lib/hibana/traefik`を保存します。DBのデータディレクトリを稼働中に単純コピーしません。
4. 元のレプリカ数へ戻し、Keycloak・ストレージ・HTTPSが正常になってからHibanaを再開します。バックアップ失敗時も受付再開の可否を明示的に判断します。
5. 復元は別の隔離先で、同じバージョン・同じVault・同じユーザーIDで行います。DB復元とオブジェクト配置が終わるまでKeycloakとHibanaを起動しません。既存の生成済みrealmを新規ユーザーで置き換えません。issuer変更は[認証移行手順](../docs/authentication.md)に従います。

## ローカルVM検証

[lab/README.md](lab/README.md)と[検証結果](../docs/vps-iac-validation.md)を参照してください。独立したUbuntu VMを3台使い、OSから同じAnsibleを適用します。kindだけではOS初期設定・kubeadm・再実行の検証にならないためです。Terraformはmock providerを使い、Cloudflare APIへアクセスせず検証できます。

```sh
.local/iac-venv/bin/python infra/tests/test_render.py
terraform -chdir=infra/terraform/dns init -backend=false
terraform -chdir=infra/terraform/dns validate
terraform -chdir=infra/terraform/dns test
```

CIではこの検証とAnsible構文検査を実行します。リモートへのapplyやVaultの復号は行いません。

### 共有コンパイルキャッシュ

レンダラは保存済みの署名seedから、用途を分けた`COMPILED_CACHE_KEY`を導出して`hibana-runtime`へ設定します。対応する新しいControl Plane・Workerでは、既存Garageバケットへコンパイル結果を保存し、新しいPodで再利用します。追加のサービスやPVCは不要です。共有キャッシュは2 GiB・256件までで、元Wasmと別のprefixから古いキャッシュを回収します。鍵を変更すると共有成果物は再作成されます。詳しくは[キャッシュ設計](../docs/architecture.md)と[ローカル検証結果](../docs/vps-shared-cache-validation.md)を参照してください。

Workerのローカルコードは公開アプリ全件を保持せず、容量を超えた分を退避し、必要時にGarageから復元します。アプリの切替えでPodは作り直しません。最小VPS構成のPod更新は、新旧Podの同時配置に必要な余裕がないため`Recreate`のままです。標準のRollingUpdate用DNS終了猶予は外し、旧Podの予約を早く解放します。この最小構成はPod交換中の無停止を保証しません。
