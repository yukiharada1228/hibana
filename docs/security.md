# Hibana のセキュリティ境界

Hibana の MVP は、組織が管理するオンプレ Kubernetes 上で、Hono・TypeScript/JavaScript・Go・Rust の Web API を Wasm として実行する基盤です。アプリ開発者は `hibana init → dev → build → deploy` を使用し、Kubernetes 管理権限や kubeconfig を必要としません。Hono 専用アダプターも不要です。

「完全に安全」という保証はしません。達成条件は、脅威・防御・試験・未検証項目を追跡でき、既知の問題を修正して運用を継続できることです。機能数を減らしても、この受入条件は削りません。

## 信頼するものと保護するもの

- 信頼する主体：基盤管理者、Kubernetes 管理者、ホスト OS、Control Plane / Worker の実装、ビルド・配布経路。
- 信頼しない入力：HTTP リクエスト、アップロードされた Component、ゲストからのホスト呼び出し。認証済みのテナントでも他テナントへのアクセスは許可しません。
- 保護対象：他の実行のメモリと Secrets、テナントの設定・成果物、基盤の認証情報、内部ネットワーク、共有資源の可用性。
- `hibana build` は開発者の端末でツールチェーン・プロジェクトのビルドコマンドを実行します。このビルド工程は Wasm サンドボックスではありません。第三者のプロジェクトや依存パッケージをビルドする CI は、別の隔離されたビルド環境が必要です。

## MVP で維持する防御

| 境界 | 実装・受入条件 |
|---|---|
| CLI → 管理 API | loopback 以外は HTTPS 必須。リダイレクトを拒否。保存したトークンは別のサーバーへ再利用しない。プロファイルはユーザー設定領域にディレクトリ0700・ファイル0600で保存。旧 `.hibana/auth.json` は互換用に読む。Secrets は標準入力で渡す |
| 管理 API | 認証・スコープ確認と tenant-scoped トランザクション / FORCE RLS。マイグレーション権限を通常プロセスに渡さない |
| 配備成果物 | HTTP Component の形式・契約と import を検証。検証子プロセスに時間・Linux メモリ制限。Worker は Wasm の SHA-256 を照合する |
| 内部実行 | 署名トークン、期限、テナント・実行・バージョンの照合。公開アプリ API と管理 API と内部 API を分離。HTTP を自動再実行しない |
| ゲスト | 実行ごとの Store / WASI context。ホストの環境・ファイルを継承しない。vars / Secrets は承認したキーだけ注入し、ゲスト stderr を共有ログへ出さない |
| 資源 | 実行時間・fuel・同時実行数・合計線形メモリ量を制限。Wasm threads をビルドから除外。WASI リソース数 4096、テーブル要素は実行合計 100,000、core instance / memory / table 数は各 256、乱数取得は1回1 MiBまで |
| 通信 | ゲスト外向き通信は既定拒否。承認時も内部 IP を拒否して接続 IP を固定し、プロキシ環境・リダイレクトで迂回させない。クラスタ側でも NetworkPolicy を強制する |
| 共有制限 | Redis を確認できなければ受付を 503 で拒否。DB の受付記録でもテナント同時実行上限を保証する |
| Kubernetes | 非 root、read-only rootfs、権限昇格禁止、capabilities 全削除、seccomp、ServiceAccount トークン自動マウント禁止。管理者が CNI・TLS・ノード構成を検証する |

`memory_mb` は実行内の線形メモリ合計です。ホストの HTTP バッファ、コンパイル、コードキャッシュ、JIT 管理領域を含む Pod の RSS 全体の上限ではありません。Pod の cgroup 制限と同時実行数も必要です。割当失敗時のメモリ計量は保守的な上限値となる場合があります。

WorkerはPodごとに承認済み線形メモリ上限の合計を予約し、空きがなければ実行開始前に拒否します。同時コンパイル・成果物ダウンロード量にも制限を設けています。予約の解放と実行開始はDBの条件付き更新で競合を解決し、実行した可能性のあるリクエストを再送しません。具体的な予算と残る限界は[スケールと過負荷制御](scaling.md)を参照してください。

## 依存関係を継続的に検査する

Wasmtime はサポート中の LTS 36.0.14 以上を使用し、実際の解決版は Cargo.lock に固定します。不要な Preview 1、threads、profiling、WASI HTTP の既定送信クライアントをビルドから外しています。Hibana の送信処理は独自の許可確認と reqwest を使用します。

`Security dependencies` CI は push / PR / 毎日 / 手動実行で RustSec と npm を検査します。Rust の既知脆弱性と unsound 警告、npm の low 以上の検出を失敗にします。アドバイザリの取得失敗も成功扱いにしません。リポジトリ管理者はブランチ保護の必須チェックに指定し、通知先と修正担当者を設定してください。

```bash
cargo install cargo-audit --version 0.22.2 --locked
bash scripts/check-security.sh
npm audit --prefix sdk --package-lock-only --audit-level=low
```

唯一の例外は `RUSTSEC-2023-0071` です。SQLx の未使用 MySQL 経由の `rsa` が lockfile に残りますが、Hibana は PostgreSQL のみ使用します。検査スクリプトは **全ターゲットの実際の依存グラフに rsa が存在しないことを確認してから** この1件だけを除外します。将来その経路が有効になれば検査は失敗します。この例外を「Cargo.lock 全体で検出ゼロ」と表現しません。

2026-09-07の検査では、`spin` 0.9.8 / 0.10.0 に yanked 警告も残ります。multer / crc-fast の推移依存です。既知脆弱性・unsoundとは区別して表示を維持し、上流更新時に再確認します。警告を削除するための独自の依存パッチは導入していません。

## 本番公開前に残る受入条件

1. 実 CNI 上で管理・アプリ・内部口の分離、他 namespace / 内部ネットワーク / メタデータ IP への到達拒否を実測する。TLS は外部だけでなく Secrets が流れる内部通信と DB / Redis / S3 まで設計する。
2. 不正 Component、無限ループ、メモリ / ハンドル大量生成、遅い HTTP、同時 cold start を負荷試験し、正常アプリへの影響を計測する。Fleetのコンパイルは資源制限付きの子プロセスへ分離し、キャッシュにも容量上限を設けた。ただし同じUID・Podであり、ネイティブコンパイラの侵害に対する独立したセキュリティ境界ではない。[実装と試験手順](resilience.md)を参照。
3. 本番イメージの digest 固定、OS を含む脆弱性検査、署名・出所確認、更新手順、鍵ローテーション、バックアップからの Secrets を含む復元試験を行う。依存監査だけでは全経路の安全性を証明できない。
4. [デプロイ仕様](deployment.md)に従って新CLI・CP・WorkerとDBを揃えて更新し、実環境でも失敗時の設定維持とrollbackを確認する。ローカル統合テストでは版ごとのvars・選択したSecretの参照と、active版の原子的な公開を検証する。
5. 相互に敵対する第三者テナントを収容する前に追加の隔離設計と第三者レビューを行う。現在は複数テナントを同じ Worker プロセスで実行し、Worker は共有 DB の接続権限を持つ。`hardened` の VM RuntimeClass は Worker Pod 単位であり、同じ Pod 内のテナントごとの VM 分離ではない。ランタイム脱出時の影響をテナント単位に限定するには、実行プロセス・資格情報・ネットワークもその単位で分離する必要がある。

通常の配備・rollbackはRead・Deployスコープで実行します。Secretを保存しただけでは新しい版へ注入できません。管理者の`allow-deploy`と、配備する`hibana.json`の`secrets`指定を両方必要とします。この許可は「そのテナントのDeploy権限を持つ開発者が、そのアプリの将来のコードからSecretを読める」という委任です。アプリ別のトークンスコープやコードレビュー承認を実装したものではありません。`deny-deploy`は既存の版の承認を取り消さず、Secretsの削除は稼働中の版を含む注入を停止します。削除前に取得済みの値は回収できないため、漏えい時には提供元の資格情報も失効させます。

アプリに渡した Secrets は、そのアプリ自身が HTTP 応答や許可された外向き通信で外へ出せます。アプリの認証不備・SQL injection・意図したデータ公開は、Wasm のメモリ隔離では防げません。アプリ側と基盤側の責任を分けてレビューします。

## 根拠

- [Wasmtime のセキュリティモデル](https://docs.wasmtime.dev/security.html)：import ベースの隔離と多層防御。
- [Wasmtime のサポート期間](https://docs.wasmtime.dev/stability-release.html)：LTS とセキュリティ修正の提供方針。
- [WASI ホスト資源枯渇のアドバイザリ](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-852m-cvvp-9p4w)：旧 29.0.1 を継続利用しない理由。
- [Kubernetes Security Checklist](https://kubernetes.io/docs/concepts/security/security-checklist/)：クラスタ・Pod・Secret・ネットワークの受入条件。
