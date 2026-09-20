# 認証・OIDC

Hibanaの対話ログインは、開発環境も含めOpenID Connect（OIDC）に統一しています。組織の認証基盤で本人確認・MFA・パスワード復旧を行い、Hibanaはテナント所属、操作権限、APIトークンの有効期限と失効を管理します。Hibanaは標準OIDCクライアントとして動作し、Keycloakなど特定製品のAPIには依存しません。既存のMicrosoft Entra IDなども、以下のOIDC契約を満たす設定で接続できます。Entra IDとの実環境接続試験は別途必要です。

`hibana dev`でアプリだけをローカル開発する場合、認証基盤は不要です。

社内基盤がSAMLの場合は、[Keycloakを仲介する接続手順](saml-keycloak.md)を使用できます。署名付きSAMLからHibanaのコンソール・CLIまでを通す自動試験と設定テンプレートを用意しています。Hibana自体の接続方式はOIDCです。

## 実装と責務

Rustの[`openidconnect` 4.0.1](https://docs.rs/openidconnect/4.0.1/openidconnect/)を使用します。Discovery、Authorization Code + S256 PKCE、IDトークンの署名・issuer・audience・有効期限・nonce検証はこのライブラリに委ねます。HTTP通信は既存の`reqwest` + `rustls`を使い、リダイレクト禁止・応答サイズ上限・タイムアウトを適用します。OAuthやJWT専用のライブラリを別途組み合わせる必要はありません。

Hibanaに残すのは、ログイン開始と戻り先、単回のstateとコンソール/CLIへの引き渡し、`issuer + sub`の照合、テナント所属・権限、Hibanaのセッション/APIトークンの発行・失効・監査です。パスワード保存・照合・再設定、MFA、SAMLの処理と仲介、認証用アカウントの運用は接続先の責務です。Keycloakは任意の外部認証基盤であり、Hibanaのインストーラーは構築・運用しません。

`POST /auth/login`、コンソールのメール/パスワード入力、CLIの`--email`・`--password-stdin`・`HIBANA_PASSWORD`によるログインは削除しました。新規テナント・ユーザー登録ではOIDC subjectが必須で、パスワードフィールドは拒否します。

## 設定

Control Planeの環境変数に設定します。設定は全レプリカで統一してください。

| 設定 | 内容 |
|---|---|
| `OIDC_ISSUER_URL` | Discoveryを提供する発行元。例：`https://sso.example.com/realms/hibana` |
| `OIDC_CLIENT_ID` | 登録したクライアントID。例：`hibana` |
| `OIDC_CLIENT_SECRET` | クライアントの秘密値。Control Plane用Kubernetes Secretに保存 |
| `OIDC_CALLBACK_URL` | 認証基盤から戻る固定URL。例：`https://hibana.example.com/api/auth/oidc/callback` |
| `OIDC_CONSOLE_URL` | コンソールの正確なURL。例：`https://hibana.example.com/` |
| `OIDC_SESSION_TTL_SECS` | Hibanaログイントークンの有効期間。既定900秒、60〜3600秒 |
| `OIDC_CA_CERT_FILE` | 任意。認証基盤の社内CAを追加するPEM証明書ファイル（複数可） |

上記の設定URLはHTTPSが必須で、資格情報・クエリ・フラグメントは含めません。Discoveryで取得する認可・トークン・JWKSエンドポイントはクエリを許可し、その値を保持して利用します。これらもHTTPSが必須で、資格情報・フラグメントは拒否します。issuerはDiscoveryの`issuer`と完全に同じ文字列を指定します。末尾の`/`も区別し、Hibana側では補正しません。`OIDC_ALLOW_INSECURE_HTTP=true`はループバック上の試験専用で、`localhost`・`127.0.0.1`・`[::1]`のHTTPだけを許可します。Kubernetesの本番導入設定では使用しません。

認証基盤のDiscovery・JWKS・トークンエンドポイントへControl Planeから到達できる必要があります。ブラウザも認証基盤へ接続します。生成されたサイト設定の`hibana-site-identity-provider` NetworkPolicyに宛先CIDRとポートを設定してください。社内CAを使用する場合はCA証明書をControl Planeへ読み取り専用でマウントし、`OIDC_CA_CERT_FILE`を指定します。ブラウザの信頼ストアと、CLIから接続するHibana側の証明書に対する`NODE_EXTRA_CA_CERTS`も設定します。証明書検証を無効にする設定はありません。

OIDC設定が不足すると起動を拒否します。`AUTH_MODE`は廃止しました。`oidc`を含むすべての値を拒否するため、環境変数とKubernetesの設定から削除してください。認証基盤やRedisの障害時にパスワード認証へ自動切り替えはしません。OIDC開始・コールバック・交換を複数のControl Planeに分散できます。短命な処理状態はRedisに保存し、原子的に一度だけ消費します。

## 認証基盤に登録するOIDCクライアント

導入環境ごとに1つのissuerを設定します。必要な契約は次のとおりです。

- DiscoveryとJWKSを公開し、署名付きIDトークンを返す。
- confidential clientのAuthorization CodeフローとS256 PKCEに対応する。
- `client_secret_basic`または`client_secret_post`でクライアントを認証する。
- `OIDC_CALLBACK_URL`を正確に登録し、安定した`sub`を発行する。

ユーザー情報API・メール属性・グループ属性・refresh tokenは要求しません。要求するOIDCスコープは`openid`だけです。

## Keycloakでの登録

以下は任意の接続例です。SAML仲介やKeycloakの運用は導入先が担当します。

公式ガイド：[サーバー管理](https://www.keycloak.org/docs/latest/server_admin/)・[コンテナ運用](https://www.keycloak.org/server/containers)。本番には`start`と永続DB・HTTPSを使用してください。`start-dev`はテスト用です。

1. 組織用のrealm（例：`hibana`）を作成します。
2. OpenID Connectクライアント`hibana`を作成し、Client authenticationを有効にします。
3. Standard flowを有効にし、Implicit flow・Direct access grantsを無効にします。
4. Valid redirect URIsに`OIDC_CALLBACK_URL`の完全なURLを登録します。ワイルドカードは不要です。CLIもこの共通コールバックを使います。
5. PKCEの方式を`S256`にします。クライアント認証は`client_secret_basic`または`client_secret_post`を使用します。
6. クライアントの秘密値を`OIDC_CLIENT_SECRET`へ設定します。
7. ユーザーを作成し、組織の方針に沿ってMFAを必須にします。OIDCを有効にするだけではMFAは必須になりません。
8. Keycloakのユーザー詳細にあるID（OIDCの`sub`）を取得し、Hibanaのユーザーへ明示的に紐付けます。

Keycloakを全導入先へ追加する必要はありません。既存の認証基盤がある場合は、それを直接接続します。Microsoft Entra IDでは特定ディレクトリのissuerを使用します。`common`などの複数issuerを許す構成は対象外です。

## テナントとアカウントの作成

最初の管理者は、bootstrap用資格情報を使う既存の`POST /admin/tenants`で登録します。OIDCモードではパスワードを送信しません。

```json
{
  "slug": "team",
  "name": "Team",
  "admin_email": "admin@example.com",
  "admin_oidc_subject": "認証基盤で確認したユーザーのsub"
}
```

以後のユーザー作成はAdmin権限の`POST /tenants/{tenant_id}/users`です。

```json
{
  "email": "developer@example.com",
  "role": "member",
  "oidc_subject": "認証基盤で確認したユーザーのsub"
}
```

本人の照合には設定済みissuerと`sub`を使います。メールアドレスは表示・管理用であり、同じメールアドレスの別ユーザーを自動的に紐付けません。ユーザーは事前登録制です。同じOIDCユーザーが複数テナントを利用する場合は、各テナントに所属を登録し、ログイン時に対象テナントを選びます。

初期管理者と追加ユーザーは、作成時からOIDC identityを含む1行として登録します。メールまたはidentityが同一テナントで重複すると409を返し、ユーザーと監査記録の作成はまとめて取り消します。

## コンソール・CLI・CI

コンソールではテナントを入力し、「組織のアカウントでログイン」を選びます。認証基盤でログイン後、元のコンソールへ戻ります。Hibanaのセッション資格情報はHttpOnly・SameSite=Strictのホスト限定Cookieに保存し、HTTPS環境ではSecureと`__Host-`接頭辞を付けます。JavaScriptには資格情報を返さず、localStorage・sessionStorageにも保存しません。再読み込み・新しいタブでは`GET /auth/session`で所属・権限・有効期限を再確認して画面を復元します。期限はログイン時から固定で、再読み込みでは延長しません。

コンソール専用の`POST /auth/oidc/browser/exchange`だけがCookieを発行し、戻り先がコンソールのログイン結果に限定します。CLIは従来の`POST /auth/oidc/exchange`でBearerトークンを取得します。Cookie認証では専用ヘッダーと設定済みコンソールのOrigin／Fetch Metadataを検証し、同じサイトの別サブドメインも信頼しません。各画面は読み込んだセッションIDにリクエストを紐付け、別タブでアカウント・テナントが変わった場合の操作を拒否します。認証済みAPI応答は`Cache-Control: no-store`です。Cookie名はコンソールのオリジンごとに分け、異なるポートのローカル環境を区別します。Cookie属性と送信元検証は[OWASPのセッション管理](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html)・[CSRF対策](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)に沿った構成です。

画面の期限タイマーは`GET /auth/session`の`expires_in_ms`（サーバーで計算した残り時間）を使い、端末の時計のずれによる誤ったログアウトを防ぎます。期限の強制は各APIリクエスト時にサーバー側で行います。

ブラウザのリダイレクト中だけ、単回の照合値とPKCE verifierをsessionStorageに保存します。戻り先のフラグメントに含まれるコードは60秒間だけ有効で、verifierなしではセッションへ交換できません。復帰時にフラグメントとsessionStorageの一時値を削除します。認証基盤のトークンはブラウザ・CLIへ渡しません。

```sh
hibana login --url https://hibana.example.com/api --tenant team
```

CLIは既定のブラウザを開きます。自動起動ができない場合は表示されたURLを**CLIと同じPC**のブラウザで開きます。`--no-browser`で自動起動を省略できます。CLIは`127.0.0.1`の一時ポートでコールバックを受け、最大10分で待受けを終了します。SSH接続先でのブラウザなし対話ログインやDevice Authorization Grantは現時点では対象外です。

CIではAdmin権限で`POST /tokens`から発行した必要最小限のスコープ・有効期限のAPIトークンを`HIBANA_TOKEN`に設定します。OIDCモードでもこの方式を使えます。トークンはユーザーに紐付き、そのユーザーの停止・全トークン失効に従います。

## 失効と停止

ユーザーの作成・紐付け・失効・停止はHibana側のテナント境界で制限し、監査ログを残します。

テナント停止中も、有効な認証情報で自分のセッション確認とログアウト（全セッション失効を含む）はできます。アプリ・ユーザー管理などのテナント操作と新しいログインは拒否します。ブラウザを再読み込みしても、停止されたテナントのセッションからログアウトできます。

| API | 対象 |
|---|---|
| `POST /auth/logout` | 提示された現在のトークンだけを失効 |
| `POST /auth/logout-all` | 自分のログイン・APIトークンすべてを失効 |
| `POST /users/{user_id}/revoke-tokens` | Adminが同じテナントの指定ユーザーの全トークンを失効 |
| `DELETE /users/{user_id}` | Adminが同じテナントのユーザーを論理削除し、ログイン・既存トークンを無効化 |
| `GET /users` | Adminが同じテナントのユーザーID・表示メール・ロール・OIDC subjectを確認 |

全トークン失効は、失効前に本人確認を終えた未交換のログインコードも無効にします。失効後に新しく認証し直すことは可能です。継続的にアクセスを止める場合はユーザーを停止します。アカウントに紐付いたCIも止まるため、CIには用途別の専用アカウントを使用してください。

APIトークンの発行・ユーザー作成・OIDCの紐付け変更は、本文の受信を終えた後、DBロック下で要求元のトークン・認証世代・現在の権限を再確認します。失効や停止が先に完了した場合、すでに受け付けていたリクエストでも新しい認証情報を作成・変更しません。別ユーザー向けの操作にも同じ検証を適用します。ユーザー停止・ユーザーの全トークン失効・全ログアウトでも、DBロック待機後に実行者を再検証します。一般ユーザー自身の全ログアウトにはAdmin権限を要求しません。

Hibanaのログアウトは認証基盤全体のSSOセッションを終了しません。また、認証基盤だけでアカウントを停止しても、発行済みのHibanaトークンは有効期限まで残る場合があります。OIDC Back-Channel Logout・SCIM同期・自動更新は未実装です。即時停止が必要な場合はHibana側でも上記APIを使って停止してください。CLIの`hibana logout`は従来通りローカルの保存トークンだけを削除します。

## OIDC専用版への切り替え

旧版との混在運用・旧トークンの継続利用はサポートしません。新規導入ではOIDC設定とbootstrap APIだけで利用できます。

既存DBを使用する場合は、DBのバックアップを取得し、すべてのControl Planeを停止してから`m20260920_000008_oidc_only`まで適用します。この変更はパスワードハッシュ列、メールからの認証用SQL関数、旧トークン発行の互換トリガーを削除します。既存のテナント・ユーザー・トークン行・監査ログは保持し、全トークンと未交換のログイン結果を失効させます。削除したパスワードハッシュは復元できないため、downによる巻き戻しは拒否します。旧版への復旧には切り替え前のバックアップを使用してください。

既存の管理者にOIDCが未登録の場合は、停止中に管理用DB接続で、対象テナント・ユーザーIDを指定して確認済みの`oidc_issuer`と`oidc_subject`を設定し、変更を監査記録へ残します。メールアドレスによる自動紐付けは行いません。`AUTH_MODE`を削除し、全Control Plane・CLI・コンソールを同じ版とOIDC設定で起動します。管理者がOIDCで再ログインした後、必要なAPIトークンを再発行してください。マイグレーションの再実行では新たに発行したトークンを失効させません。

適用済みのマイグレーションファイルは履歴として保持します。現行スキーマと実装にパスワードの保存・検証・旧Control Plane向けの互換経路はありません。

## ローカル基盤開発

`hibana dev`だけなら認証基盤は不要です。Control Planeやkind環境を起動する場合は、外部OIDCを設定してください。`hibana platform install`の初回実行では上記5つの必須OIDC設定を環境変数として渡します。kind内のControl Planeとブラウザの両方からissuerへ接続できる必要があります。ローカル基盤はKeycloakを自動配置しません。

ローカルkindでは、外部IdPの宛先を`HIBANA_OIDC_EGRESS_CIDRS`（カンマ区切りのIPv4/IPv6 CIDR）と`HIBANA_OIDC_EGRESS_PORTS`（TCPポート、初回の既定は`443`）で指定します。例：`HIBANA_OIDC_EGRESS_CIDRS=192.0.2.10/32,2001:db8::10/128 HIBANA_OIDC_EGRESS_PORTS=443`。この例の予約アドレスは実際のIdPの宛先に置き換えてください。Discovery・JWKS・トークンの全エンドポイントを含めます。`/0`は受け付けません。Control Planeだけを対象とした`hibana-local-identity-provider` NetworkPolicyを適用し、Workerの通信許可は広げません。

通信設定はクラスタの状態ディレクトリの`oidc-egress.json`に保存し、再導入時に再利用します。IdPのIP変更時は上記環境変数で該当設定を上書きして`install`を再実行するか、保存ファイルの`cidrs`配列・`ports`配列を修正します。`--dry-run`は有効な通信設定を表示し、保存ファイルを変更しません。既存クラスタの更新でも、初めてこの設定を使う場合はCIDR指定が必要です。

任意で`HIBANA_ADMIN_EMAIL`と`HIBANA_ADMIN_OIDC_SUBJECT`を渡すと、初期テナントの管理者を作成します。subject未指定なら自動作成を省略し、`POST /admin/tenants`で登録します。既存テナントとの競合時は紐付けを変更しません。既存の`.local/.../secrets.json`は自動で鍵を変更しないため、Control Plane用SecretへOIDC設定を追加してから再適用してください。

管理者のメールは空にできず、subjectは制御文字なし・UTF-8で255バイト以内です。subjectは大文字小文字や空白を含め、そのまま保存します。クラスタ作成前にこれらを検証し、保存済みの`sdk.env`も現行形式の必須項目を確認します。`HIBANA_URL`、`HIBANA_TENANT`、`BOOTSTRAP_ADMIN_TOKEN`、`HIBANA_ADMIN_EMAIL`、`HIBANA_ADMIN_OIDC_SUBJECT`（自動作成を省略する場合は空文字列）が必要です。旧キーへの読み替えは行わず、不足時は修正対象のファイルと項目を示して停止します。

ローカル導入では、クラスタ作成・資格情報の保存前にOIDCの必須設定、URL、有効期間を検証します。`--dry-run`でも同じ検証を行います。保存済みの`secrets.json`がある場合はその設定を使用するため、環境変数だけを変更しても置き換わりません。エラーに表示されたファイル内の`hibana-control-plane` SecretのOIDC設定を修正し、その他の鍵は維持してください。

`make bootstrap SMOKE_OIDC_SUBJECT=...`も同じ登録APIを使います。`make login`はCLIのブラウザログインを開き、接続プロファイルを保存します。非対話の実行試験は必要な権限を持つ`HIBANA_TOKEN`を使用します。

## 検証

```sh
npm ci --prefix console --ignore-scripts
npx --prefix console playwright install chromium
bash scripts/test-oidc.sh
```

ループバック限定の一時PostgreSQL・Redis・Keycloak 26.7.4を使います。二つのControl Planeをまたぐコード交換、コンソール・CLI、PKCEとコード再利用拒否、同一メールの別主体の拒否、テナント境界、全トークン失効、権限降格、論理削除、旧パスワード経路とトークンの拒否を検証します。任意の[SAML仲介の互換性試験](saml-keycloak.md#自動検証)は`HIBANA_TEST_SAML=1 bash scripts/test-oidc.sh`で追加実行します。既存環境へは接続しません。

CIでは`HIBANA_TEST_HTTPS=1 HIBANA_TEST_SAML=1 bash scripts/test-oidc.sh`を使用します。一時CAでIdPとコンソールのHTTPSを構成し、CPは`OIDC_CA_CERT_FILE`、CLIは`NODE_EXTRA_CA_CERTS`で証明書を検証します。CA未設定のCPが接続を拒否することと、ブラウザのSecure・`__Host-` Cookieによる復元・失効を確認します。ブラウザだけは一時証明書の公開鍵を明示して許可し、ホストの信頼ストアを変更しません。これは隔離された自動試験であり、導入先のPKIや別の利用者による試用は別途確認します。
