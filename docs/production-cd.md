# 本番の継続的デリバリー

GitHub Actionsが本番更新を開始し、完了・失敗まで確認します。VPSの定期確認は行いません。

`develop`へのpushでCI・Security dependencies・VPS IaCを実行します。`vVERSION`タグでは配布物をビルドし、同じコミットの3つのチェックが成功してからGitHub Releaseとnpmへ公開します。ビルドが先に終わった場合はCIを最大30分待ちます。公開npmの実際のインストールを確認し、`production.json`マーカーを追加してから、`Deploy production VPS`ジョブが本番に接続します。

```text
GitHub Actions: CI → Release・npm公開 → VPSへ更新指示 → 完了・公開疎通を確認
VPS:                            バックアップ → Ansible → DB更新・配備・検査
```

## 接続と権限

control-planeのTCP 2222にデプロイ専用SSHリスナーを置きます。公開鍵認証だけを使い、`hibana-deploy`という専用アカウントで、`deploy <tag> <commit>`だけを受け付けます。任意のシェル、SFTP、ポート転送、rootログインを拒否します。管理用SSHの22番は従来の接続元IP制限を保持します。Kubernetes APIを新たに公開しません。

GitHubの`production` environmentには専用SSH秘密鍵1個を保存します。SSHホスト鍵は管理用接続で照合してから固定し、Actions実行時に`ssh-keyscan`で取り直しません。DB・Keycloak・ストレージ・Vaultの鍵や、一般管理用SSH鍵はGitHubへ渡しません。

VPS側でも公開マーカーのrepository・tag・commit、Gitタグの実体、ソースの版、配備済みコミットからの前進を検査します。古い・別系統の版への自動巻き戻しは拒否します。SSHの入力はroot所有のプログラムで検査してから固定のデプロイ処理へ渡します。

ActionsのconcurrencyとVPSのファイルロックで更新を直列化します。処理は一時的なsystemdサービス`hibana-deploy`として動き、Actions/SSHが切れても途中で強制停止しません。Actionsはサービスの終了コードを待ち、成功したtag・commitを照合してから公開URLを確認します。接続が切れた場合はActionsが失敗になるため、VPSの状態を確認してから再実行します。

## 更新の順序

1. HibanaとKeycloakのDBをオンラインdumpし、`pg_restore --list`で検査。設定・Secretsと合わせてVault鍵で暗号化し、復号一致を確認。
2. チェックサム付きコンテナを両Workerノードへ取得・import。
3. `platform install`で受付停止・実行完了待ち・DBマイグレーション・Control Plane/Worker/Console更新・受付再開。
4. 3つのDeploymentのイメージとrollout、3ノードReady、Console/API/OIDCと指定アプリのHTTP 200を検査。
5. 成功した版とバックアップの場所を保存。成功後に限り、暗号化DB・設定バックアップを直近3世代へ整理。
6. Actions側からもAPIと公開helloへアクセスし、成功した場合だけproduction deploymentを成功にする。

同じ版を手動で指定した場合も更新手順を実行します。単一Control Plane・単一Worker構成では更新中に停止があります。バックアップはDB・設定用で、Garageを含む災害復旧用バックアップとは別です。

## 初回設定

`infra/ansible/cd.yml`を管理者PCから適用します。既存の定期実行があれば停止・削除します。以下のファイルはGit外に保存します。

VPS事業者側にもフィルターがある場合は、control-planeだけにTCP 2222を許可します。管理用22番の接続元制限と他ノードの設定は維持します。KAGOYA側のセキュリティグループ作成・割当は管理画面で行い、ホスト内のSSH・sudo・firewall・controller設定はAnsibleで管理します。

| 変数 | 内容 |
| --- | --- |
| `cd_inventory_file` | CP自身は`ansible_connection: local`、2ノードはprivate IP。鍵とknown_hostsは`/opt/hibana/cd/`を指定 |
| `cd_site_file` | 既存site.yml。`hibana.cd_smoke_urls`に常設公開アプリURLを指定 |
| `cd_vault_file` / `cd_vault_key_file` | 暗号化Vaultと復号鍵 |
| `cd_key_file` | CPからWorkerノードへ接続する専用秘密鍵。隣に`.pub`を置く。CPのprivate IPからだけ許可 |
| `cd_known_hosts_file` | 照合済みの2ノードのprivate IPとSSHホスト公開鍵 |
| `cd_actions_public_key_file` | Actions接続用の別の公開鍵。秘密鍵はGitHub environmentへ登録 |
| `cd_actions_ssh_port` | inventoryの共通変数に`2222`を指定。通常のhost firewall再適用でも接続口を維持 |
| `cd_initial_state` | 配備済みの`tag`・40桁`commit`・Releaseの`published_at`。既存stateは上書きしない |

```bash
ANSIBLE_CONFIG=infra/ansible/ansible.cfg ansible-playbook \
  -i .local/vps/inventory.yml infra/ansible/cd.yml -e @.local/vps/cd/setup.yml
```

GitHubの`production` environmentは`v*`タグと手動実行用の`develop`だけを許可します。

| 種類 | 名前 | 内容 |
| --- | --- | --- |
| Secret | `PRODUCTION_SSH_KEY` | Actions専用SSH秘密鍵 |
| Variable | `PRODUCTION_SSH_HOST` | control-planeの公開IP |
| Variable | `PRODUCTION_SSH_PORT` | `2222` |
| Variable | `PRODUCTION_SSH_KNOWN_HOSTS` | 照合済みの`[IP]:2222 ssh-ed25519 …`行 |

ソース変更・タグ発行・GitHub Release編集の権限は本番コードを変更できる権限です。マーカーとSHA256SUMSはGitHubの同じ権限境界内の検証で、独立した署名ではありません。

## 手動再配備・失敗時

通常はReleaseの最後に自動実行します。既存の検証済み版を再配備するときはActionsの`Deploy production`を使います。

```bash
gh workflow run deploy-production.yml --ref develop -f release_tag=v0.2.0-rc.13
```

バックアップ・適用・疎通のどれかが失敗すると、`state.json`に`attempt`を残します。旧イメージへの自動復帰やDBのdown migrationは行いません。秘密値を含み得る詳細ログはVPSに保持し、Actionsへ転送しません。

```bash
sudo cat /opt/hibana/cd/state.json
sudo journalctl -u hibana-deploy --no-pager -n 30
sudo less /opt/hibana/cd/deploy.log
sudo less /opt/hibana/install.log
```

未完了の`attempt`がある場合は原因を修正してから、管理者が指定版を明示して再試行します。Actions専用鍵から`--retry`は指定できません。

```bash
sudo /opt/hibana/cd/venv/bin/python /opt/hibana/cd/cd.py \
  --tag vVERSION --commit EXACT_COMMIT_SHA --retry
```

controllerや秘密値の更新は管理者PCから`cd.yml`を再適用します。controller自身をReleaseから自己更新しません。稼働検証は[本番CDの検証記録](production-cd-validation.md)に記載します。
