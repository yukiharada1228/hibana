# Hibana 0.2.0-rc.1

CLI・PC用Wasmtimeランタイム・Control Plane・Workerを`0.2.0-rc.1`に揃えた候補版です。npmレジストリには公開しません。GitHub Actionsで作成した候補を取得し、検証環境へ導入します。正式なReleaseを公開するまで、候補版のGitHub Release URLによる自動取得は使えません。

## 含まれる変更

- 単独CLIによるサイト設定生成、実際のKubernetes dry-run、導入段階の記録と修復。
- 受付を閉じた後の処理完了待ち、停止・再開・撤去、複数CLIの操作競合防止。
- 初回起動前のNetworkPolicy、更新中の旧Podの通信維持、最終ポリシー適用後の全Podの依存先検査。
- アップロードの中断・公開・回収の競合対策と、削除失敗時の継続的な回収。マイグレーション`0029`・`0030`を追加。
- 実行中・準備中のWasmキャッシュ保護、バージョン操作の直列化、CLIのヘルプ・エラー・開発サーバー停止の改善。

個別の検証内容は[修正・検証記録](review-fixes-validation.md)と[導入CLI検証](cli-platform-validation.md)に記載しています。

## 配布物を取得する

`release/v0.2.0-rc.1`ブランチの同じコミットに対して、CI、Security dependencies、GitHub releaseの全ジョブが成功した候補を選びます。`RUN_ID`はそのGitHub release実行のIDです。Actionsの成果物は14日間保持されます。

```bash
gh run download RUN_ID --repo yukiharada1228/hibana \
  --name hibana-release-candidate --dir .local/release/0.2.0-rc.1
cd .local/release/0.2.0-rc.1
shasum -a 256 -c SHA256SUMS
```

完全な候補は以下の8ファイルと`SHA256SUMS`です。

| 配布物 | ファイル |
| --- | --- |
| CLI | `hibana-cli-0.2.0-rc.1.tgz` |
| Kubernetesマニフェスト | `hibana-kubernetes-0.2.0-rc.1.tar.gz` |
| PC用ランタイム | `hibana-worker-0.2.0-rc.1-{darwin,linux}-{x64,arm64}`の4ファイル |
| 基盤イメージ | `hibana-platform-0.2.0-rc.1-linux-{amd64,arm64}.tar`の2ファイル |

候補の作成元はActions実行のcommit SHAで確認できます。異なる実行・バージョンのファイルを混在させず、ハッシュ確認後に社内へ搬入してください。

## 開発者のPCで試す

以下はmacOS arm64の例です。OS/CPUに応じてランタイムのファイル名を変更し、`HASH`には`SHA256SUMS`の該当値を指定します。Node.js 24以上が必要です。

```bash
# 取得したファイルがあるディレクトリで実行
npm install -g ./hibana-cli-0.2.0-rc.1.tgz
hibana --version
hibana runtime install --from ./hibana-worker-0.2.0-rc.1-darwin-arm64 --sha256 HASH
hibana init hello --cli-package "$PWD/hibana-cli-0.2.0-rc.1.tgz"
cd hello
npm run dev
# 別ターミナルで curl http://127.0.0.1:8787/
# Ctrl+Cで開発サーバーを終了
```

`--cli-package`により、生成するHonoプロジェクトも取得済み候補を使います。正式公開前はこの指定とランタイムの事前導入が必要です。Hono・JavaScriptコンパイラーの依存はnpmまたは社内ミラーから取得します。

## オンプレ検証環境へ導入する

CPUに合うDocker archiveを読み込み、社内レジストリへ搬入します。`registry.example.internal`とkubeconfig/contextを実サイトのものへ変更してください。

```bash
docker load --input hibana-platform-0.2.0-rc.1-linux-amd64.tar
docker tag hibana-platform:0.2.0-rc.1-linux-amd64 registry.example.internal/hibana/platform:0.2.0-rc.1-amd64
docker push registry.example.internal/hibana/platform:0.2.0-rc.1-amd64
hibana platform init my-site
# my-site/README.mdに沿ってDB・Redis・S3・DNS・TLSを設定
hibana platform install --kubeconfig /secure/config --context staging \
  --overlay my-site --image registry.example.internal/hibana/platform@sha256:DIGEST --dry-run
hibana platform install --kubeconfig /secure/config --context staging \
  --overlay my-site --image registry.example.internal/hibana/platform@sha256:DIGEST
```

既存環境を更新する場合は、保存済みのoverlayと鍵を使います。CLIと基盤を同じ候補から用意し、`platform install`でマイグレーション`0030`まで適用してから、新しい停止・再開コマンドを使ってください。DBマイグレーションの自動巻き戻しはありません。[バックアップ・復元手順](resilience.md)と[オンプレ導入条件](on-prem-production.md)を確認して検証環境から更新します。

基盤操作には予約済みConfigMap `hibana-platform-operation`の`get`・`create`・`update`権限が必要です。CLI強制終了後にロックが残った場合の解除条件と手順は[基盤管理ガイド](../deploy/kubernetes/README.md)に記載しています。

基盤管理者による構築後、別PCの開発者はKubernetes資格情報を使わず、[HTTPS APIへのログイン・デプロイ・削除](remote-cli.md#開発者の操作)を実施します。実オンプレでの通し検証とデモ収録は、この候補を使って行う次の段階です。
