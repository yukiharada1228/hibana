# HTTPS環境を別PCから試す

公開npm版`0.2.0-rc.10`で、Hibanaのリポジトリ・Docker・kubectlを持たない開発者のPCから、ログイン・作成・deploy・tail・rollbackを確認します。ここにあるURLは設定例です。実環境の接続先とテナント名へ置き換えてください。

## 管理者が用意する接続先

LAN・VPN・インターネットのどこから接続させるかを決め、対象PCから到達するDNSとHTTPS入口を用意します。

| 用途 | URL例 | 転送先 |
| --- | --- | --- |
| コンソール | `https://hibana.example.internal/` | `hibana-console:8080` |
| 管理API | `https://hibana.example.internal/api` | コンソール経由の`hibana-api:8080` |
| アプリ | `https://remote-pilot.team.apps.example.internal/` | `hibana-apps:8083` |
| 認証 | `https://auth.example.internal/realms/hibana` | `keycloak:8080` |

`APP_PUBLIC_ORIGIN`は`https://apps.example.internal`に設定します。アプリURLはその前に`<アプリ>.<テナント>`を追加するため、`*.apps.example.internal`という証明書だけでは足りません。この例では`*.team.apps.example.internal`のDNS・Ingressルール・証明書が必要です。別テナントにはそのテナント用の設定を用意します。

KeycloakのissuerはPCとControl Planeの両方から、同じHTTPS名で到達できるようにします。固定の`KC_HOSTNAME`、信頼するプロキシの送信元とヘッダー処理は[Keycloak公式のhostname設定](https://www.keycloak.org/server/hostname)と[reverse proxy設定](https://www.keycloak.org/server/reverseproxy)に従います。既存ユーザーを移す際は[issuer変更の手順](authentication.md#既存環境のissuerをhttpからhttpsへ変更する場合)も実施します。

社内CAの場合はブラウザへCAを信頼登録し、CLIには`NODE_EXTRA_CA_CERTS=/path/to/company-ca.pem`、Control Planeには`OIDC_CA_CERT_FILE`を設定します。アプリをcurlで確認する際も同じCAを`--cacert`で指定します。CAの公開証明書を配布し、秘密鍵は管理者が保持します。

管理者は切り替え後に次を確認します。

- 対象PCからコンソール・管理APIの`/api/readyz`・アプリへ証明書エラーなしで接続できる。
- PCとControl Planeの双方からOIDC Discovery・JWKSに接続でき、Discoveryのissuerが設定値と完全一致する。
- Keycloakのクライアントに`https://hibana.example.internal/api/auth/oidc/callback`が完全一致で登録されている。
- コンソールからのログインと、以下のCLIログインの両方でtoken交換を完了できる。

開発者へ渡すのは管理API URL、テナント名、組織アカウント、CLIバージョン、必要ならCAの公開証明書です。通常の試用にはRead・Deploy権限を使います。

## 開発者のPCで実行する

Node.js 24以上を用意し、CLIとブラウザを同じPCで使います。既存アプリとの衝突を避け、テナント内で未使用のアプリ名を選んでください。以下は`remote-pilot`が未使用の場合の例です。

```sh
npx --yes @yukiharada1228/hibana@0.2.0-rc.10 login \
  --profile pilot --url https://hibana.example.internal/api --tenant team
npx --yes @yukiharada1228/hibana@0.2.0-rc.10 init remote-pilot
cd remote-pilot
```

`src/index.ts`を次の内容にします。HTTP応答とアプリのログで版を確認できる最小のアプリです。

```ts
import { Hono } from 'hono'

const app = new Hono()
const release = 'pilot-v1'
app.use('*', async (c, next) => {
  await next()
  console.log(JSON.stringify({ release, method: c.req.method, path: c.req.path, status: c.res.status }))
})
app.get('/', (c) => c.json({ release }))
export default app
```

```sh
npm run deploy -- --profile pilot --version pilot-v1
npm exec -- hibana tail --profile pilot --format pretty
```

tailが接続完了を表示した後、別のターミナルからdeployで表示されたURLを呼びます。

```sh
curl --fail --show-error https://remote-pilot.team.apps.example.internal/
curl --show-error --output /dev/null --write-out '%{http_code}\n' \
  https://remote-pilot.team.apps.example.internal/missing
```

最初は`{"release":"pilot-v1"}`、次は404を期待します。tailで両方のHTTPステータスと`release: pilot-v1`のアプリ出力を確認し、Ctrl+Cで終了します。HTTP 404もアプリが正常に応答した場合は`Ok`です。過去の実行はコンソールで確認します。

`src/index.ts`の`release`を`pilot-v2`へ変更してから実行します。

```sh
npm run deploy -- --profile pilot --version pilot-v2
# 同じアプリURLが {"release":"pilot-v2"} を返すことを確認
npm exec -- hibana rollback --profile pilot --version pilot-v1
# 同じアプリURLが {"release":"pilot-v1"} に戻ることを確認
npm exec -- hibana list --profile pilot
```

途中で認証期限が切れた場合は最初のloginを再実行します。最後にCLIを終了し、別の端末からもアプリが応答することを確認します。開発PCを停止して試す場合、k8sのホストは稼働させたままにします。

## 記録する結果

OS・Node.js・CLIの版、利用したネットワーク、各操作の成否、初回配備までの時間、手順だけでは分からなかった点を記録します。パスワード・トークン・認証中のURLは記録しません。HTTPS疎通だけの確認、同一PCでの確認、別PCからの本人ログイン完了を区別してください。
