# Hibanaの配布と導入

CLI は npm の `@yukiharada1228/hibana` と [GitHub Releases](https://github.com/yukiharada1228/hibana/releases) の tarball で配布します。PC用Wasmtimeランタイム、オンプレ用コンテナイメージとKubernetesマニフェストはGitHub Releasesで別々に配布します。初回作成はバージョンを指定した`npx`、作成後のHono・JavaScriptプロジェクトはローカルCLIをnpm scriptsから使用します。グローバルインストールは任意です。

MVPではCLI・ローカルランタイム・Control Plane・Workerを同じバージョンに揃えます。異なる版の混在は検証対象外です。

現在のソース候補は`0.2.0-rc.5`です。以下のnpxの例は、その版のnpm公開後に使用できます。公開前は[候補版の導入手順](release-candidate.md)でtarballから取得・検証します。

## 開発者のPC

Node.js 24以上を用意し、公開されているバージョンを指定します。

```bash
npx --yes @yukiharada1228/hibana@0.2.0-rc.5 --version
npx --yes @yukiharada1228/hibana@0.2.0-rc.5 init hello
cd hello
npm run dev
# Ctrl+Cで停止
```

CLIの導入にリポジトリ、Rust、Docker、kubectlは不要です。`init`が作るHono・JavaScriptプロジェクトは、作成時のCLIバージョンを`devDependencies`に固定します。npm scriptsは`hibana dev`・`hibana build`・`hibana deploy`としてローカルCLIを実行します。`package.json`と`package-lock.json`をGitに保存し、別のPCやCIでは`npm ci`で開発依存も導入します。

以降の`hibana ...`は、CLIを導入済みのプロジェクト内では`npm exec -- hibana ...`として実行できます。グローバル導入済みなら直接実行でき、プロジェクト作成前やRust・Goでは`npx --yes @yukiharada1228/hibana@VERSION ...`も使えます。[グローバル導入と既存プロジェクトの移行](../sdk/README.md#cliの導入とテンプレート)も参照してください。

`dev`はランタイムが見つからなければ、CLIと同じバージョンのOS・CPUに合うファイルをHTTPSで取得し、Releaseの`SHA256SUMS`と照合してから保存します。次回以降は保存済みのランタイムを再利用します。`hibana runtime install`で事前に取得することもできます。

管理APIへのログインと配備は[リモートCLI手順](remote-cli.md)を参照してください。リモート配備だけを行うPCにはローカルランタイムは不要です。

## 閉域環境への搬入

Releaseの`hibana-cli-VERSION.tgz`、OSに合う`hibana-worker-VERSION-OS-ARCH`、`SHA256SUMS`を搬入します。HASHには該当バイナリのチェックサムを指定します。

```bash
npx --yes --package=/path/to/hibana-cli-0.2.0-rc.5.tgz hibana runtime install --from /path/to/hibana-worker-0.2.0-rc.5-linux-x64 --sha256 HASH
npx --yes --package=/path/to/hibana-cli-0.2.0-rc.5.tgz hibana init hello --cli-package /path/to/hibana-cli-0.2.0-rc.5.tgz
cd hello
npm run dev
```

事前導入したランタイムは通常の`hibana dev`で再利用します。外部通信ができない環境では、搬入時にCLIとランタイムのバージョンを揃えてください。JavaScriptコンパイラーやHonoなどのnpm依存も、社内ミラーまたはキャッシュに用意します。チェックサムは破損検出に使用します。同じReleaseに置くハッシュは、配布元と独立した署名ではありません。

## 基盤管理者

CPUに合う`hibana-platform-VERSION-linux-ARCH.tar`をDockerへ読み込み、社内レジストリへ搬入します。`registry.example.com`は自社の宛先へ置き換えます。

```bash
docker load --input hibana-platform-0.2.0-rc.5-linux-amd64.tar
docker tag hibana-platform:0.2.0-rc.5-linux-amd64 registry.example.com/hibana/platform:0.2.0-rc.5-amd64
docker push registry.example.com/hibana/platform:0.2.0-rc.5-amd64
tar -xzf hibana-kubernetes-0.2.0-rc.5.tar.gz
```

`hibana platform init my-site`でサイト用overlayを生成し、同梱のREADMEに沿ってAPIとアプリのDNS・TLS、DB・Redis・S3の接続情報を設定します。署名・暗号化・bootstrap用のキーは生成時に作成し、秘密値のファイルはGitから除外します。

```bash
hibana platform install --kubeconfig FILE --context NAME --overlay PATH --image registry.example.com/hibana/platform@sha256:DIGEST
hibana platform stop --kubeconfig FILE --context NAME
hibana platform start --kubeconfig FILE --context NAME
hibana platform uninstall --kubeconfig FILE --context NAME --yes
```

停止は新規受付を閉じ、処理と実行結果の保存が終わってからWorker・CPを停止します。再開は保存したレプリカ数を復元し、公開アプリの準備後にHPAと受付を戻します。撤去はCLIが管理するリソースを削除し、クラスタ・namespace・PVC・外部DB/Redis/S3を保持します。実サイトでの導入条件は[オンプレ運用ガイド](on-prem-production.md)を参照してください。

このソース候補では、停止制御・回収・キャッシュ保護に必要なテーブルもSeaORMの新しい初期スキーマへ含めています。CLIと基盤イメージを同じ候補から用意し、[空DBへの初期化](database.md)を先に完了させます。公開済みv0.1.0のDBへの上書き更新は行いません。

## ソースから候補を作る

```bash
npm ci --prefix sdk
cargo build --locked --release -p hibana-worker
HIBANA_RUNTIME_BIN="$PWD/target/release/hibana-worker" npm run test:package --prefix sdk
mkdir -p .local/release
node scripts/release.mjs cli .local/release
node scripts/release.mjs runtime target/release/hibana-worker .local/release
node scripts/release.mjs platform .local/release
node scripts/release.mjs checksums .local/release
```

ローカルで作るランタイムはそのPCのOS・CPU向けです。バージョンは`Cargo.toml`、`sdk/package.json`、`sdk/package-lock.json`で一致を検査します。

## GitHub Releaseとnpmパッケージを公開する

`.github/workflows/release.yml`（Hibana release）は`release/**`ブランチまたは手動実行で候補をビルドします。`vVERSION`タグでは、同じコミットのCIとSecurity dependenciesの最新実行が両方成功したことを確認してからGitHub Releaseを公開・検証し、同じCLI tarballをnpmへ公開します。未実行・実行中・失敗の場合は公開を止めます。正式版は`latest`、候補版は`next`タグを使います。ローカルランタイムの自動取得先が先に利用可能になる順序です。

公開担当者は npm の `@yukiharada1228` スコープへの公開権限を用意します。初回は同じ版のランタイムをGitHub Releaseに公開した上で、`npm login` 後に検証済みの tarball を `npm publish ./hibana-cli-0.2.0-rc.5.tgz --access public --tag next` で公開し、以後は [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/) を設定します。npm のパッケージ設定に GitHub owner `yukiharada1228`、repository `hibana`、workflow filename `release.yml` を登録し、`npm publish` を許可します。CI は Node.js 24 と npm 11.5.1 以上を使い、公開ジョブだけに `id-token: write` を付与します。長期間有効なnpmトークンは保存しません。

公開後は `npm view @yukiharada1228/hibana@VERSION version` と `npx --yes @yukiharada1228/hibana@VERSION --version` で確認します。npm は公開済みの同じバージョンを上書きできないため、変更した候補は新しいバージョン番号で配布します。コンソールも同じ版を取得できることを確認してから更新してください。

npm は[公開時の自動検査](https://github.blog/changelog/2026-07-28-npm-publish-time-malware-scanning-and-dual-use-metadata/)を行うため、公開成功後も通常約5分、混雑時などは15分以上インストールできない場合があります。CI はパッケージを取得できるまで20秒間隔で最大45回確認してから `npx` を検証します。待機が時間切れになった場合は公開状態を確認し、同じ版を再公開しないでください。

候補ブランチでは通常CIと依存監査も実行します。全OS/CPUの成果物を集めた`hibana-release-candidate`を14日間保存します。バージョンに`-rc.1`などの接尾辞があるタグは、GitHubのprereleaseとして公開する設定です。

- Linux x64/arm64、macOS x64/arm64をネイティブビルド。各OSでtarballの独立インストール、HonoのWasm変換、ランタイム導入、実HTTP応答、Ctrl+C停止を検証。
- Linux amd64/arm64の基盤イメージをDocker archiveで保存し、load後の起動とバージョンを確認。
- CLI、Kubernetes archive、4つのランタイム、2つの基盤イメージ、2つのコンソールイメージの全10ファイルが揃った場合だけ`SHA256SUMS`を作成。コンソールはLinux amd64 / arm64それぞれのイメージを別に配布し、サイトの`console/kustomization.yaml`へ設定する。
- タグとパッケージの版が一致した場合だけ、同じコミットの成果物をReleaseへ添付。公開後にGitHub URLからCLIを再インストールし、既定の`init → dev`によるランタイムの自動取得と`HTTP → 停止`を確認。

LinuxバイナリはUbuntu 22.04、MacはIntelがmacOS 15、arm64がmacOS 14でビルド・検証します。古いOS、Windowsネイティブ、MacのDeveloper ID署名・公証は対象外です。[GitHub公式runner一覧](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)を参照してください。

パッケージ配布の試験と[2時間の実機受入](pilot-validation.md)を分けて記録します。実オンプレのTLS・CNI・HAや、作者以外の開発者による試用の結果を、自動試験だけで合格とはしません。
