# 独立Ubuntu VMでIaCを検証する

Mac（Apple Silicon）・Lima 2.2以降の例です。VMは合計6GiBを使います。ホストと他のプロジェクトの使用量を確認してから起動します。既存のHibana/kind環境とは別の3台で、各2CPU・2GiB、Ubuntu 24.04です。ARMの実測値はx86 VPSの処理性能を保証しません。

```sh
brew install lima
limactl network create hibana-iac --gateway 192.168.106.1/24
limactl start --name=hibana-iac-cp --vm-type=vz --network=lima:hibana-iac --memory=2 --cpus=2 --disk=30 --mount-none --containerd=none --tty=false template:ubuntu-24.04
limactl start --name=hibana-iac-management --vm-type=vz --network=lima:hibana-iac --memory=2 --cpus=2 --disk=40 --mount-none --containerd=none --tty=false template:ubuntu-24.04
limactl start --name=hibana-iac-execution --vm-type=vz --network=lima:hibana-iac --memory=2 --cpus=2 --disk=50 --mount-none --containerd=none --tty=false template:ubuntu-24.04
.local/iac-venv/bin/python infra/lab/inventory.py
.local/iac-venv/bin/python infra/lab/prepare.py
```

`user-v2`ネットワークでVM間通信を提供します。Macの標準vzNATはホストからVMへ接続できても、このホストではVM間通信が分離されたため使用しません。新しいVMへ同じAnsibleを適用します。

inventoryスクリプトはLima経由で取得した実VMのホスト鍵を専用known_hostsへ固定します。テスト用ドメインは`hibana.iac.test`で、名前解決の変更は3台のVM内だけです。独自CAはVM内のアプリと検証コマンドだけに渡します。Macのhosts・キーチェーン・公開DNSを変更しません。Cloudflare APIやLet's Encryptにもアクセスしません。

```sh
ANSIBLE_CONFIG=infra/ansible/ansible.cfg .local/iac-venv/bin/ansible-playbook \
  -i .local/iac/inventory.yml infra/ansible/bootstrap.yml
ANSIBLE_CONFIG=infra/ansible/ansible.cfg .local/iac-venv/bin/ansible-playbook \
  -i .local/iac/inventory.yml infra/ansible/deploy.yml \
  -e @.local/iac/site.yml -e @.local/iac/vault.yml
```

labのVault入力は一時的な検証資格情報だけを含む0600の平文ファイルです。本番の秘密値を流用しません。CAの有効期間は7日です。期限切れの検証環境は、必要なデータがないことを確認して作り直してください。

専用VM内のkubeconfigで確認します。普段のkubectl contextは変更しません。

```sh
limactl shell hibana-iac-cp sudo kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes
limactl shell hibana-iac-cp sudo kubectl --kubeconfig=/etc/kubernetes/admin.conf get pods -A
limactl shell hibana-iac-cp sudo curl --cacert /opt/hibana/rendered/site/ca.crt https://hibana.iac.test/api/readyz
```

再実行前後のNode/PVC/Pod UID、DB・realmのユーザーID、Secretの値のハッシュを比較します。鍵・DB・PVを再生成せず、設定が変わらないPodを置換しないことを確認します。TerraformのmockテストとローカルTLS検証は、実Cloudflare DNS伝播やLet's Encrypt本番発行を検証したことにはなりません。

終了時はデータを残してVMだけ停止できます。

```sh
for vm in hibana-iac-cp hibana-iac-management hibana-iac-execution; do
  limactl stop "$vm"
done
```

削除はこの検証環境が不要になった時だけ、上記3台を名前指定して行ってください。稼働中の他のLima/Docker環境を一括停止・削除しません。
