# WASM FaaS Platform — M4 (運用成熟度)

[![CI](https://github.com/yukiharada1228/wasm-fass/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/yukiharada1228/wasm-fass/actions/workflows/ci.yml)

WebAssembly Component をアップロードして invoke すると、Wasmtime Worker が実行して
結果を返す FaaS プラットフォームです。本リポジトリの現状は **仕様書.md §15 の M4
（運用成熟度）範囲**であり、M1 の invoke 経路・M2 のアップロード/検証/デプロイ・M3 の
マルチテナント / 認証 / RLS / 結果出所認証 / Capability 強制 / 共有 admission ストア /
大容量 I/O 退避の上に、Prometheus メトリクス + `/readyz`、`execution_id` を相関 ID とする
構造化ログ、`.failed` (DLQ) subscriber と OS スレッドベースの epoch ticker による完全な
リトライ/タイムアウト処理、`max_fuel` / `max_execution_time` を含む全リソース制限、
テナント別クォータ上書き（`tenants.quotas` JSONB）と `Retry-After` 付き 429 バックプレッシャを
追加します。

```
                        ┌─ POST /components/{id}/versions (multipart) ─┐
                        │   検証(wasmparser + import 許可リスト)        │
Client ──HTTP──▶ control-plane ──put──▶ MinIO (Object Storage, S3 互換)
                      │  ▲                       │
                      │  │ presigned GET URL     │ presigned GET URL
                      │  │ (短命 read-only)       ▼
                      │  └─ NATS(result) ◀── worker ──▶ HTTP GET 本体取得
                      │                           │       │
                      └─ NATS(invoke) ───────────▶│       ▼
                                                  │   wasm_sha256 をキーに
                      PostgreSQL                  │   Component キャッシュ +
                  (components / versions /        │   事前コンパイル(cwasm)
                   executions の状態遷移)          ▼   → coldstart 短縮 (§3.6)
                                              Wasmtime ──▶ Component(echo)
```

- `control-plane` (axum): REST API。認証・認可、Component / version のライフサイクル管理、invoke。
  - 認証 / 認可: `POST /auth/login`（`tenant_slug` + email + password、argon2id）で API トークンを
    発行し、各 API は `Authorization: Bearer` + 必要スコープ（read/invoke/deploy/admin）を強制（M3a, §3.7）。
  - テナント分離: 全テナント表に FORCE RLS + `SET LOCAL app.tenant_id` でリクエスト境界に
    分離を強制（M3b, §3.2）。
  - `POST /components/{id}/versions` で wasm 本体を multipart 受信 →
    サイズ上限・**wasmparser による検証**・**admin 承認集合との import 厳密照合**（Capability
    deny-all, M3d §4.4）・sha256 算出を行い、通過したものだけ **MinIO** へ保存し version を
    `active` 化する（§6.2）。
  - `POST /invoke` を受けると active version を解決し、本体への**短命 presigned GET URL**
    （既定 TTL 300 秒・read-only）と、**Ed25519 で署名したジョブトークン**（claim: execution_id /
    tenant_id / version_id / kid / iat / exp, M3c §3.3）を JobMessage に同梱し invoke subject へ publish。
    `Idempotency-Key` 提示時は 3 層冪等（M3c §6.6）で重複実行を防ぐ。受付は **Redis 共有 admission
    ストア**で `invoke_rate` / `max_concurrent_executions` を原子チェック（M3d §8）。
  - 大入力は `POST /uploads` で `execution_id` を予約 → single-key 限定 presigned PUT URL → invoke の
    `input_ref` 完全一致検証で退避（M3d §3.4 / §5.2）。
  - result subject を購読し、ジョブトークンを kid で署名検証 → claim と DB / subject 整合を
    確認 → CAS で executions を終端状態へ更新。検証失敗は drop + `audit_logs`（append-only）に記録（M3c §3.3 / §3.7）。
- `worker`: NATS JetStream の invoke subject を共有 Pull Consumer（durable `workers`）で購読。
  JobMessage の `wasm_url`（presigned GET URL）から本体を取得し、`wasm_sha256` をキーに
  **Component をキャッシュ + 事前コンパイル**する（§3.6）。cwasm は `WASM_CACHE_DIR/{sha256}.cwasm`
  として永続化し、2 回目以降は cache hit で deserialize するだけになり coldstart が短縮される。
  Worker は**署名検証鍵を保持せず**、`job_token` を全 result に **verbatim に echo** するだけ
  （§3.3 MUST NOT）。
- `components/echo`: 入力をそのまま返す最小の Component（`wasm32-wasip2`）。アップロード対象の
  ローカル成果物。

---

## M4 スコープ（と非スコープ）

仕様書.md §15 M4 に準拠。M4 は a〜d の 4 スライスで段階的に実装しました。

含む（M4a: Observability + correlation ID, §3.8）:
- Prometheus `/metrics` を control-plane と worker に独立公開
  （control-plane: `faas_executions_total{status}` / `faas_execution_duration_seconds`
  / `faas_tenant_invoke_total{tenant_id}` / `faas_admission_rejections_total{kind}` /
  `faas_reaper_swept_total` / `faas_reaper_tenants_last`、worker:
  `wasmtime_execution_duration_seconds{outcome}` / `wasmtime_component_cache_hits_total{tier}` /
  `wasmtime_component_cache_misses_total` / `executions_total{outcome}`）
- `/readyz`（readiness probe）が DB `SELECT 1`、NATS `connection_state()`、共有ストア
  `Store::ping()` を順に叩き、いずれか失敗で 503 を返す。`/healthz` は liveness のまま（DB/NATS 非依存）
- `LOG_FORMAT=json` で `tracing-subscriber` の JSON formatter に切替（既定 `text` は従来挙動と完全互換）。
  invoke handler / subscriber `handle_message` / worker `handle_payload` に `#[tracing::instrument]` を
  当て、`execution_id` / `tenant_id` を全ネストログに伝播
- worker は独立 axum サーバ（`METRICS_BIND_ADDR`, 既定 `0.0.0.0:9090`）で `/metrics` + `/readyz` +
  `/healthz` を公開。JetStream pull ループが詰まっても liveness が応答できる構成

含む（M4b: 全リソース制限, §4.3）:
- `ResourceLimits` を `max_execution_time_ms`（既定 5000ms / 上限 60000ms）と `Option<u64> max_fuel`
  まで拡張。`validate()` で §4.3 上限を強制し、超過は upload 時に 422
- worker は `instantiate_async + call_handle` 全体を `tokio::time::timeout(max_execution_time)` で包み、
  host+guest 総時間の上限を強制。epoch (`Trap::Interrupt`) / tokio elapsed → `timeout`、
  `Trap::OutOfFuel` → `failed` に分類
- epoch ticker は **OS スレッド** (`std::thread::spawn`) で 50ms 周期で経過時間を確認し、
  `max_wall_time` 経過後に `engine.increment_epoch()` を繰り返し呼ぶ。tokio LIFO-slot スタベーション
  対策で、wasm guest が tight loop で tokio worker thread を塞いでも確実に発火する
  （chaos_d で確定。詳細は `crates/worker/src/main.rs` の設計メモ）
- `consume_fuel(true)` を Engine 既定で有効化し、`max_fuel` 未設定の component には `u64::MAX` を
  注入することで、決定性モード（fuel 上限）と既定モード（epoch のみ）の両立を 1 Engine で実現

含む（M4c: DLQ subscriber + reaper 二段救済, §6.6 / §3.3）:
- control-plane に `tenant.*.component.failed` (DLQ) 用の独立 subscriber を spawn。
  job_token の kid 検証 + claim と execution 行 + subject 由来テナントの突き合わせ後、
  CAS で `failed` に finalize して in-flight カウンタを DECR。`commit_finalize_and_release` を
  result / DLQ で共有し、同一の冪等性 / 監査 / メトリクス契約を強制
- worker JetStream pull consumer に `backoff` 配列（既定 `[5, 15, 60]` 秒、`BACKOFF_SECS` で
  CSV 上書き可）を追加し、`info()`-based drift detector が `ack_wait` / `max_deliver` / `backoff` /
  `max_ack_pending` のいずれかが drift していたら起動時に delete+recreate して TTL 結合を保つ
- worker は `delivered == max_deliver` の最終試行で result publish が失敗したら、自前で
  `FailedMessage`（新規 shared 型）を `.failed` subject へ publish してから ack する。
  `.failed` の publish 自体も失敗した場合は最終手段として CP の stuck-execution sweeper が回収する
  二段救済構成

含む（M4d: per-tenant quotas + 完全バックプレッシャ, §8 / §3.2）:
- `tenants.quotas` JSONB から per-tenant override を毎 invoke で解決（PK lookup、サブミリ秒）。
  `invoke_rate_per_sec` / `invoke_burst` / `max_concurrent_executions` をフィールド単位で
  グローバル既定にマージし、§8 推奨上限（500 rps / 200 concurrent）へ clamp
- 全 429 応答に `Retry-After` ヘッダを必ず付与（concurrency limit=1s、publish backpressure=5s、
  rate limit=トークンバケットの次補充時刻）。JetStream publish が `TimedOut` / `BrokenPipe` /
  `Other` で失敗したケースを 500 → 429 `publish_backpressure` に再分類
- `tenants.status = 'suspended'` を auth middleware で短絡し 403 + `tenant_suspended_denied`
  監査ログ。全 protected route に適用
- worker pull consumer に `max_ack_pending = 1000` を明示（§8 表 line 742 の MUST）。
  drift-detection 対象に含めて既存 durable も起動時に再構成

含まない（M4 以降の follow-ups）:
- HTTP middleware で `faas_http_requests_total` / `_duration_seconds` の record 配線
  （メトリクスは登録済み）
- `execution_duration_seconds` の subscriber finalize 時の observe（created_at→finished_at）
- per-tenant ラベルのカーディナリティ対策（`METRICS_INCLUDE_TENANT_LABEL=false` フラグ）
- `.result` / `.failed` の JetStream 化（現状 core NATS、CP 再起動で in-flight 結果ドロップの恐れ）
- `tenants.quotas` / `tenants.status` の admin API（現状 SQL 直 UPDATE）

> **露出ガード（仕様書 §15 MUST NOT）**: M4 完了により「Observability / 完全リトライ DLQ / 孤児
> GC / 全リソース制限 / 完全クォータ」が成立しました。仕様書 §15 M4 の **完了条件**「障害注入
> （Worker 落下・再配送・タイムアウト）で実行喪失・二重実行が起きない」は `crates/control-plane/tests/chaos_m4.rs`
> の 4 シナリオ（A: worker crash → reaper finalize、B: Idempotency-Key dedup、C: trap → DLQ finalize、
> D: tokio timeout → timeout finalize）で end-to-end 検証済み。**ただし開発用ダミー資格情報は
> 引き続き露出ガードの対象**です（`BOOTSTRAP_ADMIN_TOKEN` / `JOB_SIGNING_KEY` / MinIO root /
> Redis 未認証 / NATS 未 mTLS）。本番投入時は高エントロピー値に置換し、`JOB_SIGNING_KEY` は
> KMS / Secrets 経由で配布、NATS は mTLS、Redis は HA + AUTH 化してください（§9 / §3.3）。

---

## M3 スコープ（と非スコープ）

仕様書.md §15 M3 に準拠。M3 は a〜d の 4 スライスで段階的に実装しました。

含む（M3a: 認証・認可・RBAC, §3.7 / §6.0 / §9）:
- 管理 API: `POST /admin/tenants`（`BOOTSTRAP_ADMIN_TOKEN` で gate）でテナント + 最初の
  admin ユーザを 1 トランザクションで作成（§9 bootstrap）。`POST /tenants/{id}/users` /
  `POST /tokens` / `DELETE /tokens/{id}` を admin スコープで提供
- 認証: **API トークン**（`Authorization: Bearer ${secret}`）。`api_tokens.token_hash` に
  `sha256(secret)` を保存し、提示トークンをハッシュして照合。トークンは `POST /auth/login`
  （`tenant_slug` + email + password、パスワードは argon2id）で発行
- 認可: スコープ（read/invoke/deploy/admin）を `Principal` で確立し、ルートごとに必須スコープを
  強制。`POST /tokens` は 要求 ∩ 発行者 ∩ 対象ロール上限 を検証（権限昇格防止, §6.0 MUST）
- IDOR/越権の秘匿: 所有テナント外のリソースは `404 Not Found`（§3.7）

含む（M3b: テナント分離 + FORCE RLS, §3.2 / §3.7）:
- 全テナント表に `ENABLE ROW LEVEL SECURITY` + `FORCE ROW LEVEL SECURITY` を設定し、
  `current_setting('app.tenant_id')` 照合の RLS ポリシーを定義（§3.2 MUST）
- リクエスト処理の冒頭でトランザクション内 `SET LOCAL app.tenant_id = '<tenant>'` を実行。
  `SET LOCAL` はトランザクション境界でリセットされるため、コネクションプール再利用時に
  テナント値が残存しない
- ランタイム接続ロール（`faas_app`）は `BYPASSRLS` を持たない非特権ロール。起動時に
  `rolsuper` / `rolbypassrls` を検証し、特権ロールで接続していたら fail-fast（§3.2）

含む（M3c: 結果出所認証 + 冪等性 + 監査ログ, §3.3 / §6.6 / §3.7）:
- ジョブ署名トークン: control-plane は invoke 時に claim（`execution_id` / `tenant_id` /
  `version_id` / `kid` / `iat` / `exp`）を **Ed25519** で署名し、不透明な `job_token` を JobMessage
  に同梱。Worker は鍵を持たず、すべての result（成功 / 失敗 / タイムアウト）に `job_token` を
  **verbatim に echo** する（§3.3 MUST NOT: Worker が検証鍵を保持）。subscriber は kid で
  公開鍵を選んで検証し、claim を execution 行 + subject 由来テナントと突き合わせてから CAS 終端
- TTL 結合: `exp = iat + ACK_WAIT_SECS × MAX_DELIVER + 壁時計上限 + TOKEN_MARGIN_SECS`
  （§3.3）。`exp` 失効でも行が `pending`/`running` なら受理（正規の遅延結果の取りこぼし防止）
- 冪等性 3 層（§6.6）: `Idempotency-Key`（クライアント提示, `UNIQUE (tenant_id, idempotency_key)`）
  + `execution_id`（サーバ採番・CAS 終端） + `Nats-Msg-Id`（JetStream 重複排除）
- 監査ログ: `audit_logs` を append-only（`faas_app` は SELECT/INSERT のみ、UPDATE/DELETE なし）
  で運用。認証失敗・認可拒否・トークン発行 / 失効・テナント / ユーザ作成・署名検証失敗等を記録

含む（M3d: Capability 強制 + 共有 admission ストア + 大容量 I/O, §4.4 / §8 / §3.4 / §5.2 / §6.4）:
- Capability 強制: 既定 **deny-all**。クライアント宣言の `capabilities` は信用せず、WIT import を
  admin 承認済み集合（標準 handler world + 標準 WASI Preview2 baseline）と厳密照合。未承認の
  import は 422 拒否（§4.4 MUST）。`component_versions.capabilities` には宣言値ではなく
  「承認集合と照合して解決した import」を保存
- 共有 admission ストア（Redis）: control-plane x N で共有する `invoke_rate`（token-bucket）/
  `max_concurrent_executions`（in-flight = `pending`+`running`）/ login 失敗カウンタを Lua で原子化
  （§8 MUST）。fail-mode は login=fail-closed、invoke 系=fail-open（§8）。Redis 到達不能時は
  プロセスローカルの縮退スタブで起動継続（縮退中は警告）
- in-flight 終端化: `.result` を受信しない経路（DLQ / Worker 落下 / 無音失踪）も終端化して
  カウンタを減算する経路を結線（§6.6 MUST。完全な reaper は M4）
- 大容量 I/O: `POST /uploads` で `execution_id` を予約し、single-key 限定の短命 presigned PUT URL
  を発行。`/invoke` 時は `input_ref` が `tenants/{tenant_id}/io/{execution_id}/input` に
  **完全一致**することを検証（prefix 一致は不可, §3.4 MUST）。Worker へ同梱する read 資格も
  当該 execution_id のキーに固定

含む（M1/M2 から継続）:
- REST: `POST /components` / `POST /invoke` / `GET /executions/{id}` / `GET /healthz`
- wasm アップロード + 検証パイプライン（サイズ上限 → wasmparser 検証 → import 許可リスト →
  sha256, §6.2。別プロセスサンドボックスは TODO で M4 以降）
- Object Storage: MinIO への本体保存と Worker への presigned GET URL（§3.4）
- Worker: `wasm_sha256` キーの Component キャッシュ + 事前コンパイル（cwasm, §3.6）
- バージョン管理（§6.7）: 一覧 / soft delete / `active-version` 切替（ロールバック）
- NATS JetStream の invoke ストリーム + 共有 Pull Consumer、result 購読・永続化
- リソース制限: `max_memory`（StoreLimits）+ `max_wall_time`（epoch interruption）

含まない（M5 以降, §10 / §14）:
- **検証パイプラインの隔離プロセス化**（§6.2 MUST。現状はインプロセス）
- **大出力 offload**: DB 列・キーレイアウト（`io/{execution_id}/output`）は敷設済みだが、
  Worker 側の write offload は最小実装（後続スライスの TODO, §3.4）
- 将来⬜: 同期 Invoke、OIDC 連携ログイン、外部イベントトリガー、Cron Job / Workflow Engine、
  Secrets Manager、分散トレーシング、AI 統合、Multi Region、Result Ingestor 分離（§10 / §14）

---

## 前提ツールの導入

| ツール | 用途 |
| --- | --- |
| `rustup`（stable toolchain） | control-plane / worker / echo のビルド |
| `wasm32-wasip2` target | echo を WebAssembly Component としてビルド |
| `wasm-tools` | Component の検査（任意。本体検証は control-plane が `wasmparser` で行う） |
| Docker + Docker Compose v2 | PostgreSQL 16 / NATS 2（JetStream）/ Redis 7 / MinIO の起動 |
| `psql`（任意） | DB の手動確認。`make migrate` はコンテナ内の psql を使うため不要 |
| `curl`（任意） | `POST /components/{id}/versions` への multipart アップロード（`make deploy` でも使用） |

toolchain と target は `rust-toolchain.toml` に固定済み（`channel = "stable"`,
`targets = ["wasm32-wasip2"]`）です。次のコマンドでまとめて導入できます。

```bash
# rustup 未導入なら: https://rustup.rs
rustup show                       # rust-toolchain.toml に従って stable を導入
rustup target add wasm32-wasip2   # WebAssembly Component 向け target
cargo install wasm-tools          # 任意
```

`make setup` が上記の確認・導入補助と `COMPONENTS_DIR` / `WASM_CACHE_DIR` 作成、
`.env` 雛形作成までを行います。

```bash
make setup
```

---

## 環境変数

`.env.example` をコピーして `.env` を作成します（`make setup` でも作成されます）。

```bash
cp .env.example .env
```

| 変数 | 既定値 | 用途 |
| --- | --- | --- |
| `DATABASE_URL` | `postgres://faas:faas@localhost:5432/faas` | PostgreSQL 接続先 |
| `NATS_URL` | `nats://localhost:4222` | NATS（JetStream）接続先 |
| `BOOTSTRAP_ADMIN_TOKEN` | `dev-bootstrap-admin-token` | system-admin トークン。`POST /admin/tenants`（テナント作成）を `Authorization: Bearer` で gate（M3a〜）。固定 `AUTH_TOKEN` は廃止 |
| `COMPONENTS_DIR` | `./components-dist` | `make build-component` が echo.wasm を置くローカルディレクトリ |
| `BIND_ADDR` | `0.0.0.0:8080` | control-plane の bind アドレス |
| `S3_ENDPOINT` | `http://localhost:9000` | MinIO の S3 API エンドポイント（§3.4） |
| `S3_REGION` | `us-east-1` | リージョン。MinIO は任意だが aws-sdk が必須とするため固定 |
| `S3_BUCKET` | `faas-components` | wasm 本体の保存バケット（`minio-setup` が作成） |
| `S3_ACCESS_KEY` | `minioadmin` | MinIO アクセスキー（`MINIO_ROOT_USER` と一致） |
| `S3_SECRET_KEY` | `minioadmin` | MinIO シークレットキー（`MINIO_ROOT_PASSWORD` と一致） |
| `MAX_WASM_UPLOAD_BYTES` | `33554432`（32 MiB） | アップロード wasm 本体の最大サイズ（§6.2） |
| `PRESIGN_TTL_SECS` | `300` | JobMessage に同梱する presigned GET URL の TTL（秒・短命 read-only） |
| `WASM_CACHE_DIR` | `./worker-cache` | worker が cwasm（事前コンパイル成果物）をキャッシュするディレクトリ（§3.6） |
| `JOB_SIGNING_KEY` | （開発用ダミー seed） | **M3c**: control-plane が結果トークンの署名に使う Ed25519 seed（32 バイトを hex/base64url/base64 で）。control-plane のみが保持。本番は高エントロピーに置換（§3.3 / 露出ガード §15） |
| `JOB_SIGNING_KID` | `k1` | **M3c**: 署名トークンに埋める kid。subscriber が kid で検証鍵を選ぶ（rotation-ready） |
| `ACK_WAIT_SECS` | `30` | **M3c**: JetStream consumer の ack 待ち秒数。token exp の計算にも使う。**CP と worker で同一値**にすること（§3.3 TTL 結合） |
| `MAX_DELIVER` | `5` | **M3c**: JetStream consumer の最大再配送回数。token exp 計算と共有。**CP と worker で同一値**にすること |
| `TOKEN_MARGIN_SECS` | `60` | **M3c**: token exp に足す余裕秒数（`exp = iat + ACK_WAIT_SECS*MAX_DELIVER + 壁時計上限 + margin`） |
| `REDIS_URL` | `redis://localhost:6379` | **M3d**: 共有 admission ストア（Redis）。全 Axum インスタンスで共有するレート制限 / in-flight / login ロックアウトのカウンタ（§8 MUST）。pure-Rust の `redis` crate で接続 |
| `QUOTA_INVOKE_RATE_PER_SEC` | `50` | **M3d**: `invoke_rate` グローバル既定（req/秒/テナント, §8）。token-bucket 補充レート。テナント上書きは `tenants.quotas` JSONB |
| `QUOTA_INVOKE_BURST` | `500` | **M3d**: token-bucket のバースト容量（§8 上限相当） |
| `QUOTA_MAX_CONCURRENT_EXECUTIONS` | `20` | **M3d**: `max_concurrent_executions` グローバル既定（in-flight = pending+running, §8） |
| `INFLIGHT_TTL_SECS` | `3600` | **M3d**: in-flight カウンタキーの TTL（秒）。reaper が DB COUNT へ再同期するまでの孤立カウンタ保険（§8） |
| `REAPER_INTERVAL_SECS` | `30` | **M3d**: reaper の再同期間隔（秒, §8 ドリフト補正） |
| `LOGIN_LOCKOUT_THRESHOLD` | `10` | **M3d**: login 失敗ロックアウト閾値（§6.0）。(tenant,email) と IP の **両キー**で数え、どちらか到達で 401（fail-closed） |
| `LOGIN_LOCKOUT_WINDOW_SECS` | `900` | **M3d**: login 失敗カウンタの減衰窓（秒）。最後の失敗からこの時間無失敗で解放 |
| `TRUST_PROXY_HEADERS` | `false` | **M3d**: `X-Forwarded-For` を信頼してクライアント IP を取り出すか（§6.0）。既定 false（プロキシ信頼は明示オプトイン） |
| `UPLOAD_PRESIGN_TTL_SECS` | `300` | **M3d**: `POST /uploads` が返す single-key presigned PUT URL の TTL（秒, §3.4/§5.2） |
| `STUCK_EXECUTION_DEADLINE_SECS` | `900` | **M4c**: stuck-execution sweeper の deadline。`created_at + この秒数`を超えても終端化されない pending/running 行を reaper が `failed` に finalize して in-flight スロットを回収（§8）。0 で無効化。**M4c の `.failed` (DLQ) subscriber が一次救済、本値は二次救済**（§6.6） |
| `LOG_FORMAT` | `text` | **M4a**: ログ形式。`json` で `tracing_subscriber::fmt().json()` を有効化し、`execution_id` / `tenant_id` を flatten した 1 行 JSON で出力（集約基盤向け）。既定 `text` は従来挙動と完全互換（§3.8） |
| `METRICS_BIND_ADDR` | `0.0.0.0:9090` | **M4a**: worker が `/metrics` + `/readyz` + `/healthz` を公開する独立 axum サーバの bind 先。JetStream pull ループから独立し、stall しても liveness が応答可能（§3.8） |
| `BACKOFF_SECS` | `5,15,60` | **M4c**: JetStream pull consumer の `backoff` 配列（CSV、秒単位）。空文字で「固定 ack_wait 構成」に戻る（§6.6）。合計が `ACK_WAIT_SECS*MAX_DELIVER` を下回る範囲で使うこと（上回ると正規の遅延結果がトークン失効後に届く恐れ。§3.3） |

> `sqlx` はコンパイル時マクロ（`query!`）ではなくランタイム API（`sqlx::query` /
> `sqlx::query_as`）を使うため、**ビルド時に DB / MinIO / Redis は不要**です。これらは実行時のみ必要です。

> **M3d 共有ストア（§8）**: control-plane はステートレス x N で水平分散するため、レート制限・
> in-flight 同時実行・login 失敗カウントを**全インスタンスで共有する低レイテンシストア（Redis）**で
> 集計します（インスタンスローカルだと上限が実効 N 倍に緩む）。読み取り→判定→書き込みは Lua
> スクリプトでサーバ側原子実行し TOCTOU を避けます。**fail-mode（§8）**: login ロックアウトは
> fail-closed（Redis 到達不能 → 拒否）、invoke のレート制限 / in-flight は fail-open（許可しつつ
> 縮退をログ・監査）。起動時に Redis 到達不能なら**プロセスローカルの縮退スタブ**で起動を継続します
> （ブートを単一障害点にしない。縮退中は分散共有でなくなるため警告を出します）。本番は HA 構成に
> してください（§9）。`redis` crate は pure-Rust（`tokio-comp`）で C 依存（cc/aws-lc）を引き込みません。

> **M3c 結合の注意**: `JOB_SIGNING_KEY` / `JOB_SIGNING_KID` は **control-plane のみ**が読みます
> （worker は鍵を持たず、不透明トークンを echo するだけ）。`ACK_WAIT_SECS` / `MAX_DELIVER` は
> control-plane（token exp）と worker（consumer 設定）の **両方**が読むため、値をずらすと
> 正規の遅延結果がトークン失効扱いになりえます（subscriber は「失効しても行が pending/running
> なら受理」する安全網を持ちますが、定数は揃えてください）。既存の durable consumer
> `workers` が既定値で作成済みの場合、ack_wait/max_deliver の変更が反映されないことがあります
> （その場合は consumer を作り直してください）。

---

## 起動手順

別々のターミナルで control-plane と worker を起動します。Make ターゲットの一覧は `make help`。

### 1. 依存サービス起動（PostgreSQL + NATS JetStream + Redis + MinIO）

```bash
make up        # docker compose up -d --wait（healthcheck が healthy になるまで待機）
```

`make up` は MinIO も起動し、ワンショットの `minio-setup` がバケット `faas-components` を
冪等に作成します。手動で作り直したい場合は `make minio-bucket`（コンテナ内の `mc` で作成）。

### 2. マイグレーション適用

```bash
make migrate   # migrations/*.sql を順に適用。default テナントを 1 行 seed する
```

`make migrate` はコンテナ内 psql に `migrations/` 配下を順に流し込みます（`0001_init.sql`
→ `0002_m2.sql` → `0003_auth.sql` → `0004_rls.sql` → `0005_provenance.sql` →
`0006_large_io.sql`）。control-plane 起動時にも埋め込み sqlx migrator が pending を冪等適用
するため、`make migrate` を省略しても起動時に揃います。スキーマは `tenants` / `users` /
`api_tokens` / `components` / `component_versions` / `executions` / `audit_logs` で、全テナント
表に `ENABLE / FORCE ROW LEVEL SECURITY` を適用済み（§3.2 M3b）。状態は TEXT + CHECK enum、
id は推測困難なランダム TEXT（§3.7）。ランタイム接続ロール `faas_app` は `BYPASSRLS` を持たず、
`audit_logs` は SELECT/INSERT のみで UPDATE/DELETE が拒否されます（§3.7）。

### 3. echo Component のビルドと配置

```bash
make build-component
# 内部: cargo build -p echo --target wasm32-wasip2 --release
#       → target/wasm32-wasip2/release/echo.wasm を COMPONENTS_DIR/echo.wasm へコピー
```

`wasm32-wasip2` ターゲットでビルドすると、出力は WebAssembly **Component** として
そのまま生成されます（追加の adapter / componentize ステップは不要）。この成果物は
**アップロード対象のローカルファイル**です（worker は MinIO から取得します。§3.4）。

### 4. control-plane 起動

```bash
make run-cp    # cargo run -p faas-control-plane
```

### 5. worker 起動（別ターミナル）

```bash
make run-worker  # cargo run -p faas-worker
```

worker は JobMessage の presigned GET URL から本体を取得し、`WASM_CACHE_DIR` に cwasm を
キャッシュします（M1 のように `COMPONENTS_DIR` から直接 wasm を読むことはありません）。

---

## end-to-end 確認

ワンショットで通すには `make invoke` を実行します。`make invoke` は内部で `make deploy`
（登録 + アップロード）を呼んだうえで invoke → ポーリングまで行います。

```bash
make bootstrap # スモーク用テナント + admin ユーザを作成（再実行可能）
make deploy    # POST /components(name=echo) → POST /components/{id}/versions(version=0.1.0, wasm=echo.wasm)
make invoke    # deploy を実行 → POST /invoke → GET /executions/{id}
```

以下は同等の `curl` 手順です。M3a 以降は固定トークンではなく、`POST /auth/login`
で取得した API トークン（`$TOKEN`）を `Authorization: Bearer` に使います。

### 0. ログインしてトークンを取得（`POST /auth/login`）

事前に `POST /admin/tenants`（`BOOTSTRAP_ADMIN_TOKEN` で gate）でテナントと最初の
admin ユーザを 1 回で作成しておきます（`make bootstrap` 参照。リクエストボディに
`admin_email`・`admin_password` を含めると、テナントと admin ユーザが同一
トランザクションで作成されます）。

```bash
TOKEN=$(curl -s -X POST http://localhost:8080/auth/login \
  -H "Content-Type: application/json" \
  -d '{"tenant_slug":"smoke","email":"admin@example.com","password":"dev-password"}' \
  | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
# => 201 Created。token は一度だけ返るので $TOKEN に保持する。
```

### 1. Component を登録（`POST /components`）

メタデータのみ作成します。初期 version は作られず `active_version_id` は NULL です。

```bash
curl -s -X POST http://localhost:8080/components \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"echo"}'
# => 201 Created
#    {"component_id":"cmp_xxx","name":"echo"}
```

### 2. wasm 本体をアップロード（`POST /components/{id}/versions`, multipart）

本体は検証パイプライン（サイズ上限 → wasmparser 検証 → import 許可リスト → sha256）を
通過したものだけが MinIO に保存され、その version が `active` 化されます。

```bash
curl -s -X POST http://localhost:8080/components/cmp_xxx/versions \
  -H "Authorization: Bearer $TOKEN" \
  -F "version=0.1.0" \
  -F "wasm=@components-dist/echo.wasm;type=application/wasm"
# => 201 Created
#    {"version_id":"ver_xxx","version":"0.1.0","status":"active"}
```

> 任意フィールド: `resource_limits`（`{"max_memory_bytes":..,"max_wall_time_ms":..}` の JSON）、
> `capabilities`（JSON）。
>
> **Capability 強制（M3d, §4.4）**: capability は既定 **deny-all**。クライアントが宣言した
> `capabilities` は **信用しません**（承認は admin スコープの管理操作）。検証は WIT import を
> **admin 承認済み集合**（本スライスでは標準 handler world 契約 `faas:component/*` + 標準
> WASI Preview2 `wasi:*` を baseline 固定）と **厳密照合** し、未承認の import（任意の ambient
> capability）があれば **422** で拒否します。`component_versions.capabilities` には宣言値ではなく
> **承認集合と照合して解決した import** を保存します。

### 3. Invoke（`POST /invoke`）

```bash
curl -s -X POST http://localhost:8080/invoke \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"component":"echo","input":{"hello":"world"}}'
# => 202 Accepted
#    {"execution_id":"exec_xxx","status":"pending"}
```

> 実行するバージョンはサーバ側が active version から解決するため、`version` フィールドは
> 受け取りません。control-plane が active version の本体へ短命 presigned GET URL を発行し、
> JobMessage に同梱します。

#### 冪等な invoke（`Idempotency-Key`, M3c §6.6）

`Idempotency-Key` ヘッダを付けると、同一キーの再送が重複実行を生みません（3 層冪等）:

```bash
curl -s -X POST http://localhost:8080/invoke \
  -H "Authorization: Bearer $TOKEN" \
  -H "Idempotency-Key: order-2026-0001" \
  -H "Content-Type: application/json" \
  -d '{"component":"echo","input":{"hello":"world"}}'
# 1 回目 => 202、execution を作成して publish。
# 同一キー + 同一 body の再送 => 202、**既存の** execution をそのまま返す（再作成・再 publish しない）。
# 同一キー + 異なる body の再送 => 409 Conflict（idempotency key reused with a different request body）。
```

- キー形式: 非空・最大 255 文字・`[A-Za-z0-9._-]` のみ（違反は 400）。
- 一致判定はリクエスト body の **正準ハッシュ**（`{component,input}` をキー順非依存で sha256）。
- dedup は `(tenant_id, idempotency_key)` の部分 UNIQUE index が権威（並行同一キーの競合も backstop）。
- 透過の 2 層: `execution_id`（サーバ採番・CAS 終端）と、invoke の JetStream publish に付与する
  `Nats-Msg-Id = execution_id`（per-stream 重複排除）。

#### 大容量 I/O（`POST /uploads` + `input_ref`, M3d §3.4 / §5.2 / §6.4）

インライン上限（256 KiB）を超える入力は、`POST /invoke` の前に Object Storage へ退避します:

```bash
# 1) アップロード枠を予約（execution_id 採番 + single-key presigned PUT URL を取得）。
#    executions 行は INSERT されず、in-flight 同時実行も消費しません（§6.0）。
curl -s -X POST http://localhost:8080/uploads -H "Authorization: Bearer $TOKEN"
# => 201 {"execution_id":"exec_xxx",
#         "input_ref":"tenants/ten_xxx/io/exec_xxx/input",
#         "upload_url":"https://...signed-PUT...","expires_at":"..."}

# 2) 本体を upload_url へ PUT（single-key 限定の write 資格）。
curl -s -X PUT --upload-file big-input.bin "<upload_url>"

# 3) 予約済み execution_id と input_ref を指定して invoke（input とは排他）。
curl -s -X POST http://localhost:8080/invoke \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"component":"echo","execution_id":"exec_xxx",
       "input_ref":"tenants/ten_xxx/io/exec_xxx/input"}'
# => 202 {"execution_id":"exec_xxx","status":"pending"}
```

- control-plane は `input_ref` が `tenants/{呼び出しテナント}/io/{当該 execution_id}/input` に
  **完全一致**することを検証します（**prefix 一致は不可**。別 execution / 別テナントの入力を指す
  `input_ref` は 422/403, §3.4 MUST）。worker へ同梱する read 資格は invoke の execution_id から
  導出したキーに固定され、worker は **そのキー以外を読みません**（§3.4 MUST NOT）。
- 大出力（`output_ref`）は DB 列・キーレイアウト（`io/{execution_id}/output`）を敷設済みですが、
  worker 側の write offload は最小実装（後続スライスの TODO）です。

#### 結果の出所認証（署名ジョブトークン, M3c §3.3）

control-plane は invoke 時にジョブごとの claim（execution_id / tenant_id / version_id / kid /
iat / exp）を **Ed25519** で署名し、不透明な `job_token` を JobMessage に載せます。worker は鍵を
持たず、この `job_token` を全 result（成功 / 失敗 / タイムアウト）に **verbatim に echo** します。
control-plane の subscriber は署名を kid で検証し、claim を execution 行（execution_id / tenant_id
/ version_id）と subject 由来テナントの両方に突き合わせてから CAS 終端します。**検証に失敗した
結果は drop し、`audit_logs` に追記専用の監査行を 1 行残します**（生トークン・秘密は記録しません）。
`exp` は失効していても、execution 行が `pending`/`running` の間は受理します（正規の遅延結果を
取りこぼさないため。§3.3 TTL 結合）。

`audit_logs` は他テナントから隔離（FORCE RLS）され、`faas_app` には SELECT/INSERT のみ付与
（UPDATE/DELETE なし）で **改竄・削除不可** です（§3.2）。

### 4. 実行状態の取得（`GET /executions/{id}`）

```bash
curl -s http://localhost:8080/executions/exec_xxx \
  -H "Authorization: Bearer $TOKEN"
# => {"execution_id":"exec_xxx","status":"succeeded","output":{"echo":{"hello":"world"}}, ...}
```

`status` が `pending → running → succeeded` と遷移すれば end-to-end が貫通しています。
同じ Component を再度 invoke すると、worker が `wasm_sha256` で cache hit するため
2 回目以降の coldstart が短縮されます（M2 の完了条件）。

### ヘルスチェック（認証不要）

```bash
curl -s http://localhost:8080/healthz                 # liveness => {"status":"ok"}
curl -s http://localhost:8080/readyz                  # readiness (M4a) => {"db":"ok","nats":"ok","store":"ok"}
curl -s http://localhost:8080/metrics | head -20      # Prometheus (M4a, control-plane)
curl -s http://localhost:9090/metrics | head -20      # Prometheus (M4a, worker — METRICS_BIND_ADDR)
```

### Chaos / 障害注入テスト（M4 完了条件の検証）

仕様書 §15 M4 完了条件「障害注入（Worker 落下・再配送・タイムアウト）で実行喪失・二重実行が
起きない」を `crates/control-plane/tests/chaos_m4.rs` の 4 シナリオで end-to-end 検証します。
通常の `cargo test` では `#[ignore]` で除外されており、docker stack + 専用 component が必要です。

```bash
# 共通: docker stack を起動して bootstrap + token を用意
make up && make migrate && make bootstrap
export CHAOS_TOKEN=$(make -s login | tail -1)

# Scenario B (一番手軽。echo だけで動く) — 同一 Idempotency-Key で 2 連 invoke → 同一 execution_id
make build-component && make deploy
CHAOS_ECHO=echo cargo test -p faas-control-plane --test chaos_m4 \
  -- --ignored chaos_b_idempotency_key_dedups --nocapture

# Scenario A — Worker crash mid-execution → stuck-execution sweeper が failed で finalize
# CP を短い deadline で起動し、worker を止めた状態で invoke
pkill -f 'target.*control-plane'
set -a; source .env; set +a
STUCK_EXECUTION_DEADLINE_SECS=15 REAPER_INTERVAL_SECS=5 \
  cargo run -p faas-control-plane --release > /tmp/cp.log 2>&1 &
pkill -f 'target.*faas-worker'   # worker を落とす
export CHAOS_TOKEN=$(make -s login | tail -1)
CHAOS_STUCK_DEADLINE_SECS=15 CHAOS_WAIT_SECS=35 \
  cargo test -p faas-control-plane --test chaos_m4 \
  -- --ignored chaos_a_worker_crash_finalizes_to_failed --nocapture

# Scenario C, D — 専用 component (always-trap / slow) を deploy してから実行
cargo build --release -p always-trap -p slow --target wasm32-wasip2
cp target/wasm32-wasip2/release/always_trap.wasm components-dist/always-trap.wasm
cp target/wasm32-wasip2/release/slow.wasm components-dist/slow.wasm
# always-trap と slow を POST /components → /components/{id}/versions で個別アップロード
# （詳細手順は scripts/ に追加予定; 現状は手動 curl）
make run-worker > /tmp/worker.log 2>&1 &
CHAOS_ALWAYS_TRAP=always-trap CHAOS_SLOW=slow \
  cargo test -p faas-control-plane --test chaos_m4 \
  -- --ignored chaos_c_dlq_finalizes_after_max_deliver \
            chaos_d_tokio_timeout_finalizes_to_timeout --nocapture
```

期待される最終 status:
- chaos_a: `status=failed`（sweeper, error.message に sweeper 由来文言）
- chaos_b: `status=succeeded`（同一 execution_id を 2 度受領、wasm は 1 度だけ実行）
- chaos_c: `status=failed`（trap → result subscriber または DLQ subscriber）
- chaos_d: `status=timeout`（epoch interruption → `Trap::Interrupt` → tokio timeout 経由）

---

## エンドポイント一覧

| メソッド / パス | 認証 | 説明 |
| --- | --- | --- |
| `GET /healthz` | 不要 | liveness（DB/NATS 非依存。プロセスが応答できるか） |
| `GET /readyz` | 不要 | **M4a**: readiness。DB `SELECT 1` / NATS `connection_state()` / 共有ストア `Store::ping()` を順に叩き、いずれか失敗で 503（§3.8） |
| `GET /metrics` | 不要 | **M4a**: Prometheus exposition format（control-plane: `faas_*` 系、worker: `wasmtime_*` 系 + `executions_total{outcome}`）。worker は別 axum サーバ（既定 `:9090`）で公開（§3.8） |
| `POST /auth/login` | 不要 | email+password で API トークンを発行（一度だけ平文 secret を返す, §3.3） |
| `POST /admin/tenants` | bootstrap | テナント + 最初の admin ユーザを作成（§9 bootstrap）。`BOOTSTRAP_ADMIN_TOKEN` で gate（§3.3） |
| `POST /tenants/{id}/users` | admin | テナント内ユーザ作成（自テナント限定。他テナントは 404, §3.3） |
| `POST /tokens` | admin | 対象ユーザ向け API トークン発行（要求 ∩ 発行者 ∩ 対象ロール上限, §3.3） |
| `DELETE /tokens/{id}` | admin | トークン失効（自テナント限定。他テナントは 404, §3.3） |
| `POST /components` | deploy | Component メタデータ作成（初期 version は作らない, §6.2） |
| `GET /components` | read | Component 一覧（soft delete 済みを除く, §6.7） |
| `POST /components/{id}/versions` | deploy | wasm 本体アップロード + 検証 + デプロイ（multipart, §6.2） |
| `GET /components/{id}/versions` | read | version 一覧（soft delete 済みを除く, §6.7） |
| `DELETE /components/{id}` | admin | Component の soft delete（参照中の実行があれば 409, §6.7） |
| `DELETE /components/{id}/versions/{ver}` | admin | version の soft delete（active / 参照中は 409, §6.7） |
| `PUT /components/{id}/active-version` | admin | active version の切替（ロールバック, §6.7） |
| `POST /invoke` | invoke | active version を解決し JobMessage を publish（202）。大入力時は予約済み `execution_id` + `input_ref`（完全一致検証, §3.4） |
| `POST /uploads` | invoke | 大入力アップロード用 single-key presigned PUT URL を発行（`execution_id` 予約。行 INSERT/INCR なし, §5.2/§6.4） |
| `GET /executions/{id}` | read | 実行状態の参照（`input_ref`/`output_ref` を含む, §6.4） |

---

## トラブルシュート

| 症状 | 原因と対処 |
| --- | --- |
| `make up` が healthy にならない | ポート 5432 / 4222 / 8222 / 9000 / 9001 が他プロセスと衝突。`docker compose ps` / `make logs` を確認。`make clean` で volume ごと初期化して再試行。 |
| `make migrate` が `already exists` で失敗 | 既にスキーマ適用済み。再初期化は `make clean && make up && make migrate`。 |
| `401 Unauthorized` | API トークンが欠損・不一致・失効・期限切れ。`POST /auth/login` で新しいトークンを取得して `Authorization: Bearer` に使う。テナント未作成なら先に `POST /admin/tenants`（`BOOTSTRAP_ADMIN_TOKEN`）。 |
| `403 Forbidden` | トークンのスコープがルート要件（read/invoke/deploy/admin）に不足。必要なスコープを持つトークンで発行し直す。 |
| アップロードが `400 Bad Request`（検証失敗） | wasm が Component Model として不正、または許可外 import を含む。`make build-component` で生成した `wasm32-wasip2` の Component を使う。許可リストは `crates/control-plane/src/validation.rs` を参照。 |
| アップロードが「max upload size」で失敗 | 本体が `MAX_WASM_UPLOAD_BYTES`（既定 32 MiB）超過。`.env` で上限を見直す。 |
| アップロードが `version already exists` | 同一 `(component, version)` への上書きは禁止（§6.7）。別の `VERSION` を指定する（`make deploy VERSION=0.1.1`）。 |
| control-plane 起動直後にアップロードが内部エラー / presign 失敗 | MinIO 未起動、またはバケット未作成。`make up` で MinIO が healthy か確認し、`make minio-bucket` でバケット `faas-components` を作成。`S3_ENDPOINT` / `S3_ACCESS_KEY` / `S3_SECRET_KEY` が `.env` と MinIO の資格情報で一致しているか確認。 |
| invoke が `has no active version` | その Component に version が未アップロード。先に `POST /components/{id}/versions`（`make deploy`）を実行する。 |
| invoke 後ずっと `pending` のまま | worker 未起動 / NATS 未接続。`make run-worker` の起動と `NATS_URL` を確認。JetStream は `make up` の NATS が `--jetstream` で起動済み。 |
| `running` で止まり `failed` になる（本体取得失敗） | presigned URL の期限切れ（`PRESIGN_TTL_SECS` 既定 300 秒）や MinIO 不通。worker から `S3_ENDPOINT` のホストへ到達できるか、URL 発行から取得までが TTL 内かを確認。 |
| 2 回目の invoke でも coldstart が短縮されない | `WASM_CACHE_DIR` に書き込めていない可能性。worker のログで `cwasm hit` / `cwasm 書き込み` を確認し、ディレクトリの権限と空き容量を見る。`make clean` 相当で cache を消すには `WASM_CACHE_DIR` を手動削除。 |
| `status: timeout` | `max_wall_time`（既定 1000ms）超過。アップロード時の `resource_limits` を見直す。 |
| ビルド時に DB / MinIO を要求される | 本実装は `sqlx` のランタイム API を使うため不要。`query!` マクロ起因のエラーが出る場合は実装が契約違反。 |

---

## ディレクトリ構成

```
Cargo.toml                 # [workspace] members（crates/* と components/echo）
rust-toolchain.toml        # stable + wasm32-wasip2
.env.example               # 環境変数の雛形（S3_* / WASM_CACHE_DIR / 上限・TTL 含む）
docker-compose.yml         # postgres:16 + nats:2 (--jetstream) + minio (+ minio-setup)
Makefile                   # setup / up / migrate / minio-bucket / build-component / run-* / deploy / invoke
wit/world.wit              # faas:component@1.0.0（handler world）
migrations/0001_init.sql   # M1: tenants / components / component_versions / executions
migrations/0002_m2.sql     # M2: バージョン管理・サイズ等の追加カラム / インデックス
migrations/0003_auth.sql   # M3a: users / api_tokens、faas_app ロール、認証・認可スキーマ
migrations/0004_rls.sql    # M3b: 全テナント表に ENABLE / FORCE ROW LEVEL SECURITY とポリシー
migrations/0005_provenance.sql  # M3c: 署名鍵 kid / idempotency_key UNIQUE / audit_logs (append-only)
migrations/0006_large_io.sql    # M3d: input_ref / output_ref 列、tenants.quotas、大容量 I/O 用
crates/shared/             # faas-shared: 型・NATS subject・メッセージ・エラー（共有契約の唯一の真実）
                           #   FailedMessage / failed_subject 等の M4c DLQ 型を含む
crates/control-plane/      # faas-control-plane (bin): axum + storage(MinIO) + validation(wasmparser)
                           #   admission(Redis) / signing(Ed25519) / authz / RLS / subscriber
                           #   reaper + DLQ subscriber + metrics (M4a/c)
crates/control-plane/tests/chaos_m4.rs  # M4 障害注入の end-to-end テスト（#[ignore]）
crates/control-plane/src/metrics.rs     # M4a: Prometheus Registry とメトリクス定義
crates/worker/             # faas-worker (bin): wasmtime + async-nats + reqwest + cwasm キャッシュ
                           #   epoch ticker は OS スレッド (chaos_d 対策。M4b 設計メモ参照)
crates/worker/src/metrics.rs            # M4a: worker 側 Prometheus 公開（独立 axum サーバ）
components/echo/           # サンプル Component（cdylib, wasm32-wasip2）
components/always-trap/    # M4 chaos_c 用: handle 入口で panic（trap → DLQ 経路）
components/slow/           # M4 chaos_d 用: handle が tight loop（epoch interrupt → timeout 経路）
仕様書.md                  # 全体仕様（M4 範囲は §15 M4 / §3.8 / §4.3 / §6.6 / §8）
```

---

## 次のマイルストーン（仕様書 §15）

M4 完了済み（本リポジトリの現状）。次は M5 以降の将来⬜:

- **M5 以降（§10 / §14）**: 分散トレーシング（OpenTelemetry）→ 同期 Invoke（reply subject） →
  外部イベントトリガー → Result Ingestor 分離 → Workflow Engine / Cron → Secrets Manager →
  AI 統合 → Multi Region。スケール要求が顕在化した時点で順次。
- **M4 follow-ups**（M4 範囲内で残る配線。`crates/control-plane/src/metrics.rs` 等の
  メトリクス登録は完了しているが record 配線が未到達）:
  - HTTP middleware で `faas_http_requests_total` / `_duration_seconds` の observe 配線
  - `execution_duration_seconds`（created_at→finished_at）を subscriber finalize 時に observe
  - per-tenant ラベル（`faas_tenant_invoke_total{tenant_id}`）のカーディナリティ対策
    （`METRICS_INCLUDE_TENANT_LABEL=false` フラグ追加）
  - `.result` / `.failed` の JetStream 化（現状 core NATS、CP 再起動で in-flight ドロップ）
  - `tenants.quotas` / `tenants.status` の admin API（現状 SQL 直 UPDATE）
  - 検証パイプラインの隔離プロセス化（§6.2 MUST。現状はインプロセス）

詳細は `仕様書.md` の §15 実装ロードマップを参照してください。
