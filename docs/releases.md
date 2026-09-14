# GitHubからHibanaを導入する

CLI、PC用Wasmtimeランタイム、オンプレ用コンテナイメージとKubernetesマニフェストを別々に配布します。配布先は[GitHub Releases](https://github.com/yukiharada1228/hibana/releases)です。`sdk/package.json`の`private: true`でnpmレジストリへの公開を禁止しています。npmはtarballとJavaScript依存のインストーラーとして使用します。

MVPではCLI・ローカルランタイム・Control Plane・Workerを同じバージョンに揃えます。異なる版の混在は検証対象外です。

現在のソース候補は`0.2.0-rc.1`です。以下のv0.1.0は公開済みReleaseの導入例です。候補版は[候補版の導入手順](release-candidate.md)から取得・検証します。

## 開発者のPC

Node.js 24以上を用意し、公開されているバージョンを指定します。

```bash
npm install -g https://github.com/yukiharada1228/hibana/releases/download/v0.1.0/hibana-cli-0.1.0.tgz
hibana --version
hibana init hello
cd hello
npm run dev
# Ctrl+Cで停止
```

CLIの導入にリポジトリ、Rust、Docker、kubectlは不要です。`init`が作るプロジェクトも、同じバージョンのGitHub Release URLを参照します。`dev`はランタイムが見つからなければ、CLIと同じバージョンのOS・CPUに合うファイルをHTTPSで取得し、Releaseの`SHA256SUMS`と照合してから保存します。次回以降は保存済みのランタイムを再利用します。`hibana runtime install`で事前に取得することもできます。

管理APIへのログインと配備は[リモートCLI手順](remote-cli.md)を参照してください。リモート配備だけを行うPCにはローカルランタイムは不要です。

## 閉域環境への搬入

Releaseの`hibana-cli-VERSION.tgz`、OSに合う`hibana-worker-VERSION-OS-ARCH`、`SHA256SUMS`を搬入します。HASHには該当バイナリのチェックサムを指定します。

```bash
npm install -g /path/to/hibana-cli-0.1.0.tgz
hibana runtime install --from /path/to/hibana-worker-0.1.0-linux-x64 --sha256 HASH
hibana init hello --cli-package /path/to/hibana-cli-0.1.0.tgz
cd hello
hibana dev
```

事前導入したランタイムは通常の`hibana dev`で再利用します。外部通信ができない環境では、搬入時にCLIとランタイムのバージョンを揃えてください。JavaScriptコンパイラーやHonoなどのnpm依存も、社内ミラーまたはキャッシュに用意します。チェックサムは破損検出に使用します。同じReleaseに置くハッシュは、配布元と独立した署名ではありません。

## 基盤管理者

CPUに合う`hibana-platform-VERSION-linux-ARCH.tar`をDockerへ読み込み、社内レジストリへ搬入します。`registry.example.com`は自社の宛先へ置き換えます。

```bash
docker load --input hibana-platform-0.1.0-linux-amd64.tar
docker tag hibana-platform:0.1.0-linux-amd64 registry.example.com/hibana/platform:0.1.0-amd64
docker push registry.example.com/hibana/platform:0.1.0-amd64
tar -xzf hibana-kubernetes-0.1.0.tar.gz
```

`hibana platform init my-site`でサイト用overlayを生成し、同梱のREADMEに沿ってAPIとアプリのDNS・TLS、DB・Redis・S3の接続情報を設定します。署名・暗号化・bootstrap用のキーは生成時に作成し、秘密値のファイルはGitから除外します。

```bash
hibana platform install --kubeconfig FILE --context NAME --overlay PATH --image registry.example.com/hibana/platform@sha256:DIGEST
hibana platform stop --kubeconfig FILE --context NAME
hibana platform start --kubeconfig FILE --context NAME
hibana platform uninstall --kubeconfig FILE --context NAME --yes
```

停止は新規受付を閉じ、処理と実行結果の保存が終わってからWorker・CPを停止します。再開は保存したレプリカ数を復元し、公開アプリの準備後にHPAと受付を戻します。撤去はCLIが管理するリソースを削除し、クラスタ・namespace・PVC・外部DB/Redis/S3を保持します。実サイトでの導入条件は[オンプレ運用ガイド](on-prem-production.md)を参照してください。

この停止制御・失敗アップロード回収・キャッシュ保護の更新は、マイグレーション`0029`・`0030`と新しいCP内アダプターを含むソース候補の変更です。CLIと基盤イメージを同じ候補から用意し、`platform install`でDB更新を先に完了させてください。公開済みv0.1.0の基盤へ、新しい停止コマンドだけを先行適用する構成は対象外です。

## ソースから候補を作る

```bash
npm ci --prefix sdk
cargo build --locked --release -p hibana-worker
HIBANA_RUNTIME_BIN="$PWD/target/release/hibana-worker" npm run test:package --prefix sdk
mkdir -p .local/release
cd sdk
npm pack --pack-destination ../.local/release
cd ..
node scripts/release.mjs runtime target/release/hibana-worker .local/release
node scripts/release.mjs platform .local/release
node scripts/release.mjs checksums .local/release
```

ローカルで作るランタイムはそのPCのOS・CPU向けです。バージョンは`Cargo.toml`、`sdk/package.json`、`sdk/package-lock.json`で一致を検査します。

## GitHub Releaseを作る

`.github/workflows/release.yml`は`release/**`ブランチまたは手動実行で候補をビルドし、`vVERSION`タグでGitHub Releaseへ公開します。npm公開ジョブ・npmトークン・OIDC権限はありません。

候補ブランチでは通常CIと依存監査も実行します。全OS/CPUの成果物を集めた`hibana-release-candidate`を14日間保存します。バージョンに`-rc.1`などの接尾辞があるタグは、GitHubのprereleaseとして公開する設定です。

- Linux x64/arm64、macOS x64/arm64をネイティブビルド。各OSでtarballの独立インストール、HonoのWasm変換、ランタイム導入、実HTTP応答、Ctrl+C停止を検証。
- Linux amd64/arm64の基盤イメージをDocker archiveで保存し、load後の起動とバージョンを確認。
- CLI、Kubernetes archive、4つのランタイム、2つの基盤イメージの全8ファイルが揃った場合だけ`SHA256SUMS`を作成。
- タグとパッケージの版が一致した場合だけ、同じコミットの成果物をReleaseへ添付。公開後にGitHub URLからCLIを再インストールし、既定の`init → dev`によるランタイムの自動取得と`HTTP → 停止`を確認。

LinuxバイナリはUbuntu 22.04、MacはIntelがmacOS 15、arm64がmacOS 14でビルド・検証します。古いOS、Windowsネイティブ、MacのDeveloper ID署名・公証は対象外です。[GitHub公式runner一覧](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)を参照してください。

パッケージ配布の試験と[2時間の実機受入](pilot-validation.md)を分けて記録します。実オンプレのTLS・CNI・HAや、作者以外の開発者による試用の結果を、自動試験だけで合格とはしません。
