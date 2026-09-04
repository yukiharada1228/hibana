# WASM FaaS Platform — M9 (サンドボックス強化・サプライチェーン)

[![CI](https://github.com/yukiharada1228/hibana/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/yukiharada1228/hibana/actions/workflows/ci.yml)

WebAssembly Component をアップロードして invoke すると、Wasmtime Worker が実行して
結果を返す FaaS プラットフォームです。本リポジトリの現状は **仕様書.md §15 の M9
（サンドボックス強化・サプライチェーン）範囲**であり、M1〜M6 の invoke 経路 / アップロード・検証・
デプロイ / マルチテナント・認証・RLS・結果出所認証・Capability 強制 / メトリクス・DLQ・
完全なリトライ/タイムアウト / 利用量計量 / 同期 Invoke・Cron・トリガー、
M7 のデプロイ運用（canary / rollback / per-function env / Secrets）、
M8 の弾力スケールとテナント間アイソレーション（テナント別 lane / 実行クレジット / オートスケール）の上に、
**「未信頼 wasm をマルチテナントで常時実行するための多層防御」**を完成させます。具体的には
(1) **検証のプロセス隔離**（悪性 wasm による検証 DoS を子プロセス 1 個に限局, M9b / §6.2）、
(2) **egress allowlist の実効強制**（承認した `host:port` へのみ outbound。SSRF / DNS rebinding を
    ハードデニーで塞ぐ, M9c / §4.4）、
(3) **署名付き Component**（テナント鍵での供給網検証。deploy トークン漏洩を単独で無力化, M9a / §6.2）を、
いずれも **既存の分離・計量・デプロイ運用・スケールを壊さず**に積み上げます。完了条件は脅威モデル
（T1〜T8）に基づくレッドチームテスト `chaos_m9.rs`（v1/v3/v4/v5/v6/v7）で、悪意ある wasm が
隣接テナント / ホスト / 未許可 outbound / 供給網に到達できないことを確認します。

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
- `worker`: NATS JetStream の invoke subject を **テナント別 lane Consumer**（durable
  `workers-t-{tenant}` / 超過分は `workers-overflow`, M8 §3.3）で購読。**consumer は作らない**
  —— 作成者は control-plane 単独であり、worker は NATS の consumer 一覧から購読先を発見する
  （`faas.lane.changed` の即時通知 + 周期 discovery）。プロセス全体の同時実行は
  `WORKER_MAX_CONCURRENCY` を lane 数で割った実効クレジットで閉じ、SIGTERM では
  in-flight を待ってから終了する（§4）。`TENANT_LANES_ENABLED=false` なら M7 までと同じ
  共有 durable `workers` 1 本に収束する。
  JobMessage の `wasm_url`（presigned GET URL）から本体を取得し、`wasm_sha256` をキーに
  **Component をキャッシュ + 事前コンパイル**する（§3.6）。cwasm は `WASM_CACHE_DIR/{sha256}.cwasm`
  として永続化し、2 回目以降は cache hit で deserialize するだけになり coldstart が短縮される。
  Worker は**署名検証鍵を保持せず**、`job_token` を全 result に **verbatim に echo** するだけ
  （§3.3 MUST NOT）。
- `components/echo`: 入力をそのまま返す最小の Component（`wasm32-wasip2`）。アップロード対象の
  ローカル成果物。
- `components/burn`: 入力 `{"burn_ms": N}` のぶんだけ busy-loop してから **succeeded で終端**する
  検証用 Component（M8）。`components/slow` は無限ループで必ず timeout 終端するため
  「N ミリ秒かかって成功する」ノブとしては使えず、M8 の負荷（件数 × 1 件あたり実行時間）を
  制御するために新設した。
- `scripts/worker-autoscale.sh` ほか: `GET /internal/scale` を見て worker プロセスを増減させる
  **参照アクチュエータ**（M8 §5.5）。`make run-workers` / `make autoscale` / `make stop-workers`。

---

## M9 スコープ（と非スコープ）

仕様書.md §15 M9 に準拠。**未信頼 wasm をマルチテナントで常時実行するための多層防御を完成させる**
（§12 原則 5）。設計の詳細と脅威モデル（T1〜T8）は `docs/M9-design.md`。

### 着手前に現状を実測した（レッドチーム偵察）

敵対的 component を作り、既存の防御が実際にどこで止めるかを確かめた。分かったこと:
**防御は二重で両方 deny-by-default** ——(1) アップロード検証、(2) ランタイムの `WasiCtxBuilder::new()`
が何も grant しない（env 非継承 / preopen 無し / socket_addr_check 既定全拒否）。
**真の防御は空の WasiCtx** であり、M9 はこの結論に沿って「import は許可し、実際の到達はランタイムで
gate する」形にした。埋めた穴は 3 つ:

### 含む（M9b: 検証のプロセス隔離, §6.2）

- wasm 検証を control-plane 本体とは**別プロセス**で実行（`--validate-stdin` で自分自身を起動）。
  悪性 wasm が wasmparser の資源を食い潰しても被害を子プロセス 1 個に限局する。
- 子は Linux の `RLIMIT_AS` + wall-clock timeout で二重に縛る。timeout/OOM/クラッシュは wasm 起因
  として 4xx、子の spawn 失敗のみ 503。`VALIDATION_TIMEOUT_SECS` / `VALIDATION_MEM_LIMIT_MB`。

### 含む（M9c: egress allowlist の実効強制, §4.4）

- admin 承認した `host:port`（`capabilities.net_allow_outbound`）へのみ outbound を許す。
  未承認 / allowlist 外は worker の `socket_addr_check` が拒否する。
- **SSRF / DNS rebinding 対策が核**: `socket_addr_check` は解決後の IP しか見えないので、worker が
  allowlist を自分で解決して IP 照合し、プライベート/メタデータ IP（`169.254.169.254` 等）を
  **allowlist より優先して hard-deny** する（`crates/shared/src/egress.rs`、網羅テスト済み）。
- `wasi:sockets/*` / `wasi:filesystem/*` の **import は許可**する（Rust std が推移的に要求するため）。
  実際の到達可否はランタイムが決める（fs は preopen 無しで全拒否）。

### 含む（M9a: 署名付き Component と供給網検証, §6.2）

- テナントが登録した Ed25519 公開鍵で wasm 本体の sha256 への detached 署名を検証する。
  deploy トークン漏洩を単独で無力化する（認可と真正性を別々の秘密に依存させる）。
- per-tenant の `require_signed_components`（既定 false = 後方互換）。**秘密鍵は渡さない**
  （公開鍵だけ登録。複数鍵でローテーション、retire は検証を残す soft）。

### 完了条件（レッドチームテスト）

`crates/control-plane/tests/chaos_m9.rs` の v1（fs 全拒否）/ v3・v4・v5（egress）/ v6（検証 DoS）/
v7（署名）が全緑。悪意ある wasm が隣接テナント / ホスト / 未許可 outbound / 供給網に到達できない
ことを、各シナリオに**負の対照**（正規操作が成功すること）を付けて確認する。

### 含まない（M10 以降 / follow-up）

- egress バイトの計量（到達可否のみが M9 スコープ）/ mTLS・証明書ピンニング /
  seccomp・gVisor 等の OS レベル追加サンドボックス（`docs/M9-design.md` §8）。

---

## M8 スコープ（と非スコープ）

仕様書.md §15 M8 に準拠。**「隣のテナントの負荷で自分が遅くならない」ことと「負荷に応じて
worker が増減すること」**を与える。

### 完了条件を書き換えた理由（重要）

§15 M8 の当初の完了条件は「あるテナントのバーストが他テナントの **p99 レイテンシ**を劣化させない」
だった。これは**このリポジトリでは測れない**。chaos は公開 API だけを使う黒箱テストであり、
Prometheus の histogram を読む手段が無いからである。測れない条件は「通ったことにする」以外の
使い道が無いので、同じ性質を**測れる形**へ書き換えた:

> あるテナントのバーストが他テナントの **サーバ時計で測ったキュー待ちの絶対上限**・
> **429 件数**・**実行順序の件数**を劣化させず、負荷に応じて worker の**実プロセス数**が増減すること。

- キュー待ちは `started_at - created_at`（どちらもサーバが書いた時刻）で測る。
  クライアントの `Instant` は使わない
- 順序は「B の最後の実行より前に始まった A の実行の**件数**」で測る。
  FIFO 単一レーンなら構造的に BURST 件、lane が分かれていれば数十のオーダーになる
- worker 台数は `wasmtime_worker_slot` gauge で数える（プロセス introspection は使わない）

### 含む（M8a: テナント別 lane Consumer, §3.3 / §8）

- **JetStream の配送枠をテナント単位で分ける**。`max_ack_pending` を**テナント自身の
  in-flight 上限から導出**する（M7 までは全テナント合算の固定値 1000 だった）
- 専有 lane の上限 `MAX_DEDICATED_LANES` を超えた分は **overflow lane 1 本**へ束ねる
- consumer の作成 / 更新 / 削除の責務を **worker から control-plane へ移管**。
  単一 writer は `pg_try_advisory_lock` で強制する
- stream の retention を `WorkQueue` へ移行（`make recreate-stream`）
- **ロールバックは env 1 個で完結する**（`TENANT_LANES_ENABLED=false` で legacy へ収束）

> **なぜ合算方式が壊れるのか**: `Σ(アクティブテナント数 × max_concurrent_executions)` が
> 1000 を超えると、**1 件しか投げていないテナントが、他テナントで埋まった配送枠のせいで
> 配送されない**。配送されないと pending 行が減らず、reaper が Redis カウンタを高いまま維持し、
> やがてそのテナント自身の invoke が 429 になる。既定 20/テナントなら **50 テナントで到達する**。

### 含む（M8b: worker の実行クレジット + ドレイン, §4.2 / §4.3 / §4.6）

- `WORKER_MAX_CONCURRENCY` を lane 数で割った実効クレジットを lane ごとに配り、
  **プロセス全体の同時実行上限を「除算」で閉じる**（第 2 の Semaphore を重ねない）
- pull は「確保済み permit 数ちょうど」しか要求しない（over-fetch しない）
- `WORKER_DRAIN_TIMEOUT_SECS`（**既定 0 = 無効**）で SIGTERM ドレイン。
  ドレイン中は `/readyz` が 503 になり、取得済み未処理メッセージは**遅延付き NAK** で差し戻す

### 含む（M8c: backlog シグナルと参照アクチュエータ, §5）

- `backlog = Σ over all lanes (num_pending + num_ack_pending)` を周期観測（`SCALE_POLL_INTERVAL_SECS`）
- `GET /internal/scale` が desired worker 数を返す。**未観測 / 陳腐化 / ポーラ無効のときは 503**
  （`desired: 0` を返すとアクチュエータが全台停止させるため、「値が取れない」と「0」を HTTP で分ける）
- scale-out は即時、scale-in は `SCALE_IN_COOLDOWN_SECS` のヒステリシス
- `scripts/worker-autoscale.sh`（+ `make autoscale`）が参照実装

> **scale-from-zero が成立する根拠**: lane consumer を作るのは control-plane であり、
> durable consumer はクライアント接続と独立にサーバ側状態として残る。したがって
> **worker 0 台でも backlog が読める**。M7 までのように worker が consumer を作る構造だと
> 「worker 0 台 → consumer 無し → backlog 読めず → 増やす判断ができない」というブートストラップ・
> デッドロックになる。実機で 0 台のまま `backlog=150 / desired=4` を確認済み。

### 含まない（M9 以降の follow-up）

- **Dockerfile / docker compose の worker サービス**: `--scale worker=N` は per-replica の固定
  ホストポートを公開できず、「何台生きているか」の観測手段を壊す。参照アクチュエータは
  ローカルプロセスの supervisor として実装した（§5.5.2）
- **k8s HPA / KEDA 連携**: `GET /internal/scale` は外部アクチュエータが読める形にしてあるが、
  マニフェストは同梱しない
- **テナント別レイテンシ分布のメトリクス**: `METRICS_INCLUDE_TENANT_LABEL` は M9 へ
- **worker の cwasm 事前 warm**: プロセス初期化に比べ寄与が小さいと見込まれ、
  「どの component を warm すべきか」を worker は知らない。実測で支配的と判明したら M9 で導入

### 運用上の縮退モード（静かに壊れないための設計）

| 状況 | 挙動 | 検出 |
| --- | --- | --- |
| アクティブテナント数 > `MAX_DEDICATED_LANES` | 超過分は overflow lane を共有し、その中では合算の頭打ちが復活する | `faas_lane_*{lane="workers-overflow"}` |
| lane 数 > worker のプロセス容量 | 先頭 N 本だけ提供し `LANE_ROTATION_SECS` ごとに回転（喪失はしない。最大でこの秒数待たされる） | `wasmtime_lanes_unserved > 0` + warn ログ |
| NATS が一時的に見えない | backlog を **0 に落とさず**前回値を保持し、判断ロジックが hold に落ちる | `faas_scale_signal_age_seconds` が伸びる / `/internal/scale` が 503 |
| ドレインが間に合わない | 諦めてプロセス終了。JetStream の再配送が保険として効く（ゲストは 2 回走る） | `wasmtime_drain_abandoned_total` |

---

## M7 スコープ（と非スコープ）

仕様書.md §15 M7 に準拠。**本番運用に耐えるリリース安全性**を与える。

### 含む（M7a: バージョン traffic splitting / canary + 即時 rollback, §6.7）

- canary の配分は **`components` への additive 4 列**（`canary_version_id` / `canary_weight` /
  `previous_active_version_id` / `canary_updated_at`, `migrations/0009`）。**新規テーブルを作らない**
  ことで「新表の GRANT / RLS ポリシー漏れ」という最大の退行リスクを構造的に消している
  （`migration_tests::m7a_creates_no_new_table` が CI で固定）。
- 版の選択は**乱数を使わない**。ルーティングキーの SHA-256 からバケット（0..=99）を決定的に導出し、
  規則は `bucket < canary_weight` の**単一不等式のみ**（端点に分岐を書かない）。これにより
  (a) 同一 `Idempotency-Key` の再送・cron の同一 slot 再発火・chain の再配送が常に同じ版へ落ち、
  (b) 分配比率の正しさを DB 非依存の全列挙ユニットテストで**証明**できる。
- sticky ルーティング用に `X-Faas-Routing-Key` ヘッダを受ける（未指定なら `Idempotency-Key` →
  `execution_id` の順にフォールバックするので既存クライアントは無変更）。
- 解決点は `enqueue::resolve_version_for_enqueue` の**1 箇所だけ**。HTTP invoke / Cron / event /
  chain の全入口がここを通るので、**どの入口も canary をバイパスできない**。
- stable ポインタを動かす 3 操作（`PUT /active-version` / `POST /promote` / `POST /rollback`）は
  **単一 UPDATE で完結**し、**必ず canary をクリア**し、no-op 再 activate では `previous` を
  CASE ガードで保全する（潰すと直後の rollback が「200 を返すのに何も戻らない」最悪の failure mode）。
- `POST /components/{id}/versions` に `activate=false`（既定 true ＝ 従来挙動）。段階移行の出発点。
- `executions.routing_reason` に決定理由を記録し、`GET /components/{id}/traffic` が直近 60 分の
  version 別成績（成功/失敗/timeout・canary_routed・p50/p95）を返す。

### 含む（M7b: per-function 環境変数, §4.4）

- `function_configs`（`migrations/0010`）に**行 = 1 キー**で平文の環境変数を持つ。JSONB 1 列に
  しないのは、`serde_json::Value` 経由で API 応答や `audit_logs.detail` へ丸ごと横流れする経路を
  作らないため。
- **注入面そのものを admin 承認の対象にする**。`component_versions.capabilities` を
  `{"imports": [...], "env": [...]}` の 2 キー構造へ拡張し、`env`（注入を許可する名前のリスト）は
  **admin 専用の `PUT /components/{id}/versions/{version}/capabilities`** でしか書けない。
  `POST /versions`（Deploy スコープ）は常に空で保存し、`capabilities` に `env` が混ざっていたら 400。
- 注入は worker が `set_tenant_guc` 済み tx で DB を直読み（HTTP 往復ゼロ）。
  許可リストに無いキーは**値が DB に存在しても注入しない**。

### 含む（M7c: Secrets Manager, §10）

- **XChaCha20-Poly1305 の封筒暗号**（KEK → 行ごとの DEK → 値）。AAD に
  `(tenant_id, component_id, name)` と `(tenant_id, secret_id, version, kek_kid)` を束縛し、
  DB 書き込み権限を得た攻撃者による**行の貼り替え**と **version の巻き戻し**を遮断する。
- 版台帳（`function_secret_versions`）は**追記専用**（`faas_app` に UPDATE/DELETE を GRANT しない）。
  値の変更も KEK 再ラップも「新しい version 行の INSERT」で表現するので、追記専用のまま rotation が成立する。
- API は **write-only**。`GET /secrets` はメタのみで、`value_len` / `kek_kid` は admin 専用の
  `GET /secrets/keys` へ分離した（`value_len` は**平文長のオラクル**になるため）。
- 注入は **worker が CP の内部専用エンドポイントから引き換える**。worker は KEK を持たない
  （keyless by design, §3.3）ので、平文は「CP のメモリ → HTTP レスポンス → worker のメモリ →
  `WasiCtx`」だけを通り、**NATS にも DB にも S3 にも永続化されない**。
- 引き換えトークン（`env_token`）は `job_token` の**流用ではない**。`job_token` は
  `ResultMessage` / `FailedMessage` にも echo されるため、流用すると result / DLQ / reply の
  購読権しか持たない主体が secret を読めてしまう。別ドメインタグ + `aud="job-env"` の専用トークンにした。
- **世代は execution に固定する**。引き換えは `current_version` ではなく
  `executions.created_at` 以前の最大 version を解決する。そうしないと worker 再配送の間に rotate が
  走ったとき、同一 execution の 1 回目と 2 回目で別の資格情報が外部 API へ到達しうる。
- 引き換え失敗はすべて **fail-closed**（`secret material unavailable` で実行を failed 終端）。
  secret 欠損のまま実行しない。

### 含まない（M8 以降の follow-up）

- **version 別の恒久的な課金集計**。`usage_rollups` の PK に version 次元が無く、粒度も日次なので
  canary 判断には使えない。M7 は `executions` 生表の直近ウィンドウ集計に留める。
- **自動 canary 判定 / 自動 promote / 自動 rollback**（エラー率 SLO によるゲート）。M7 は
  「人間が `GET /traffic` を見て次の重みを PUT する」半自動まで。
- **細粒度スコープ**（`rollout:write` / `secrets:write` 等）の新設。Scope は
  read / invoke / deploy / admin の 4 値固定。→ **canary 運用と secret 管理には admin スコープの
  トークンが要る**（CI/CD の設計に影響するので注意）。
- **`net.allow_outbound` の実効的なネットワーク強制**。`wasi:sockets/` は baseline 非承認のまま。
  M7 の「Capability の outbound 認証情報管理」は**資格情報を secret として保管し、承認された env 名で
  注入するところまで**で、実際に外へ出る経路は存在しない（egress allowlist の実効強制は M9）。
- **KEK 侵害からの復旧としての deep re-encryption**（DEK 再生成 + 平文再暗号化）。M7 は
  wrap-only の rekey のみ（下記「露出ガード」参照）。
- **in-flight ジョブの即時停止 / drain API**。rollback は新規 enqueue のルーティングだけを変える。
- host interface 方式の secret 注入（`faas:secrets/store` 等）。ゲストは baseline 承認済みの
  `wasi:cli/environment` を使う。

### 露出ガード（M7 で新たに増える運用上の MUST）

1. **`SECRETS_MASTER_KEY` を `.env.example` のプレースホルダのまま起動しない**。
   その値のままだと control-plane は**起動に失敗する**（warn ではない）。署名鍵と違い
   **secret の暗号文は DB に永続する**ため、公開リポジトリに載る既知鍵で暗号化すると影響が長く残る。
   実鍵の生成例: `openssl rand -hex 32`。
2. **`INTERNAL_BIND_ADDR`（既定 `127.0.0.1:8081`）を公開しない**。`POST /internal/job-env` だけを
   載せた内部専用 listener であり、認証 middleware の外にある（認証は env-token の署名そのもの）。
3. **`SECRETS_RETIRED_KEYS` を早期に撤去しない**。signing 鍵の overlap は TTL で有限時間に終わるが、
   **secret の暗号文は DB に永続する**。`faas_secret_versions_by_kid{kid="<旧 kid>"}` が 0 になってから
   撤去すること（早期撤去は復号不能 ＝ データ喪失）。
4. **`POST /admin/secrets/rekey` は侵害復旧ではない**。wrap-only の再ラップなので DEK も ciphertext も
   不変であり、旧 KEK + 旧 DB ダンプがあれば再ラップ後も全平文を復元できる。rekey は
   **KEK の計画的ローテーション専用**。侵害時の唯一の復旧経路は**値そのものを rotate すること**。
5. **`GET /components/{id}/config` は平文を返し、deploy スコープで読める**。資格情報は必ず
   secrets 側に置くこと（config 側に入れた瞬間、deploy トークン保持者が読める）。
6. **canary 運用・secret 管理には admin スコープのトークンが必要**（細粒度スコープは非スコープ）。
7. **rollback は新規 enqueue のみに効く**。publish 済みの canary ジョブは最大
   `ACK_WAIT_SECS × MAX_DELIVER`（`BACKOFF_SECS` 併用時はその総和）のあいだ canary 版で完走・再試行する。
8. **`components/echo` は検証用に env を出力する**。本番の Component が注入された env を
   そのまま出力へ返すのは誤り（invoke 応答から secret が読めてしまう）。
9. **低頻度 cron / chain の版混在**: cron の標本数が少ないと canary の統計が意味を持たない。また
   canary 中に rollback すると 1 チェーン内で上流 canary / 下流 stable が混ざる（起動時点解決の帰結）。

> **完了条件（仕様書 §15 M7）**: 「active-version を 10%→100% へ段階移行でき、ワンクリック rollback が
> 効き、secret はログ・監査・他テナントへ漏れない（露出ガード §7 / §15）」。
> `crates/control-plane/tests/chaos_m7.rs` の S1（段階移行 + ワンクリック rollback）/
> S2（env・secret 注入）/ S3（secret 非漏洩）で end-to-end 検証する（chaos_m4/m5/m6 と同じ作法・
> 全て `#[ignore]`・docker compose stack + `CHAOS_TOKEN` 前提）。

---

## M6 スコープ（と非スコープ）

仕様書.md §15 M6 に準拠。非同期 invoke 一択から、商用で要求される起動形態を揃える。M6 は
0（基盤: shared 型 / migration 0008）〜c の段階で実装しました。

含む（M6a: 同期 Invoke, §6.3）:
- `POST /invoke?wait=1` は **JetStream（永続記録）とは別に Core NATS の reply subject + correlation**
  で結果を待ち受け、上限レイテンシ（`SYNC_REPLY_TIMEOUT_MS`）内に `200 OK` + `output` を返す。
  `?wait` 無しの既存 `POST /invoke` は **202 のまま不変**（後方互換）
- reply 先は invoke メッセージの **`reply-to` ヘッダ**で Worker へ伝搬。Axum×N 構成では reply subject に
  **CP instance id**（`INSTANCE_ID`、未設定なら `inst_{uuid}` を採番）を埋め込み、JobMessage を送った
  当該インスタンスだけが reply を購読する（ステートレス前提との整合を設計で解く, §15）
- 同期で受けた結果も **job_token 署名を verify してから**クライアントへ返す（provenance 保全, §3.3）。
  偽 reply / 所有インスタンス障害は 200 にならず **202 へ縮退**。同期 reply は finalize を起こさず、
  計量は依然 result/DLQ subscriber の単一 finalize 経由（二重計上しない）

含む（M6b: Cron スケジュール起動, §11）:
- `cron_jobs`（migration 0008、FORCE RLS）に schedule（`* * * * *`）・component・input を登録する
  トリガー設定 API（`POST /cron-jobs` / `GET /cron-jobs` / `DELETE /cron-jobs/{id}`）
- スケジューラ（`CRON_POLL_INTERVAL_SECS` 間隔で due スキャン）が `next_fire_at` を跨いだ slot を
  invoke へ合流させ、`next_fire_at` を前進させる。**同一スロットは複数 CP / 重複 tick でも 1 度しか
  発火しない**（`cron:{job}:{slot}` を安定な冪等キーにした single-flight, §6.6）

含む（M6c: 外部イベントトリガー, §11）:
- `triggers`（migration 0008、FORCE RLS）に **トリガー登録モデル**（どのイベント源がどの Component を
  起動するか）を持ち、`object_storage`（Object Storage イベント）/ `chain`（Component チェーン）の
  trigger_type を `POST /triggers` / `GET /triggers` / `DELETE /triggers/{id}` で設定
- Object Storage イベント受領（`POST /events/object-storage`）はテナントを **object キーの
  `tenants/{tenant}/...` プレフィックスから導出**し principal と一致を要求（anti-spoof）。同一イベント
  （`{bucket}/{key}/{etag}`）の再送は **`trigger_deliveries` PK + `event_idempotency_key` UNIQUE** の
  二重防御で 1 度だけ enqueue（再送は `enqueued=0`）
- Component チェーンは subscriber の **終端成功フックが CAS 遷移時のみ発火**し、上流 execution_id を
  `event_dedup_id` にした配送台帳で downstream を 1 度だけ起動。イベントペイロード → Component input は
  `input_mapping`（未指定なら event payload 素通し）で変換
- **チェーン暴走の有界化**: chain 下流は毎回新しい execution_id を持つため配送台帳の冪等だけでは
  A→B→A の循環を止められない。各 execution に `chain_depth`（migration 0008、root=0・chain 下流のみ +1）を
  持たせ、上流深さ+1 が `MAX_CHAIN_DEPTH`（既定 8）を超える起動を拒否する（循環チェーンを深さで有界化）

含まない（M8 以降の follow-up）:
- 同期 Invoke の `.result` JetStream 化（現状 reply は Core NATS、CP 再起動で in-flight reply ドロップ）
- Workflow Engine（多段 chain のオーケストレーション / 分岐・合流）・Cron の秒精度 / タイムゾーン指定
- トリガーの admin API 越しの一括管理
（per-function 環境変数・Secrets Manager は **M7 で実装済み**。下の M7 スコープ節を参照）

> **完了条件（仕様書 §15 M6）**: 「HTTP 同期呼び出しが上限レイテンシ内で結果を返し、Cron 登録で定時
> 起動し、トリガー経路でも冪等性・テナント分離・計量（M5）が non-HTTP 起点で破れない」。
> `crates/control-plane/tests/chaos_m6.rs` の S1（`?wait=1` が上限内に 200 + output、`?wait` 無しは
> 202 不変）/ S2（毎分 cron が due slot を 1 度だけ fire し `next_fire_at` を前進）/ S3（object-storage
> イベントの再送は `enqueued=0`、chain は上流成功で 1 度だけ起動）で end-to-end 検証する（chaos_m4/m5 と
> 同じ作法・全 `#[ignore]`・docker compose stack + `CHAOS_TOKEN` 前提）。

---

## M5 スコープ（と非スコープ）

仕様書.md §15 M5 に準拠。テナント別の利用量を冪等に計測・集計し課金可能にする。

含む（per-execution 計量）:
- `executions` に計量5列（`cpu_fuel_used` / `wall_time_ms` / `peak_memory_bytes` / `output_bytes` /
  `invocation_count`。全 nullable で NULL=未計測）を additive 追加（`migrations/0007`）
- worker が `MeteredLimits`(ResourceLimiter) で peak memory、`get_fuel` で fuel 消費、wall time、
  出力バイトを計測し、`ResultMessage.usage`（`faas_shared::UsageMetrics`, serde default で後方互換）で運ぶ

含む（冪等な集計・参照）:
- 終端 writer（subscriber）の **CAS finalize の SET 句に計量列を同梱**し、CAS が遷移させたとき
  だけ同一 tx 内で `usage_rollups`（テナント×UTC日×component 粒度）を UPSERT。再配送 / DLQ後着 /
  sweeper先着は CAS が no-op になり rollup を触らない＝**二重計上が構造的に不可能**（冪等アンカー）
- worker 計測値は信頼境界外としてバージョン上限に clamp。権威 limit を解決できない result は
  invocation のみ計上しリソース指標は記録しない（fail-closed）
- sweeper（reaper）で終端化した実行も rollup に計上（§15「欠落しない」）
- `usage_rollups` は `FORCE ROW LEVEL SECURITY` + fail-closed tenant_isolation、`faas_app` は DELETE 不可（集計改竄防止）
- 利用量参照 API `GET /usage`（read スコープ、`from`/`to` 期間集計、`principal.tenant_id` 権威化で IDOR 面なし）

含まない（M5 の非スコープ。同期 Invoke / 外部イベントトリガー / Cron は M6 で実装済み）:
- クォータ超過の課金的扱い（請求連携）・利用量の per-execution 明細 API

> **完了条件（仕様書 §15 M5）**: 「障害注入（再配送・worker 落下・タイムアウト）下でも計量が
> 二重計上/欠落せず、テナント別に時間窓集計が一致する」。`crates/control-plane/tests/chaos_m5.rs`
> の E1（同一 Idempotency-Key の重複 invoke は +1 のみ）/ E2（distinct N 件はちょうど +N）で
> end-to-end 検証済み（docker compose スタックに対し live 実走で 2/2 pass）。リソース指標は計測済み
> succeeded 実行のみ寄与し、invocation/各 count は全終端で計上される（`GET /usage` のセマンティクス節参照）。

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
  sha256, §6.2）。**M9b (§6.2): 検証は control-plane 本体とは別プロセスで実行**し、悪性 wasm が
  検証器の資源を食い潰しても被害を子プロセス 1 個に限局する（子は Linux の `RLIMIT_AS` +
  wall-clock timeout で縛る）。`VALIDATION_TIMEOUT_SECS` / `VALIDATION_MEM_LIMIT_MB`
- **署名付き Component（M9a, §6.2 / §15 M9）**: テナントが登録した Ed25519 公開鍵で wasm 本体の
  sha256 への detached 署名を検証する。`require_signed_components=true` のテナントは署名必須
  （deploy トークンが漏れても署名鍵無しでは active にできない = 供給網汚染の防御）。既定 false で
  従来どおり（署名が有れば検証、無ければ通す）。鍵登録・ポリシー切替は admin 専用
  （`PUT /admin/signing-keys/{key_id}` / `PUT /admin/signing-policy`）。**秘密鍵は渡さない**。
- **egress allowlist（M9c, §4.4 / §15 M9）**: admin 承認した `host:port` へのみ outbound を許す。
  承認名が内部 IP（`169.254.169.254` 等）へ解決されても SSRF ハードデニーが優先して拒否する。
  `PUT /components/{id}/versions/{version}/capabilities/egress`
- Object Storage: MinIO への本体保存と Worker への presigned GET URL（§3.4）
- Worker: `wasm_sha256` キーの Component キャッシュ + 事前コンパイル（cwasm, §3.6）
- バージョン管理（§6.7）: 一覧 / soft delete / `active-version` 切替（ロールバック）
- NATS JetStream の invoke ストリーム + 共有 Pull Consumer、result 購読・永続化
- リソース制限: `max_memory`（StoreLimits）+ `max_wall_time`（epoch interruption）

含まない（M5 以降, §10 / §14）:
- **検証パイプラインの隔離プロセス化**（§6.2 MUST。現状はインプロセス）
- **大出力 offload**: DB 列・キーレイアウト（`io/{execution_id}/output`）は敷設済みだが、
  Worker 側の write offload は最小実装（後続スライスの TODO, §3.4）
- 将来⬜: OIDC 連携ログイン、Workflow Engine、Secrets Manager、分散トレーシング、AI 統合、
  Multi Region、Result Ingestor 分離（§10 / §14）。**同期 Invoke / 外部イベントトリガー / Cron は M6 で実装済み**

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
| `INSTANCE_ID` | （未設定なら `inst_{uuid}` を採番） | **M6a**: この CP インスタンスの subject-safe 識別子。同期 invoke の reply subject `reply.{instance_id}.{correlation_id}` に埋め込み、JobMessage を送った当該インスタンスだけが reply を購読する（ステートレス×N の鍵, §6.3）。Axum×N で値が衝突しないよう各インスタンスで一意にすること（未設定なら自動採番で衝突しない） |
| `SYNC_REPLY_TIMEOUT_MS` | `5000` | **M6a**: 同期 invoke（`POST /invoke?wait=1`）の reply 待機上限（ミリ秒）。超過でクライアントへ **202 + execution_id へフォールバック**（縮退）。HTTP 同期呼び出しの上限レイテンシ（§15 M6 完了条件） |
| `CRON_POLL_INTERVAL_SECS` | `10` | **M6b**: Cron スケジューラの due スキャン間隔（秒）。`cron_jobs.next_fire_at` を跨いだ slot を invoke へ合流させる。短いほど発火遅延が縮むが DB スキャン頻度が上がる（§11） |

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

> **M7 の新規 env（§10 / §15）**:
> `SECRETS_MASTER_KEY`（**必須**・32 バイト KEK。`.env.example` のプレースホルダのままだと**起動失敗**）/
> `SECRETS_MASTER_KID`（**必須**・新規暗号化に使う kid）/ `SECRETS_RETIRED_KEYS`（`kid:key,...` の CSV。
> 復号専用の旧 KEK。再ラップ完了まで残す）/ `INTERNAL_BIND_ADDR`（既定 `127.0.0.1:8081`。
> `POST /internal/job-env` だけを載せる内部専用 listener。**公開しないこと**）/
> `JOB_ENV_EXCHANGE_RATE_PER_MIN`（既定 600）は **control-plane** が読みます。
> `CONTROL_PLANE_INTERNAL_URL`（既定 `http://127.0.0.1:8081`）/ `JOB_ENV_FETCH_TIMEOUT_MS`（既定 2000）は
> **worker** が読みます（引き換え先とタイムアウト。超過は fail-closed で実行を failed 終端）。

> **M8 の新規 env（§3.3 / §4 / §5 / §15）** — **control-plane** が読むもの:
> `TENANT_LANES_ENABLED`（既定 false。テナント別 lane の有効化。**ロールバックはこの 1 個を false に
> 戻すだけで完結**する）/ `MAX_DEDICATED_LANES`（既定 64。超過分は overflow lane へ）/
> `LANE_ACK_PENDING_HEADROOM`（既定 8）/ `LANE_OVERFLOW_ACK_PENDING`（既定 1000。**所属テナント数に
> 比例させない**）/ `LANE_RECONCILE_INTERVAL_SECS`（既定 10）/ `BACKOFF_SECS`（既定 `5,15,60`。
> **M8 で worker から CP へ所有移管**。consumer を作るのが CP になったため）/
> `SCALE_POLL_INTERVAL_SECS`（既定 **0 = ポーラを spawn しない**。`.env.example` は 5）/
> `SCALE_JOBS_PER_WORKER`（既定 32）/ `SCALE_MIN_WORKERS`（既定 1。0 で scale-to-zero を許可）/
> `SCALE_MAX_WORKERS`（既定 4）/ `SCALE_IN_COOLDOWN_SECS`（既定 60）/ `SCALE_SIGNAL_STALE_SECS`（既定 30）/
> `METRICS_LANE_LABELS`（既定 true。`faas_lane_*` の `lane` ラベルを付けるか。false で `"aggregate"`
> 1 値に畳む。系列数は `min(テナント数, MAX_DEDICATED_LANES) + 1` で有界だが、**テナント数と
> 一緒に伸びる軸**ではあるので逃げ道を用意してある）。
>
> **worker** が読むもの: `WORKER_MAX_CONCURRENCY`（既定 32。**プロセス全体**の同時実行上限）/
> `WORKER_LANE_CONCURRENCY`（既定 4。consumer metadata が読めないときのフォールバック）/
> `LANE_DISCOVERY_INTERVAL_SECS`（既定 10）/ `LANE_ROTATION_SECS`（既定 30）/
> `WORKER_DRAIN_TIMEOUT_SECS`（既定 **0 = ハンドラを入れない = M7 までと完全に同一挙動**）/
> `WORKER_SLOT`（supervisor が注入。`wasmtime_worker_slot` gauge に出る）。
>
> **【不変条件】`WORKER_DRAIN_TIMEOUT_SECS < ACK_WAIT_SECS`**（既定 30）。超えると、まだ実行中の
> ジョブを JetStream が再配送し、**ドレインしているのに二重実行**になります（目的が裏返る）。
> worker は起動時にこれを検査して fail-fast します。
>
> **`SCALE_JOBS_PER_WORKER` は `WORKER_MAX_CONCURRENCY` と揃えてください。** control-plane は
> worker の env を知らないので検査できません（既定を一致させてあります）。
>
> **`SCALE_MIN_WORKERS` の既定を 0 にしていない理由**: M6 の同期 invoke はサーバ側
> `SYNC_REPLY_TIMEOUT_MS`（既定 5000）で打ち切られます。from-zero の coldstart はこの窓を容易に
> 超えるため、既定 0 は **M6 の完了条件を壊します**。scale-to-zero は opt-in です。

> **M3c 結合の注意**: `JOB_SIGNING_KEY` / `JOB_SIGNING_KID` は **control-plane のみ**が読みます
> （worker は鍵を持たず、不透明トークンを echo するだけ）。`ACK_WAIT_SECS` / `MAX_DELIVER` は
> control-plane（token exp）と worker（最終配送試行の判定）が読みます。
> **M8 で consumer の作成者は control-plane 単独になった**ため、consumer 設定に効くのは CP 側の値
> だけです（worker は `MAX_DELIVER` を「今回が最終試行か」の判定にのみ使い、`ACK_WAIT_SECS` は
> ドレイン上限の健全性検査にのみ使います）。値をずらすと正規の遅延結果がトークン失効扱いに
> なりえます（subscriber は「失効しても行が pending/running なら受理」する安全網を持ちますが、
> 定数は揃えてください）。consumer の drift は CP の reconciler が毎周期是正するので、
> 設定変更後に consumer を手で作り直す必要はありません（M7 まではその必要がありました）。

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
`0006_large_io.sql` → `0007_usage_metering.sql` → `0008_m6.sql`）。control-plane 起動時にも埋め込み sqlx migrator が pending を冪等適用
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
make run-worker         # 1 台だけ（開発・デバッグ用。メトリクスは 9090）
# または
make run-workers N=3    # M8: 固定 N 台（メトリクスは 9101 起点）
make autoscale          # M8: GET /internal/scale を見て自動増減
```

worker は JobMessage の presigned GET URL から本体を取得し、`WASM_CACHE_DIR` に cwasm を
キャッシュします（M1 のように `COMPONENTS_DIR` から直接 wasm を読むことはありません）。

**M8 以降、worker は consumer を作りません。** 作成者は control-plane 単独です（§3.7）。
したがって **control-plane を先に起動してください**。worker は NATS の consumer 一覧から
購読先を発見します（`wasmtime_subscribed_lanes` が 0 のまま張り付いていたら、CP が lane を
作っていないか discovery が失敗し続けています）。

> **`make run-worker` と `make run-workers` を混ぜないこと。** 前者は 9090、後者は 9101 起点を
> 使いますが、worker は metrics サーバの bind に失敗しても warn するだけで**プロセスは無言で
> 稼働を続けます**。混ぜると「何台生きているか」の観測が信用できなくなります。

### 5-b. 既存スタックからの移行（M8-1: stream の WorkQueue 化）

M8 より前に作られた `FAAS_INVOKE` stream は `Limits` retention のままです。M8 の
control-plane は起動時に retention を検査して **fail-fast** するので、一度だけ作り直します。

```bash
# CP / worker を止めてから実行する。未消化 0 でなければコマンド自身が拒否します。
make stop-workers          # または run-worker を止める
make recreate-stream
# 未消化が残っていて意図的に捨てる場合のみ:
# make recreate-stream FORCE_ARG=--force
```

WorkQueue にする理由は 4 つあり、どれか 1 つでも単独で移行を正当化します:
(1) 「どの subject もちょうど 1 consumer に属する」をサーバに強制させる（filter の重なりによる
二重配送を、レビューではなく NATS に検出させる）/ (2) consumer の delete→recreate が
**stream 全履歴の再配送**にならない / (3) ack 済みメッセージが消えるので backlog シグナルが
素直に効く / (4) 保持期間の設定ミスが他テナントのメッセージを巻き込まない。

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
# テストは「オペレータが手で worker を落とす」前提で書かれているが、**worker を落とした状態で
# 開始すれば自動化できる**（ジョブは JetStream に滞留し、pending 行を sweeper が終端化する）。
pkill -f 'target/debug/faas-worker'     # 先に worker を落としておく
pkill -f 'target/debug/control-plane'
set -a; source .env; set +a
STUCK_EXECUTION_DEADLINE_SECS=20 REAPER_INTERVAL_SECS=5 \
  ./target/debug/control-plane > /tmp/cp.log 2>&1 &
sleep 15
export CHAOS_TOKEN=$(make -s login | tail -1)
CHAOS_WAIT_SECS=45 \
  cargo test -p faas-control-plane --test chaos_m4 \
  -- --ignored chaos_a_ --nocapture
# 終わったら通常設定（既定 deadline 900s）で CP と worker を起動し直すこと。

# Scenario C, D — 専用 component (always-trap / slow) を deploy してから実行
# `make deploy-chaos-components` がビルド + component 作成 + アップロードまでを冪等に行う。
# slow は SLOW_LIMITS（既定 max_wall_time_ms=1000 / max_execution_time_ms=2000）で上げる ——
# 既定の 1s / 5s のままだと tokio timeout がテストの待ち窓（既定 10 秒）に収まらないことがある。
make deploy-chaos-components
make run-worker > /tmp/worker.log 2>&1 &
CHAOS_ALWAYS_TRAP=always-trap CHAOS_SLOW=slow \
  cargo test -p faas-control-plane --test chaos_m4 \
  -- --ignored chaos_c_ chaos_d_ --test-threads=1 --nocapture
```

> **`make -n` で syntax check しないこと**: `deploy-chaos-components` を含む一部のターゲットは
> レシピ内で `$(MAKE) -s login` を呼ぶ。make は `$(MAKE)` を含む行を「再帰 make」とみなし
> **`-n` 指定でも実際に実行する**（POSIX）。しかも `-n` が子 make へ伝播してトークンが取れず失敗する。
> 動作確認は実行して行うこと。

期待される最終 status:
- chaos_a: `status=failed`（sweeper, error.message に sweeper 由来文言）
- chaos_b: `status=succeeded`（同一 execution_id を 2 度受領、wasm は 1 度だけ実行）
- chaos_c: `status=failed`（trap → result subscriber または DLQ subscriber）
- chaos_d: `status=timeout`（epoch interruption → `Trap::Interrupt` → tokio timeout 経由）

### Chaos / M6 起動形態テスト（M6 完了条件の検証）

仕様書 §15 M6 完了条件「HTTP 同期呼び出しが上限レイテンシ内で結果を返し、Cron 登録で定時起動し、
トリガー経路でも冪等性・テナント分離・計量（M5）が non-HTTP 起点で破れない」を
`crates/control-plane/tests/chaos_m6.rs` の 3 シナリオ（S1: 同期 invoke / S2: Cron / S3: トリガー）で
end-to-end 検証します。chaos_m4/m5 と同じ作法で、全シナリオが `#[ignore]`・docker stack +
`CHAOS_TOKEN` + echo デプロイ済みを前提とします（公開 API だけを使うブラックボックステスト）。

> **前提（M4/M5 と違い M6 は worker 稼働が必須）**: S1〜S3 は実行が **succeeded まで進む**ことを
> 前提にする（S1 は worker の reply で 200、S2/S3 は計量まで確認）。よって CP に加えて
> **worker を 1 つだけ**起動しておくこと。worker が止まっていると S1 は reply が来ず常に 202 へ縮退して
> 落ちる。逆に **古い worker が複数残っている**と JetStream の共有 Pull Consumer がジョブを分け合い、
> reply 非対応の旧 worker が引いた回だけ 202 になって S1 がフレーク化する（`pgrep -fl faas-worker` で
> 1 個だけか確認する）。

```bash
# 共通: docker stack を起動して bootstrap + token + echo + worker を用意
make up && make migrate && make bootstrap
export CHAOS_TOKEN=$(make -s login | tail -1)
make build-component && make deploy
make run-worker > /tmp/worker.log 2>&1 &   # ← worker を 1 つだけ起動（M6 では必須）

# S1 — 同期 Invoke: ?wait=1 が上限レイテンシ内に 200 + output、?wait 無しは 202 不変
cargo test -p faas-control-plane --test chaos_m6 \
  -- --ignored chaos_s1_sync_invoke_returns_200_within_timeout --nocapture

# S2 — Cron: 毎分 cron を登録 → due slot を 1 度だけ fire し next_fire_at が前進（最大 ~100 秒待つ）
cargo test -p faas-control-plane --test chaos_m6 \
  -- --ignored chaos_s2_cron_fires_once_per_slot --nocapture

# S3 — トリガー: object-storage イベント再送は enqueued=0（冪等）/ chain は上流成功で 1 度だけ起動
# object キーのテナントは principal と一致が必須（anti-spoof）。既定キーは placeholder のため、
# 実テナント ID を CHAOS_OBJECT_KEY で渡す（未指定だと 403 で落ちる）。
TA=$(docker compose exec -T postgres psql -U faas -d faas -tA -c "SELECT id FROM tenants WHERE slug='smoke';")
CHAOS_OBJECT_KEY="tenants/$TA/in/chaos-s3.txt" CHAOS_BUCKET=uploads \
  cargo test -p faas-control-plane --test chaos_m6 \
  -- --ignored chaos_s3_trigger_idempotent_and_chain_fires_once --nocapture
```

手で起動形態を叩く場合の最小手順:

```bash
# 同期 Invoke（?wait=1 で結果まで待つ。SYNC_REPLY_TIMEOUT_MS 内に 200 が返る）
curl -s -X POST "http://localhost:8080/invoke?wait=1" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"component":"echo","input":{"hello":"sync"}}'
# => 200 {"execution_id":"exec_xxx","status":"succeeded","output":{...}}（縮退時は 202 + pending）

# Cron 登録（毎分起動。CRON_POLL_INTERVAL_SECS 間隔で due slot を fire）
curl -s -X POST http://localhost:8080/cron-jobs \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"component":"echo","schedule":"* * * * *","input":{"hello":"cron"}}'
# => 201 {"cron_job_id":"...","next_fire_at":"..."}

# トリガー登録（object_storage イベントで echo を起動）
curl -s -X POST http://localhost:8080/triggers \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"component":"echo","trigger_type":"object_storage","match_config":{"bucket_prefix":"tenants/"}}'
# => 201 {"trigger_id":"..."}
```

---

### Chaos / M9 サンドボックス強化テスト（M9 完了条件の検証・レッドチーム）

`crates/control-plane/tests/chaos_m9.rs` の 6 シナリオで、悪意ある wasm が各層で弾かれることを
end-to-end 検証します（**各シナリオに負の対照** ——「正規操作は成功する」——を付け、「全部拒否」を
安全と誤読しない）。

| シナリオ | 脅威 | 検証 |
| --- | --- | --- |
| `chaos_v1_filesystem_denied_at_runtime` | T1 | fs は import できても実行時に全パス拒否（preopen 無し） |
| `chaos_v3_unapproved_egress_denied` | T3 | egress 未承認は一切 outbound 不可 |
| `chaos_v4_approved_egress_only_to_allowlist` | T3 | 承認先には到達でき（要外向き網）、allowlist 外は拒否 |
| `chaos_v5_internal_target_denied_even_if_approved` | T4 | 承認名が内部 IP を指しても hard-deny が優先して拒否 |
| `chaos_v6_validation_dos_does_not_kill_cp` | T5 | 検証 DoS を浴びても CP が `/readyz`=200 を保つ |
| `chaos_v7_unsigned_or_bad_signature_rejected` | T6 | 署名必須下で署名なし/不正署名を拒否、正しい署名は通る |

```sh
docker compose up -d && make migrate
make run-cp > /tmp/cp.log 2>&1 &
make bootstrap && export CHAOS_TOKEN=$(make -s login)
make build-component && make deploy          # 正常 echo を用意
make deploy-chaos-components                  # netprobe（egress 検証用）を用意
make run-workers N=1                          # v3/v4/v5 は worker が要る
cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1
```

> **v4 は外向きネットワーク**（`CHAOS_M9_EGRESS_TARGET`、既定 example.com:443 への TCP）を要します。
> 全て `#[ignore]` + `--test-threads=1`（検証セマフォ / 子プロセス / worker という共有資源を飽和させるため）。

---

### Chaos / M8 弾力スケール・アイソレーションテスト（M8 完了条件の検証）

`crates/control-plane/tests/chaos_m8.rs` の 4 シナリオ（U1: バーストが他テナントのクォータを
劣化させない / U2: キュー待ちが backlog に比例しない / U3: admission バイパス経路でも分離が効く
+ M6 回帰 / U4: worker が自動増減する）で end-to-end 検証します。

> **`--test-threads=1` で実行すること**: 4 シナリオは共有 JetStream stream の depth、worker の
> 実行スロット、worker プロセス台数という**プロセス外の共有状態**を飽和させます。並行実行すると
> 互いの backlog を測ってしまい、どの assert も意味を失います。通しで約 3 分です。

#### 前提（満たさないと skip ではなく panic します）

最も見落としやすいのは **admission ゲート 2** です。A の in-flight は
`max_concurrent_executions`（既定 20）で頭打ちになるため、既定のまま 300 件投げても
**A の実 execution は 20〜40 件にしかならず**、「A の後ろに B が並ぶ」状況が再現しません。
その状態では C4 の閾値が修正前でも自明に成立し、**テストが vacuous に通ります**。

さらに上限は `CHAOS_M8_BURST` より**大きく**必要です（同時投入なので上限が BURST 以下だと
超過分がその場で 429 になる）。既定 BURST=300 に対し 400 を推奨します。

```bash
make up && make migrate

# control-plane は M8 の前提 env で起動する
QUOTA_MAX_CONCURRENT_EXECUTIONS=400 TENANT_LANES_ENABLED=true SCALE_POLL_INTERVAL_SECS=5 \
  make run-cp > /tmp/cp.log 2>&1 &

# テナント A / B（U3 の M6 回帰ガードを有効にするなら C も）
make bootstrap
export CHAOS_TOKEN=$(make -s login)
make bootstrap SMOKE_TENANT_SLUG=chaos-b SMOKE_EMAIL=b@example.com
export CHAOS_TOKEN_B=$(make -s login SMOKE_TENANT_SLUG=chaos-b SMOKE_EMAIL=b@example.com)

# burn を各テナントへ（「N ミリ秒かかって succeeded で終端する」ノブ）
make deploy-chaos-components

# U1〜U3 は worker 1 台
make run-workers N=1
cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1 chaos_u1_ chaos_u2_ chaos_u3_

# U4 はアクチュエータを回した状態で（手動 worker が 9090 に居ないこと）
make stop-workers
make autoscale > /tmp/autoscale.log 2>&1 &
cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1 chaos_u4_
```

#### 「修正前は red、修正後は green」の対比（実測値）

**修正前から緑になる assert は完了条件の証拠になりません。** `TENANT_LANES_ENABLED=false`
（= M8 適用前と同じ共有 consumer 1 本のトポロジ）で実際に回した結果:

| assert | lanes=true | lanes=false | 判定 |
| --- | --- | --- | --- |
| **U2 / C3**（B のキュー待ち上限） | green | **red** | **これが M8 の証拠** |
| U1 / C2-a（B の 429 が 0） | green | green | 2 テナントでは識別力なし |

lanes=false での U2 の実測:

```text
B のキュー待ち最大値が 40689ms で、上限 5000ms を超えました
（同時刻の A の最大値は 40010ms）。
```

B の待ち時間が A のそれと**ほぼ同一**になっています。これは「B が A の backlog の後ろに
丸ごと並んだ」ことの直接観測であり、M8 が解く問題そのものです。

再現手順:

```bash
# CP を lanes 無効で起動し直す（reconciler が全 lane を消して legacy consumer へ収束する）
QUOTA_MAX_CONCURRENT_EXECUTIONS=400 TENANT_LANES_ENABLED=false SCALE_POLL_INTERVAL_SECS=5 \
  make run-cp > /tmp/cp_legacy.log 2>&1 &
make stop-workers && make run-workers N=1
cargo test -p faas-control-plane --test chaos_m8 -- --ignored chaos_u2_   # ← 落ちる
```

> C2-a が 2 テナントで発火しない理由: legacy 共有 consumer の合算 `max_ack_pending`（1000）を
> 溢れさせる必要があり、`Σ(テナント数 × max_concurrent_executions) > 1000` が要ります。
> 2 × 400 = 800 では届きません（既定クォータ 20 なら 50 テナント必要）。
> **C2-a は「劣化していないことの回帰ガード」であって、分離の証明ではありません。**

#### 黒箱で検証できない部分は手で確認する

```bash
# 1) lane トポロジの目視 —— 「分離が配送層で効いている」ことの最も直接的な証拠。
#    バースト中に A の lane だけ num_pending が大きく、B の lane が 0 に張り付くこと。
curl -s localhost:8222/jsz?consumers=1 \
  | jq '.account_details[].stream_detail[].consumer_detail[] | {name, num_pending, num_ack_pending}'

# 2) 不変条件 I1 の目視 —— 同じ subject が 2 つの consumer に現れないこと。
curl -s localhost:8222/jsz?consumers=1 \
  | jq -r '.account_details[].stream_detail[].consumer_detail[] | .config.filter_subject // (.config.filter_subjects[]?)' \
  | sort | uniq -d      # ← 何も出力されないこと

# 3) retention の目視 —— workqueue であること（起動時 fail-fast の二重確認）。
curl -s 'localhost:8222/jsz?streams=true' \
  | jq '.account_details[].stream_detail[] | {name, retention: .config.retention}'

# 4) テナント別キュー待ちの参考値（assert はしない）。
make psql <<'SQL'
SELECT tenant_id,
       percentile_cont(0.99) WITHIN GROUP (
         ORDER BY EXTRACT(EPOCH FROM (started_at - created_at)) * 1000) AS p99_queue_wait_ms
FROM executions
WHERE created_at >= now() - interval '10 minutes' AND started_at IS NOT NULL
GROUP BY tenant_id;
SQL
```

#### 運用（弾力スケール）

```bash
make run-workers N=3    # 固定 3 台（負荷試験・手動デバッグ）
make autoscale          # /internal/scale を見て自動増減
make stop-workers       # 全台ドレイン停止
make scale-status       # 現在の backlog / desired
make lane-status        # lane ごとの未消化件数
```

> **worker のメトリクスポートは 9101 起点**です（`make run-worker` の 9090 と分離してあります）。
> 分離の理由: worker は metrics サーバの bind に失敗しても warn するだけで**プロセスは無言で
> 稼働を続ける**ため、ポートが重なると「supervisor は 1 台も起動していないのに live 判定が 1 になる」
> という最も気づきにくい混入が起きます。supervisor は `/readyz` の 200 では満足せず、
> `wasmtime_worker_slot` が期待 slot と一致することまで確認します。

### Chaos / M7 デプロイ運用テスト（M7 完了条件の検証）

仕様書 §15 M7 完了条件「active-version を 10%→100% へ段階移行でき、ワンクリック rollback が効き、
secret はログ・監査・他テナントへ漏れない」を `crates/control-plane/tests/chaos_m7.rs` の 3 シナリオ
（S1: canary 段階移行 + rollback / S2: env・secret 注入 / S3: secret 非漏洩）で end-to-end 検証します。

> **`--test-threads=1` で実行すること**: 3 シナリオは同じ component の traffic 配分・config・secret を
> 書き換えるため、並行実行すると互いの状態を壊します（chaos_m5 の会計テストが直列を要求するのと同型）。
> S1 は約 700 回の invoke を終端まで待つため 10 分前後かかります。

```bash
# 共通: docker stack + bootstrap + token + echo + worker（M6 と同じ前提）
make up && make migrate && make bootstrap
export CHAOS_TOKEN=$(make -s login | tail -1)
make build-component && make deploy
make run-worker > /tmp/worker.log 2>&1 &   # ← worker は 1 つだけ（pgrep -fl faas-worker で確認）

# 3 シナリオを直列実行
cargo test -p faas-control-plane --test chaos_m7 -- --ignored --test-threads=1 --nocapture

# S1 だけ（canary の端点・単調性・sticky・削除保護・rollback の保証境界・ワンクリック復帰）
cargo test -p faas-control-plane --test chaos_m7 \
  -- --ignored chaos_t1_ --nocapture
```

**黒箱で検証できない部分は手で確認する**（CP / worker のログと `audit_logs` には参照 API が無いため。
テスト内から `docker compose` / `psql` を呼ぶのは環境依存が強すぎるので採らない）:

```bash
# 1) sentinel 値を持つ secret を置いて invoke する
SENTINEL="sentinel-$(date +%s)-DO-NOT-LEAK"
CID=$(curl -s http://localhost:8080/components -H "Authorization: Bearer $TOKEN" \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["component_id"])')
VER=$(curl -s "http://localhost:8080/components/$CID/traffic" -H "Authorization: Bearer $TOKEN" \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["stable"]["version"])')
curl -s -X PUT "http://localhost:8080/components/$CID/secrets/API_KEY" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d "{\"value\":\"$SENTINEL\"}"
curl -s -X PUT "http://localhost:8080/components/$CID/versions/$VER/capabilities" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"env":["API_KEY"]}'
curl -s -X POST "http://localhost:8080/invoke?wait=1" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"component":"echo","input":{}}' > /dev/null

# 2) ログに sentinel が出ていないこと（CP / worker とも 0 であること）
#    control-plane / worker は compose のサービスではなくホストのプロセスなので、
#    `docker compose logs` では**何も取れない**（M8 で誤りに気づいて修正）。
#    それぞれのリダイレクト先を直接見ること。
grep -c "$SENTINEL" /tmp/worker.log          # => 0   (make run-worker のリダイレクト先)
grep -c "$SENTINEL" /tmp/cp.log              # => 0   (make run-cp のリダイレクト先)
grep -c "$SENTINEL" .workers/worker-*.log    # => 全て 0 (make run-workers / autoscale 使用時)

# 3) 監査ログに値が入っていないこと（names と count だけが残る）
docker compose exec -T postgres psql -U faas -d faas -c \
  "SELECT set_config('app.tenant_id','<tenant_id>',false);" -tAc \
  "SELECT count(*) FROM audit_logs WHERE detail::text LIKE '%$SENTINEL%'"    # => 0
```

> **`executions.output` には sentinel が現れる**（`components/echo` が**検証用に**注入された env を
> 出力へ返す仕様のため）。これはゲスト自身の責務であり、プラットフォームの漏洩ではない。
> **本番の Component が注入された env をそのまま返すのは誤り**である。

**KEK ローテーションの手順**（露出ガード 3 / 4 を必ず併読すること）:

```bash
# 1) 新 KEK を生成し、旧 KEK を retired へ移して CP を再起動する
#    （この時点で新規書き込みは新 kid、既存は旧 kid で復号可能）
openssl rand -hex 32                       # => 新 KEK
#    .env: SECRETS_MASTER_KEY=<新 KEK> / SECRETS_MASTER_KID=k2 / SECRETS_RETIRED_KEYS=k1:<旧 KEK>

# 2) 当該テナントの secret を現行 KEK で再ラップする
curl -s -X POST http://localhost:8080/admin/secrets/rekey -H "Authorization: Bearer $TOKEN"
# => {"rewrapped": N}

# 3) 旧 kid の gauge が 0 になったことを確認してから retired を撤去する
curl -s http://localhost:8080/metrics | grep faas_secret_versions_by_kid
# => faas_secret_versions_by_kid{kid="k2"} N   （k1 が消えていれば撤去してよい）
#    .env: SECRETS_RETIRED_KEYS=  にして CP 再起動
```

> **早期撤去は復号不能 ＝ データ喪失**。signing 鍵の overlap は TTL で有限時間に終わるが、
> **secret の暗号文は DB に永続する**（§4.7.2）。
> また **rekey は侵害復旧ではない**（wrap-only なので DEK も ciphertext も不変）。
> KEK が漏れたときの唯一の復旧経路は **値そのものを rotate すること**。

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
| `GET /usage` | read | **M5**: テナント利用量参照。`from`/`to`（`YYYY-MM-DD`・UTC・既定は直近30日）で期間集計を返す。`principal.tenant_id` を権威化（cross-tenant パスなし）, §15 |
| `POST /invoke?wait=1` | invoke | **M6a**: 同期 Invoke。reply subject + correlation で結果を待ち、上限レイテンシ（`SYNC_REPLY_TIMEOUT_MS`）内に 200 + output。縮退時は 202 へ（§6.3） |
| `POST /cron-jobs` | deploy | **M6b**: Cron トリガー登録（`schedule` = `* * * * *`、component、input）。201 + `next_fire_at`。component ライフサイクル相当の **deploy** スコープ（§11） |
| `GET /cron-jobs` | read | **M6b**: 自テナントの Cron 登録一覧（`next_fire_at` 含む, §11） |
| `DELETE /cron-jobs/{id}` | admin | **M6b**: Cron 登録の削除（自テナント限定。他テナントは 404）。他 DELETE と整合の **admin** スコープ（§11） |
| `POST /triggers` | deploy | **M6c**: 外部イベントトリガー登録（`trigger_type` = `object_storage` / `chain`、`match_config`、任意 `input_mapping`）。201 + `trigger_id`。component ライフサイクル相当の **deploy** スコープ（§11） |
| `GET /triggers` | read | **M6c**: 自テナントのトリガー登録一覧（§11） |
| `DELETE /triggers/{id}` | admin | **M6c**: トリガー登録の削除（自テナント限定。他テナントは 404）。他 DELETE と整合の **admin** スコープ（§11） |
| `POST /events/object-storage` | invoke | **M6c**: Object Storage イベント受領。テナントは object キーの `tenants/{tenant}/...` から導出し principal と一致を要求（anti-spoof）。同一 `{bucket}/{key}/{etag}` 再送は `enqueued=0`（冪等, §6.6 / §11） |
| `PUT /components/{id}/traffic` | admin | **M7a**: canary の版と重みを設定（絶対値・冪等）。`{"canary_version":"0.2.0","weight":10}`。weight 範囲外 400 / 未知 version 404 / stable と同一 400（§6.7） |
| `DELETE /components/{id}/traffic` | admin | **M7a**: canary を解除（`weight=0` + ポインタ NULL）。既に未設定でも 204（冪等, §6.7） |
| `GET /components/{id}/traffic` | read | **M7a**: 現在の配分 + 直近 60 分の version 別成績（成功/失敗/timeout・`canary_routed`・p50/p95）。canary の go/no-go 判断の一次情報（§6.7） |
| `POST /components/{id}/promote` | admin | **M7a**: canary を stable へ昇格（単一 UPDATE + CAS）。canary 未設定 400 / CAS 不一致 409（§6.7） |
| `POST /components/{id}/rollback` | admin | **M7a**: ワンクリック rollback。body `{}` で直前 stable へ戻し canary をクリア。previous 不在 409 / 戻り先が削除済み 409（§6.7） |
| `PUT /components/{id}/versions/{ver}/capabilities` | admin | **M7b**: 注入を許可する env 名を承認（全置換）。**deploy 経路からは書けない**（§4.4 の「付与は admin スコープを要する」MUST） |
| `GET /components/{id}/config` | **deploy** | **M7b**: per-function の平文環境変数を返す。read に置かないのは、config が平文で secret と同じ env 名前空間に混ざるため（§15） |
| `PUT /components/{id}/config` | deploy | **M7b**: 平文環境変数の全置換。キー名 `^[A-Z_][A-Z0-9_]{0,63}$` / 値 4096 バイト / 64 キー / 総 32 KiB を超えると 400（§15） |
| `DELETE /components/{id}/config/{key}` | deploy | **M7b**: 1 キー削除。不在は 404（§15） |
| `GET /components/{id}/secrets` | read | **M7c**: secret の**メタデータのみ**（`name` / `version` / `has_value` / `updated_at`）。値も `value_len` も `kek_kid` も返さない（§10） |
| `GET /components/{id}/secrets/keys` | admin | **M7c**: 運用向け。`kek_kid` / `value_len` は**ここだけ**（`value_len` は平文長のオラクルなので read から分離, §10） |
| `PUT /components/{id}/secrets/{name}` | admin | **M7c**: 値を設定（新規 201 + version=1 / 既存 200 + 新 version）。config と同名なら 409（§10） |
| `POST /components/{id}/secrets/{name}/rotate` | admin | **M7c**: 値を差し替え（監査 action を `secret_rotated` に分ける）。不在は 404（§10） |
| `DELETE /components/{id}/secrets/{name}` | admin | **M7c**: soft delete（204）。版台帳は追記専用なので残る。**同名で作り直せる**（部分 UNIQUE index, §10） |
| `POST /admin/secrets/rekey` | admin | **M7c**: 当該テナントの secret を現行 KEK で再ラップ（`{"rewrapped": n}`）。**侵害復旧ではない**（露出ガード 4 を参照, §10） |
| `POST /internal/job-env` | 内部専用 | **M7c**: worker が `env_token` と引き換えに復号済み env を受け取る。**`INTERNAL_BIND_ADDR` にしか生えない**（公開 listener では 404）。認証は env-token の署名そのもの（§4.6） |
| `GET /internal/scale` | 内部専用 | **M8**: 現在の backlog と desired worker 数。**未観測 / 陳腐化 / ポーラ無効のときは 503 + `{"error":"scaler_unavailable"}`** で `desired` を出さない（`desired: 0` を返すとアクチュエータが全台停止させるため、「値が取れない」と「0」を HTTP のレイヤで分ける）。**無認証だが応答にテナントデータを含まない**ことが根拠であり、これは維持すべき不変条件（§5.4） |

> **`GET /usage` の集計セマンティクス（課金解釈の明示, M5）**: `invocation_count` と
> `succeeded_count`/`failed_count`/`timeout_count` は **全終端実行**で +1 される（worker 落下を
> sweeper が `failed` 終端化したぶんも含む）。一方リソース指標（`cpu_fuel_used`/`wall_time_ms`/
> `output_bytes` は SUM、`peak_memory_bytes_max` は MAX）は **計測値を持つ succeeded 実行のみ**が
> 寄与する。DLQ・timeout・sweeper 経路は count を立てつつリソース指標は 0 加算となる（「半端行」）。
> したがって `SUM(cpu_fuel_used)` 等を「全終端実行の総コスト」と解釈してはならない —
> あくまで「計測済み実行のリソース合計」である。権威 `resource_limits` が解決できなかった result は
> 信頼境界外の値を課金しないため、invocation は計上しつつリソース指標は記録しない（fail-closed）。
> 0007 マイグレーション適用前の実行は計量を持たず集計に現れない（additive・backfill なし）。

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
| control-plane が `SECRETS_MASTER_KEY is still the placeholder` で起動しない | **M7c の意図した fail-fast**。`.env.example` の既知プレースホルダのままでは起動させない（secret の暗号文は DB に永続するため、公開リポジトリの既知鍵で暗号化させない）。`openssl rand -hex 32` で実鍵を生成して `.env` に置く。 |
| invoke が `secret material unavailable` で `failed` になる | **M7c の fail-closed**（secret 欠損のまま実行しない）。worker から `CONTROL_PLANE_INTERNAL_URL`（既定 `http://127.0.0.1:8081`）へ到達できるか、CP が `INTERNAL_BIND_ADDR` で listen しているか（起動ログの `listening (internal: job-env exchange only)`）を確認。JetStream の backoff 再配送が自動リトライになる。 |
| secret を設定したのに env に現れない | `capabilities.env`（**admin 承認の許可リスト**）に名前が無い。`PUT /components/{id}/versions/{version}/capabilities` で承認する。**承認は version 単位**なので、新しい version を上げたら再承認が要る。 |
| 復号が失敗して invoke が落ちる（KEK ローテーション後） | `SECRETS_RETIRED_KEYS` から旧 KEK を**早期に撤去**した可能性。`faas_secret_versions_by_kid{kid="<旧 kid>"}` が 0 になるまで（＝ `POST /admin/secrets/rekey` が全行を再ラップし終えるまで）旧鍵を残すこと。撤去済みなら旧鍵を戻して再ラップをやり直す。 |
| canary を設定したのに全部 stable に落ちる | canary 版が soft delete 済み / ポインタ不整合の可能性（解決 SQL の LEFT JOIN が外れると **fail-safe に全量 stable**）。`GET /components/{id}/traffic` の `canary` が `null` でないか確認。 |
| `PUT /components/{id}/traffic` が 403 | canary 運用には **admin スコープ**のトークンが要る（細粒度スコープは M7 非スコープ）。 |
| 0009 の適用でローリング更新が詰まる | `executions` は最大テーブルであり、`CREATE INDEX idx_executions_component_finished` が索引構築のあいだ ACCESS EXCLUSIVE を取る（sqlx は各 migration を 1 tx で走らせるため `CONCURRENTLY` を書けない）。本番は保守窓に `cargo run -p faas-control-plane -- --migrate-only`（= `make migrate`）で**単独適用**してからローリング更新すること。 |
| `executions_routing_reason_chk` が `NOT VALID` のまま | 意図した状態（既存行のフルスキャン検証を避けるため）。完全化したい場合は保守窓で `ALTER TABLE executions VALIDATE CONSTRAINT executions_routing_reason_chk;` を手で流す。 |

---

## ディレクトリ構成

```
Cargo.toml                 # [workspace] members（crates/* と components/*）
rust-toolchain.toml        # stable + wasm32-wasip2
.env.example               # 環境変数の雛形（S3_* / WASM_CACHE_DIR / 上限・TTL 含む）
docker-compose.yml         # postgres:16 + nats:2 (--jetstream) + minio (+ minio-setup)
Makefile                   # setup / up / migrate / minio-bucket / build-component / run-* / deploy / invoke
                           #   M8: run-workers / autoscale / stop-workers / scale-status / lane-status
wit/world.wit              # faas:component@1.0.0（handler world）
migrations/0001_init.sql   # M1: tenants / components / component_versions / executions
migrations/0002_m2.sql     # M2: バージョン管理・サイズ等の追加カラム / インデックス
migrations/0003_auth.sql   # M3a: users / api_tokens、faas_app ロール、認証・認可スキーマ
migrations/0004_rls.sql    # M3b: 全テナント表に ENABLE / FORCE ROW LEVEL SECURITY とポリシー
migrations/0005_provenance.sql  # M3c: 署名鍵 kid / idempotency_key UNIQUE / audit_logs (append-only)
migrations/0006_large_io.sql    # M3d: input_ref / output_ref 列、tenants.quotas、大容量 I/O 用
migrations/0007_usage_metering.sql  # M5: executions 計量5列 + usage_rollups（FORCE RLS / DELETE 不可）
migrations/0008_m6.sql     # M6: cron_jobs / triggers / trigger_deliveries（FORCE RLS、冪等配送台帳）
migrations/0009_m7a_traffic_split.sql   # M7a: components へ canary 4 列 + executions.routing_reason（新表なし）
migrations/0010_m7b_function_configs.sql # M7b: function_configs（平文 env・行=1キー・複合 FK・FORCE RLS）
migrations/0011_m7c_secrets.sql          # M7c: function_secrets / function_secret_versions（追記専用の版台帳）
migrations/0012_m9a_component_signing.sql # M9a: component_signing_keys（FORCE RLS）+ tenants.require_signed_components
crates/shared/             # faas-shared: 型・NATS subject・メッセージ・エラー（共有契約の唯一の真実）
                           #   FailedMessage / failed_subject 等の M4c DLQ 型 + M6 reply subject / instance id を含む
crates/shared/src/egress.rs             # M9c: egress の SSRF ハードデニー純関数（IP 分類）+ host:port パーサ
crates/control-plane/      # faas-control-plane (bin): axum + storage(MinIO) + validation(wasmparser)
                           #   admission(Redis) / signing(Ed25519) / authz / RLS / subscriber
                           #   reaper + DLQ subscriber + metrics (M4a/c)
crates/control-plane/tests/chaos_m4.rs  # M4 障害注入の end-to-end テスト（#[ignore]）
crates/control-plane/tests/chaos_m5.rs  # M5 冪等会計の end-to-end テスト（E1: 重複は+1 / E2: N件は+N, #[ignore]）
crates/control-plane/tests/chaos_m6.rs  # M6 起動形態の end-to-end テスト（S1: 同期 / S2: Cron / S3: トリガー, #[ignore]）
crates/control-plane/tests/chaos_m7.rs  # M7 デプロイ運用の end-to-end テスト（S1: canary/rollback / S2: env・secret 注入 / S3: 非漏洩, #[ignore]）
crates/control-plane/tests/chaos_m8.rs  # M8 弾力スケール/アイソレーションの end-to-end テスト（U1〜U4, #[ignore]、要 CHAOS_TOKEN_B）
crates/control-plane/tests/chaos_m9.rs  # M9 サンドボックス強化のレッドチーム（v1/v3/v4/v5/v6/v7, #[ignore]）
crates/control-plane/src/routing.rs     # M7a: canary の決定的バケット選択（DB/時刻/乱数に非依存の純関数）
crates/control-plane/src/secrets.rs     # M7c: 封筒暗号（XChaCha20-Poly1305）+ KEK キーリング + execution 基準の世代解決
crates/control-plane/src/handlers_secrets.rs # M7c: secret の write-only API と POST /internal/job-env
crates/shared/src/redacted.rs           # M7-0: Redacted<T>（Serialize を実装しない秘密値ラッパ）
crates/worker/src/env.rs                # M7b/M7c: 許可リストで畳む env 組み立て（expose() の allowlist 対象）
crates/control-plane/src/lanes.rs       # M8: テナント別 lane の provisioning（単一 writer は advisory lock）+ backlog ポーラ
crates/control-plane/src/scale.rs       # M8: オートスケールの判断ロジック（純関数。時計は引数で注入し CI で決定的に検証）
crates/control-plane/src/metrics.rs     # M4a: Prometheus Registry とメトリクス定義
crates/worker/             # faas-worker (bin): wasmtime + async-nats + reqwest + cwasm キャッシュ
                           #   epoch ticker は OS スレッド (chaos_d 対策。M4b 設計メモ参照)
crates/worker/src/metrics.rs            # M4a: worker 側 Prometheus 公開（独立 axum サーバ）
components/echo/           # サンプル Component（cdylib, wasm32-wasip2）
components/always-trap/    # M4 chaos_c 用: handle 入口で panic（trap → DLQ 経路）
components/slow/           # M4 chaos_d 用: handle が tight loop（epoch interrupt → timeout 経路）
components/burn/           # M8 chaos 用: {"burn_ms": N} のぶん busy-loop して **succeeded で終端**（slow と違い成功する）
components/netprobe/       # M9c chaos 用: {"target":"host:port"} へ TCP 接続を試し、fs 到達も試す（egress/fs の実効検証）
scripts/worker-lib.sh      # M8: worker プロセスを slot 番号で管理する共通関数（起動確認 / ドレイン停止 / カウンタ退避）
scripts/run-workers.sh     # M8: worker を固定 N 台起動
scripts/worker-autoscale.sh # M8: GET /internal/scale を見て増減させる参照アクチュエータ
scripts/stop-workers.sh    # M8: 全台ドレイン停止
仕様書.md                  # 全体仕様（M5 範囲は §15 M5 / §14 メータリング行、M6 範囲は §15 M6 / §6.3 / §11 / §14 同期 Invoke・トリガー・Cron 行）
```

---

## 次のマイルストーン（仕様書 §15）

M9 完了済み（本リポジトリの現状）。**M5〜M9 が完了**し、商用マルチテナント SaaS の基本線が揃った。
次は M10 以降⬜:

- **M10: 可観測性の完成 / Multi Region（需要発火型, §15）**: 分散トレーシング（OpenTelemetry。
  M4a の correlation ID を OTel スパンへ。invoke → worker → result の経路を end-to-end に追う）。
  **WASM + NATS 経路はブラックボックス化しやすく高レバレッジ**のため前倒し推奨。Multi Region は
  地理分散要求が顕在化した時点で着手する需要発火型。Workflow Engine / Result Ingestor 分離も
  固定順序を持たない需要発火型。AI/LLM はプラットフォーム機能ではなく Capability 経由の外部呼び出し
  （§4.4 / §13。M9c の egress allowlist で実際に到達可能になった）で充足するため、ロードマップから除外。
- **M9 follow-ups**（M9 完了条件のスコープ外として意図的に送ったもの）:
  - egress バイトの計量（§14。M9c は到達可否のみ。gauge 配線は別途）
  - lane gauge の per-tenant ラベル（`METRICS_LANE_LABELS` は M8 で gate 済み）/ mTLS・宛先証明書
    ピンニング / seccomp・gVisor 等の OS レベル追加サンドボックス（M9 設計書 §8 参照）
- **M4 follow-ups**（M4 範囲内で残る配線）:
  - HTTP middleware で `faas_http_requests_total` / `_duration_seconds` の observe 配線
  - `execution_duration_seconds`（created_at→finished_at）を subscriber finalize 時に observe
  - per-tenant ラベル（`faas_tenant_invoke_total{tenant_id}`）のカーディナリティ対策
    （`METRICS_INCLUDE_TENANT_LABEL=false` フラグ追加）
  - `.result` / `.failed` の JetStream 化（現状 core NATS、CP 再起動で in-flight ドロップ）
  - `tenants.quotas` / `tenants.status` の admin API（現状 SQL 直 UPDATE）

詳細は `仕様書.md` の §15 実装ロードマップを参照してください。
