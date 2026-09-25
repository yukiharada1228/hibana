# 本番の継続的デリバリー

`develop`へのpushでCI・Security dependencies・VPS IaCを実行します。公開するコミットに`vVERSION`タグを付けると、4種類のランタイムと2種類のCPU用コンテナ、CLI、Kubernetesマニフェストをビルドします。同じコミットの3つのチェックが成功してからGitHub Releaseとnpmへ公開し、実際のnpm導入を確認します。最後に`production` environmentを使うジョブがReleaseへ`production.json`を追加します。このファイルが本番更新の許可です。通常のbranch/PRビルドでは発行しません。タグ・配布物・マーカーを上書きしません。

KAGOYAのcontrol-planeでsystemd timerが5分おき（最大30秒の追加揺らぎ）に公開GitHub Releaseを確認します。マーカーのrepository・tag・commit、タグの実体、ソースの版、現在稼働するコミットからの前進を検査し、既存のAnsibleを実行します。SSHやKubernetes APIの公開範囲を増やさず、GitHubに本番SSH鍵・Vault鍵・DB認証情報を保存しません。常駐するActions runnerも追加しません。1回の実行をファイルロックで直列化します。

更新の順序は次のとおりです。

1. HibanaとKeycloakのDBをオンラインdumpし、`pg_restore --list`で検査。設定・Secretsと合わせて既存Vault鍵で暗号化し、復号一致を確認。
2. 新しい版のチェックサム付きコンテナを両Workerノードへ取得・import。
3. `platform install`で受付停止・実行完了待ち・DBマイグレーション・Control Plane/Worker/Console更新・受付再開。
4. 3つのDeploymentのイメージとrollout、3ノードのReady、Console/API/OIDCと指定アプリのHTTP 200を検査。
5. 成功した版とバックアップの場所を保存。成功後に限り、古い暗号化DB・設定バックアップを削除して直近3世代を保持。

このバックアップはDB・設定の更新前スナップショットです。Garageのアプリソースを含む災害復旧用バックアップは別途必要です。現在の単一Control Plane・単一Worker構成では、更新中に停止があります。マーカー発行ジョブの成功は「デプロイ許可」であり、本番反映の成功を意味しません。本番の結果はVPS上の状態と公開疎通で確認します。

## 初回設定

`infra/ansible/cd.yml`を、既存の管理用inventoryを使って管理者PCから適用します。必要なextra varsは次のファイルの絶対パスです。すべてGit外に保存します。

| 変数 | 内容 |
| --- | --- |
| `cd_inventory_file` | CP自身は`ansible_connection: local`、2ノードはprivate IP。鍵とknown_hostsは`/opt/hibana/cd/`を指定 |
| `cd_site_file` | 既存site.yml。`hibana.cd_smoke_urls`に常設の公開アプリURLを1つ以上指定 |
| `cd_vault_file` / `cd_vault_key_file` | 既存の暗号化Vaultと復号鍵 |
| `cd_key_file` | CD専用SSH秘密鍵。隣に`.pub`を置く。公開鍵はCPのprivate IPからだけ許可 |
| `cd_known_hosts_file` | 管理者が照合済みの2ノードのprivate IPとSSHホスト公開鍵 |
| `cd_initial_state` | 現在配備済みの`tag`・40桁`commit`・GitHub Releaseの`published_at` |

`production` environmentのdeployment branch policyは`v*`タグと手動再開用の`develop`に限定します。ソース変更のレビュー権限・タグ発行権限・GitHub Releaseの編集権限を持つ人は本番コードを変更できるため、これらを運用者に限定します。マーカーとSHA256SUMSはGitHubの同じ権限境界内の検証で、独立した署名ではありません。

```bash
ANSIBLE_CONFIG=infra/ansible/ansible.cfg ansible-playbook \
  -i .local/vps/inventory.yml infra/ansible/cd.yml -e @.local/vps/cd/setup.yml
```

## 状態確認・停止・失敗時

```bash
sudo systemctl list-timers hibana-cd.timer
sudo journalctl -u hibana-cd.service --no-pager -n 30
sudo cat /opt/hibana/cd/state.json
# 詳細ログには設定情報が含まれ得るため、公開Issue/Actionsへ転記しない
sudo less /opt/hibana/cd/deploy.log
sudo systemctl stop hibana-cd.timer
```

バックアップ・適用・疎通のどれかが失敗すると、`state.json`に`attempt`を残し、以後の自動適用を停止します。デプロイ済み版が部分的に変わっている可能性があるため、旧イメージへの自動復帰やDBのdown migrationは行いません。`/opt/hibana/install.log`も確認し、原因を修正してから再試行します。

```bash
sudo /opt/hibana/cd/venv/bin/python /opt/hibana/cd/cd.py --retry
sudo systemctl start hibana-cd.timer
```

API取得・タグ照合など適用開始前の一時失敗では本番を変更せず、次のtimerで再確認します。失敗した版を修正する場合は新しい版として公開してください。controllerや秘密値を更新するときは管理者PCから`cd.yml`を再適用します。controller自身はRelease内のコードで自己更新しません。
