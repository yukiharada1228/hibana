# VPS向けIaCのローカル検証

検証日: 2026-09-24。実装・操作手順は [infra/README.md](../infra/README.md)。この検証ではVPSを契約せず、CloudflareのAPI・公開DNS・お名前.comのNS設定・Let's Encrypt本番発行を変更していません。

機械可読の主要結果は [vps-iac-validation.json](vps-iac-validation.json) に保存しています。

## 構成と範囲

Lima 2.2.0上の独立Ubuntu 24.04.4 VMを3台作成し、各2 vCPU・2GiB RAM・swapなしで、`infra/ansible/bootstrap.yml`と`deploy.yml`を適用しました。Kubernetesはkubeadm 1.36.5、Calico 3.32.2です。K3s/kindではありません。ARM64のMac上で動かしているため、KAGOYAのx86 VPSのCPU・ディスク・回線性能を保証する実験ではありません。

| VM | 役割 | 仮想ディスク |
|---|---|---:|
| hibana-iac-cp | Kubernetes管理・etcd | 30GiB |
| hibana-iac-management | Traefik、Keycloak、認証DB、Hibana API、Console、Redis | 40GiB |
| hibana-iac-execution | Hibana Worker、Hibana DB、Garage | 50GiB |

Pod以外のOS・containerd・kubeletも2GiBの枠内に含みます。CP・DB・ストレージ・Ingressはそれぞれ単一で、3台構成でもHAではありません。永続データはローカルPVです。

DNSとCAは検証VM内に限定した`hibana.iac.test`を使いました。Macのhosts・キーチェーンには変更していません。本番のDNS-01証明書発行経路は、実際のDNS切替後に別途検証が必要です。

## 確認結果

- OS・containerd・kubeadm・CNIの初回構築に成功。最終版のbootstrap再実行は3台とも`changed=0 / failed=0 / unreachable=0`。
- GitHub ReleaseからHibana 0.2.0-rc.10のARM64イメージをSHA256SUMS付きで取得し、containerdへ導入。
- マイグレーション・初期テナント作成・HTTPSの管理API確認までAnsibleが完了。
- KeycloakのHTTPS認可コード＋PKCEログイン、プロフィール更新、初期パスワード変更、Hibanaセッション取得に成功。
- 約12.4MB（12,392,954 bytes）のJavaScript/Hono WasmをHibana APIへアップロード。GarageへのPutObject、署名付きGetObject、Workerのコンパイルを経て、`https://hello.local.apps.hibana.iac.test/`がHTTP 200、本文`capacity-ok`を返しました。
- Garageに対するSigV4署名付きPut/Get/Delete/Get-404を確認。Hibanaの非active version削除も確認しました。Hibanaのversion削除は論理削除なので、即時のオブジェクト削除とは分けて検証しています。
- 同一設定のdeploy再実行前後で、Node・PV・PVC・常駐PodのUIDとSecret値のSHA256が完全一致。マイグレーションJobの再実行は既存インストーラーの仕様です。`stringData`のapply等はAnsible上でchangedになりますが、Podの置換や鍵の再生成はありませんでした。
- Garage/WorkerのPod再起動と、その後のVM3台の停止・起動を実施。既存アプリのHTTP 200、変更済みパスワードでのOIDCログイン、追加アップロードに成功。Node/PV/PVCのUID、Secret値、DBのテナントとOIDC subjectの対応が保持されました。VM再起動時には依存サービスの起動待ちで一部コンテナが再試行し、その後Readyへ復帰しました。OSのfirewall/containerd/kubeletも3台ともactiveでした。
- PythonのKustomize/インストーラー契約テスト4件、Ansible構文検査2件、Ruff、Terraform fmt/validate/mock test 2件が成功。Terraform applyは実行していません。

### 軽負荷と制限動作

同じMacで他の開発プロセスも動いており、以下は短時間の動作確認です。長時間の性能保証や最大同時実行数の測定ではありません。

| 条件 | 結果 |
|---|---|
| 4クライアント、各200ms間隔、20秒 | 400/400件 HTTP 200、本文不一致0件 |
| 上記の応答時間 | p50 24.5ms、p95 50.6ms |
| 4クライアント、待ち時間なし、30秒 | HTTP 200: 1,985件、HTTP 429: 9,544件、200の本文不一致0件 |

待ち時間なしの最初の試験は「全件200」の判定を満たしませんでした。HTTP 429を成功件数へ含めていません。同条件で8秒間の追加試験を行い、HTTP 200は894件、429は1,610件でした。429の全件が`error.code=rate_limited`で、既定の呼び出しレート制限が作動していました。500/503や本文不一致はありませんでした。

稼働時のkubelet statsの一時点で、ノードworking set / availableは以下でした。availableはカーネルによる回収可能メモリを考慮した値で、最大使用量ではありません。

| ノード | Working set | Available |
|---|---:|---:|
| Kubernetes管理 | 1,115.7MiB | 842.8MiB |
| 認証/API | 1,060.4MiB | 898.1MiB |
| 実行/DB | 872.2MiB | 1,086.2MiB |

Workerは同時実行上限8、ゲストメモリ予約合計1GiB、Pod上限1,280MiBです。アプリが256MiBを予約する場合はメモリ予約上4件までで、CPU負荷・コンパイル時の余裕を別途考慮する必要があります。

## 空のVMで見つかった問題と修正

- 元のMinIO/mcイメージは、この検証の新規VMからタグ・digestとも取得できませんでした。既存kindはキャッシュを持っていたため、以前の容量検証では表面化しませんでした。[MinIO Communityの公式保守終了](https://github.com/minio/minio)も確認し、ユーザー承認を受け、新規VPSのみ[Garage 2.3.0](https://garagehq.deuxfleurs.fr/documentation/quick-start/)へ変更しました。既存ローカルのMinIOデータは変更していません。
- Ubuntu VM間通信がVZ NATでは分離されていたため、検証用ネットワークをLima user-v2へ変更。実VPSでは事業者のプライベートNICを使います。
- Kubernetes aptリポジトリ追加後のキャッシュ更新、Calico VXLAN専用のヘルスチェック、Restricted Pod Securityに適合するDB/ストレージ権限、PVのfsGroup、Traefikの必要な読み取り権限、検証CAをOIDCクライアントへ渡す設定を修正。
- bootstrap再実行が既存PVディレクトリの権限を上書きしないよう修正。
- Keycloak realmの初期ユーザーに`UPDATE_PASSWORD`を明示し、初回のパスワード変更を要求。既存realmのユーザーをimportで上書きしません。
- ホストの競合負荷があった初期段階にKubernetes API/コントローラーの再起動を観測しました。また、DB権限修正前にKeycloakが再起動しました。検証全体を「再起動なし」とは評価していません。

検証終了後、3台のLima VMはデータを保持して停止しました。一時停止した既存`hibana-dev`・`hibana-console`は復帰済みです。元のAPI・Console・Keycloak discoveryがHTTP 200を返し、Console側の受付停止も解除されたことを確認しました。別プロジェクトのサービスには停止・変更を行っていません。

## 実VPSへの移行前に残る確認

1. 実契約プランでプライベートNIC・公開80/443・SSH接続元制限を確認し、inventoryのIPとホスト鍵を確定。
2. Cloudflare zone・既存DNSレコードを確認し、Terraform planレビュー後にDNS/NSを切替。
3. DNS-01の実証明書発行・更新、外部PCからのログイン・deploy・tailを確認。
4. DB・Garageのメタデータと本体・Vault・ACME状態を別の障害領域へバックアップし、隔離環境で復元確認。
5. 実VPS上で通常アプリの負荷、OOM、ディスク空き容量、再起動後の復旧を測定。

これらはローカルのmockや独自CAでは代替できません。今回の結果は「2GB×3台で初回構築からHibanaの基本動作まで到達できる」ことを示します。
