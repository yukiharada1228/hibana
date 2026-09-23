# Kubernetes上のKeycloak

Hibana用Keycloakを専用namespace `hibana-identity`へ配備する、検証環境向けの構成です。Keycloak 26.7.4とPostgreSQL 16を各1 Pod、DBを2 GiBのPVCへ保存します。HibanaのDBとは分離します。単一レプリカのため更新時はログインが一時停止し、ノードやDBの障害に対するHAは提供しません。本番では容量・バックアップ・DB冗長化とKeycloakのクラスタ構成を別途設計してください。

## 設定の生成

この機能は開発ブランチのCLIに含まれます。公開済み`0.2.0-rc.5`には含まれていません。チェックアウトからは次を実行します。

```sh
npm ci --prefix sdk
node sdk/scripts/pack.mjs
node sdk/src/cli.mjs platform init my-site --with-keycloak
```

この変更を含むCLI配布物を導入した場合は`hibana platform init my-site --with-keycloak`でも生成できます。以降は生成先`my-site`で作業します。`identity/`がこのディレクトリのコピーで、Hibana用のルートoverlayとは個別に適用します。既存ディレクトリへの生成は拒否します。

`identity/database.env`・`bootstrap-admin.env`・`client.env`と`control-plane.env`の鍵はランダム生成し、0600で保存します。OIDCクライアント秘密値はKeycloakとHibanaで一致します。`identity/imports/hibana-realm.json`も0600、親ディレクトリは0700です。`.env`と`imports/`はGit対象外です。秘密値とバックアップは別の安全な保管先へ保存してください。`kubectl kustomize identity`や`kubectl diff -k identity`の出力にはSecretが含まれるため、共有ログへ出力しないでください。

## DNS・TLS・通信の設定

次を実際の環境に合わせます。

| ファイル | 設定 |
|---|---|
| `identity/config.yaml` | `KC_HOSTNAME`をKeycloakの外向きHTTPS URL、`KC_PROXY_TRUSTED_ADDRESSES`をIngressプロキシの送信元IP/CIDRにする。callback・console URLも設定する |
| `identity/ingress.yaml` | 同じKeycloakホスト、IngressClass、TLS Secret名を設定する |
| `site.yaml` | issuerを`KC_HOSTNAME/realms/hibana`、callback・console URLを`identity/config.yaml`と一致させる |
| `console/ingress.yaml` | callbackのホストを配信するコンソールのIngress・TLSを設定する |
| `egress.yaml` | `CHANGE_ME_OIDC_CIDR`をControl Planeからissuerへ到達する実際のIngress宛先CIDRへ置き換え、TCP 443を許可する |
| `identity/postgres.yaml` | StorageClass・PVC容量・リソース上限を確認する。既存PVCは作り直さない |

KeycloakのDNSは利用者のブラウザとControl Plane Podの双方から解決できる必要があります。クラスタ内DNSの分岐を使う場合もissuerのホスト名と証明書は同じにします。Pod内で`localhost`を使う転送やPC上の常駐プロキシは必要ありません。

TLSはIngressで終端し、Keycloakの8080へ転送します。Ingressは外部由来の`X-Forwarded-*`を実際の接続情報で上書きしてください。NetworkPolicyはラベル`hibana.io/identity-ingress=true`のnamespaceからのみ8080を許可し、DBは同じnamespaceのKeycloakからのみ接続できます。Ingress側にもegress制限があればKeycloak Podの8080を許可します。CoreDNSのラベルが異なる場合はDNSポリシーを変更してください。9000のhealthポートとDBはService経由で外部公開しません。NetworkPolicyを強制するCNIが必要です。SMTP・外部IdP連携を追加する際は、その送信先だけegressを追加します。[Keycloakのプロキシ設定](https://www.keycloak.org/server/reverseproxy)を参照してください。

## 新しい環境への導入

**既存Keycloakを移す場合は先に次節を読み、空のrealmを起動する前に移行データを用意してください。** 稼働中の同名namespaceへ、この新規生成設定をそのまま適用しないでください。

以下の`k`は対象を明示するためのシェル関数です。kubeconfig・context・Ingress namespace・証明書ファイルを実際の値に置き換えます。

```sh
k() { kubectl --kubeconfig /path/to/config --context onprem "$@"; }
k apply -f identity/namespace.yaml
k label namespace YOUR_INGRESS_NAMESPACE hibana.io/identity-ingress=true --overwrite
k -n hibana-identity create secret tls keycloak-tls \
  --cert=/secure/auth.crt --key=/secure/auth.key
k apply --dry-run=server -k identity
k apply -k identity
k -n hibana-identity rollout status deployment/keycloak-postgres --timeout=180s
k -n hibana-identity rollout status deployment/keycloak --timeout=600s
curl --fail --silent --show-error \
  https://auth.example.internal/realms/hibana/.well-known/openid-configuration
```

TLS Secretを証明書管理コントローラーなどが管理している場合は`create secret`を省きます。社内CAはブラウザ・CLI・HibanaのOIDC HTTPクライアントにも信頼させます。Hibana側は`OIDC_CA_CERT_FILE`でCAをマウントできます。証明書検証は無効にしません。

Discoveryの`issuer`が`site.yaml`と完全一致し、authorization・token・JWKS URLが同じHTTPSホストであることを確認します。`platform install --dry-run`は設定形式を検査しますが、IdPへの実接続は検査しません。実導入後、Control PlaneからのDiscovery取得とログイン時のtoken交換が成功することも確認してください。

管理画面`https://auth.example.internal/admin/`へ`bootstrap-admin.env`の資格情報で入り、恒久管理者と必要なユーザーを作成します。恒久管理者でのログインを確認した後、[一時bootstrap管理者](https://www.keycloak.org/server/bootstrap-admin-recovery)を削除します。Hibanaの初期テナントには、そのユーザーのIDを`admin_oidc_subject`として登録します。メールアドレスからの自動紐付けはしません。Hibanaリポジトリの`docs/authentication.md`を参照してください。

生成realmには一般ユーザーや固定パスワードを含めません。`hibana`クライアントはconfidential、Authorization Code + S256 PKCE、完全一致callbackを使い、Direct Access Grants・Implicit Flow・Service Accountsを無効にしています。その後ルートのREADMEに従いHibanaを導入し、コンソールと`hibana login`でログインを確認します。

## 既存ユーザーを保持して移す

最初は移行元と同じKeycloakバージョンで移します。移行とバージョン更新を同時に行わず、旧DB/ボリュームとHibana DB・鍵のバックアップを保存します。

1. 移行元Keycloakの全Pod/プロセスを停止し、書き込みのない状態でDBバックアップを取得します。PostgreSQL間の移行は完全なDBバックアップの復元を優先し、移行先Keycloakを起動する前に専用DBへ復元します。Keycloakを0 replicaで適用してDBだけ起動し、DB接続ユーザーのパスワードを`database.env`と一致させ、復元完了後に1 replicaへ戻します。
2. H2などから移す場合は、停止した移行元と同じDB設定・ボリューム・イメージで`/opt/keycloak/bin/kc.sh export --dir /secure/export --realm hibana --users realm_file`を実行します。実際のrealm名に置き換えてください。Admin ConsoleのPartial exportではユーザーやパスワードを移せません。エクスポートには資格情報が含まれます。[公式のimport/export仕様](https://www.keycloak.org/server/importExport)を確認してください。
3. 新規環境の初回起動前に、出力された`hibana-realm.json`を`identity/imports/hibana-realm.json`へ0600でコピーします。ユーザーの`id`・`credentials`、realm名を維持し、パスワードを再設定しません。別realm名ならkustomizationのSecretキーと入力ファイル名を`REALM-realm.json`にし、issuerも合わせます。既存DBを復元した場合はこのJSON置換は不要です。
4. 既存クライアントの秘密値を`control-plane.env`の`OIDC_CLIENT_SECRET`と一致させます。生成された`client.env`の値で既存の秘密値が自動更新されるわけではありません。必要なら移行先管理画面でHibana専用クライアントを追加し、そのID・秘密値・callbackを設定します。以前の接続先と並行する場合は別クライアントを使用します。issuer変更時はHibanaの認証設定も更新し、再ログインします。
5. 移行前後のユーザーID、ユーザー数、資格情報の内容が一致することを、値をログへ出さず比較します。既存ユーザーの同じパスワードでのログインと、Hibanaのテナント所属・アプリへのアクセスを確認してから接続先を切り替えます。ユーザーIDが変わらなければ既存の`oidc_subject`を維持できます。

この簡易構成は1 MiB未満の単一realmファイルをSecretへ格納する方式です。大きいrealmや分割exportはSecretへ詰め込まず、別途インポート用ボリューム/Jobを用意します。realm exportはセッションやイベント等を含まないため完全なDBバックアップの代わりにはなりません。移行失敗時はHibanaのissuer・クライアント設定を元に戻し、保持した移行元を再開します。切替後の新規変更を失わないよう、書き込み再開前に復旧方針を決めてください。

## 再適用・更新・バックアップ

`start --import-realm`は既存realmを上書きしません。初回起動後にJSONや`client.env`を編集して再適用しても、既存のユーザー・クライアント設定・秘密値は更新されません。変更は管理画面などで実施し、Hibana側も同期します。`database.env`の変更も既存DBユーザーのパスワードを変更しません。DB上のパスワード変更と両Podの接続設定を計画的に揃えます。initを再実行して鍵を作り直さないでください。

```sh
# 設定更新後。config.yamlの変更は明示的な再起動が必要です。
k apply --dry-run=server -k identity
k apply -k identity
k -n hibana-identity rollout restart deployment/keycloak
k -n hibana-identity rollout status deployment/keycloak --timeout=600s
```

通常運用ではHibanaとKeycloakの両DB・Hibanaの鍵・Keycloak設定/TLS/秘密値をバックアップし、別環境への復元を検証します。Keycloak DBを取得する例です。移行や整合性が必要な復元点では先にKeycloakを停止し、取得後に元のreplica数へ戻してください。

```sh
umask 077
k -n hibana-identity exec deployment/keycloak-postgres -- \
  pg_dump -U keycloak -d keycloak -Fc > /secure/keycloak.dump
```

既存クラスタ向けの`hibana platform install/stop/uninstall`は`identity/`を管理しません。Keycloakの停止は専用Deploymentを0 replicaにし、PVCは保持します。`kubectl delete -k identity`はPVCとnamespaceも削除するため停止に使用しないでください。ローカルkind自体を削除する場合は同じクラスタ内のKeycloakとPVCも失われます。

## 検証

2026-09-23に次を確認しました。

- `npm --prefix sdk test`: 137件成功。生成資格情報の一致・非表示・保存権限、既存ディレクトリの上書き拒否も検査する。
- `python3 scripts/check-kubernetes.py`: 生成overlayのrender、namespace分離、永続DB、Secret参照、HTTPS設定・PKCE・NetworkPolicyの構造を検査する。CIのKubernetes検証にも含まれる。
- `npm --prefix sdk run test:package`: チェックアウト外にnpm tarballをインストールし、このフラグによる生成を含む22件のテストとサンプルのWasmビルドが成功。Wasmtime実行はこの試験では省略。
- kindの使い捨てnamespaceに生成した構成を配備し、server dry-run、両DeploymentのReady、クライアント秘密値の展開、完全一致callback、HTTPS issuerのDiscovery、PKCE付きログイン画面を確認。テストユーザーを作成してKeycloakを再起動し、ユーザーID・パスワード資格情報・クライアント秘密値・PVCの保持を確認後、試験namespaceを削除した。

実機試験のHTTP確認には一時的なport-forwardを使用しました。実DNS/TLS Ingress、NetworkPolicyの通信遮断、別PCからHibanaまでのログイン、既存環境全体の復元はこの試験の対象外です。導入先で上記の手順に沿って確認してください。
