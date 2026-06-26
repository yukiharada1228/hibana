# M6 設計書 — 同期 Invoke + Cron + 外部イベントトリガー (§15)

本書は 仕様書 §15 M6 の具体実装設計である。M6 の目標は「非同期 invoke 一択から、商用で要求
される起動形態（同期 HTTP / Cron 定時 / 外部イベント）を揃える」こと。**完了条件**は
(1) HTTP 同期呼び出しが上限レイテンシ内で結果を返す、(2) Cron 登録で定時起動する、
(3) トリガー経路でも冪等性・テナント分離・計量(M5)が **non-HTTP 起点でも破れない** こと。

設計の中核思想は一つ:**全ての起動形態（HTTP async/sync・Cron・event・chain）を、
HTTP invoke が既に通っている「pending INSERT → job_token 署名 → JetStream publish →
subscriber finalize → usage_rollups」という単一の正規パスへ合流させる**。新しい入口は
「いつ・誰が enqueue するか」だけが違い、冪等性・provenance・計量・テナント分離の不変条件は
既存パスがそのまま担保する。これを実現するため M6-0 で **再利用可能な enqueue ヘルパ** を
切り出すのが全ステップの土台になる。

---

## 0. サブマイルストン分解（依存順）

| ID | タイトル | 依存 |
|----|---------|------|
| M6-0 | 基盤: 契約拡張 + enqueue ヘルパ + migration 0008 | なし |
| M6a | 同期 Invoke（Core NATS reply + per-instance waiter） | M6-0 |
| M6b | Cron（CRUD API + single-flight スケジューラ） | M6-0 |
| M6c | トリガー（登録 API + event 取込 + component chain + payload→input） | M6-0, (M6a/M6b 非依存) |

各ステップは独立にテスト可能で、先行マイルストンを壊さない。chaos テストは全て `#[ignore]`
（既定 `cargo test` は緑のまま）。

---

## 1. 共有契約の拡張（M6-0, `crates/shared/src/lib.rs`）

### 1.1 新 NATS subject

```text
reply.{instance_id}.{correlation_id}     # 同期 invoke の Core NATS reply 先（per-instance）
```

- `instance_id`: 各 CP インスタンス起動時に確定する subject-safe な ID（env `INSTANCE_ID`、
  未設定なら `inst_{uuid_simple}` を採番）。**ステートレス×N の鍵**: reply subject に
  instance_id を埋めることで、JobMessage を送った CP インスタンスだけが reply を受け取る
  購読を持つ。所有インスタンスが死ねば waiter は解決されず、CP/クライアント双方の timeout が
  確実に発火し 202 + execution_id へフォールバックする（後述 §2）。
- `correlation_id`: `corr_{uuid_simple}`。1 回の同期 invoke ごとに採番する不透明 ID。

```rust
/// 同期 invoke の Core NATS reply subject。所有 CP インスタンスのみが購読する。
pub fn reply_subject(instance_id: &str, correlation_id: &str) -> String {
    format!("reply.{instance_id}.{correlation_id}")
}
/// あるインスタンスが購読する reply ワイルドカード `reply.{instance_id}.*`。
pub fn reply_subject_wildcard(instance_id: &str) -> String {
    format!("reply.{instance_id}.*")
}
/// reply subject の第 3 トークン（correlation_id）を取り出す。
pub fn correlation_from_reply_subject(subject: &str) -> Option<&str> { /* split('.') */ }
```

> 注: reply subject は **テナントを含めない**。理由: 同期 invoke の終端化は依然 result/DLQ
> subject 経由で subscriber が行い（テナント由来 = `tenant.*.component.result`）、reply は
> 「既に検証・finalize 済みの結果」を呼び出し元へ届けるだけの **CP インスタンス内通知**で
> ある。reply に載る ResultMessage は job_token を保持し、reply を受けた側も **必ず署名検証
> してから** クライアントへ返す（§4 不変条件 provenance）。

### 1.2 `JobMessage` への追加フィールド（全て `#[serde(default, skip_serializing_if)]` で後方互換）

```rust
pub struct JobMessage {
    // ... 既存 ...
    /// M6a: 同期 invoke の Core NATS reply 先。worker は終端 result を **追加で** ここへ publish。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// M6: 起動由来。"http_invoke"(既定) / "cron" / "event" / "chain"。観測・監査用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}
```

`reply_to` は **DB に永続化しない**（揮発フィールド; correlation の漏洩面を作らない）。
worker は `reply_to` があれば、result/failed publish に加えて Core NATS で reply_to へも
ResultMessage を publish する（job_token を verbatim に echo）。

### 1.3 安定冪等キー生成ヘルパ（M6-0 で定義、M6b/M6c が利用）

```rust
/// Cron 1 fire ごとに安定な冪等キー: `cron:{cron_job_id}:{scheduled_slot_unix}`。
/// 同一 fire（同一スロット）は同一キー → (tenant_id, idempotency_key) UNIQUE で二重発火を吸収。
pub fn cron_idempotency_key(cron_job_id: &str, scheduled_slot_unix: i64) -> String {
    format!("cron:{cron_job_id}:{scheduled_slot_unix}")
}
/// イベント 1 配送ごとに安定な冪等キー: `event:{trigger_id}:{event_dedup_id}`。
/// event_dedup_id は外部イベントの安定識別子（S3: bucket/key/etag、chain: 上流 execution_id）。
pub fn event_idempotency_key(trigger_id: &str, event_dedup_id: &str) -> String {
    format!("event:{trigger_id}:{event_dedup_id}")
}
```

---

## 2. enqueue ヘルパの抽出（M6-0, control-plane の心臓部）

現状 `handlers::invoke()`（402–797 行）は「admission → input_ref 検証 → 冪等 fast-path →
component/version 解決 → presign → job_token 署名 → pending INSERT(savepoint) → JetStream
publish」を一体で持つ。M6 の全 non-HTTP 入口が同じ provenance/冪等/計量を共有するため、
**presign・publish・署名・pending INSERT のコア**を `handlers` 内の内部関数へ抽出する。

```rust
// crates/control-plane/src/enqueue.rs（新規モジュール）
pub struct EnqueueRequest<'a> {
    pub tenant: &'a str,
    pub component: &'a str,
    pub input: &'a serde_json::Value,
    pub input_ref: Option<&'a str>,
    pub execution_id: String,        // 呼び出し側が確定済み（新規 or 予約）
    pub idempotency_key: Option<&'a str>,
    pub request_hash: Option<&'a str>,
    pub reply_to: Option<String>,    // M6a 同期 invoke
    pub origin: &'static str,        // "http_invoke" / "cron" / "event" / "chain"
}

pub enum EnqueueOutcome {
    Enqueued { execution_id: String },
    IdempotentHit { execution_id: String, status: ExecutionStatus }, // 既存返却 or 409 判定済み
}

/// 「pending INSERT(provenance+冪等列) → tx commit → job_token 署名 → JobMessage publish
/// (Nats-Msg-Id=execution_id)」を 1 関数に閉じる。admission（rate/in-flight）と input_ref の
/// **完全一致検証**は呼び出し側（HTTP）の責務として残し、ここは「確定済みの enqueue」を担う。
pub async fn enqueue_execution(
    state: &AppState,
    tx: sqlx::Transaction<'_, sqlx::Postgres>, // GUC 設定済み
    req: EnqueueRequest<'_>,
) -> Result<EnqueueOutcome, AppError>;
```

**抽出方針（不変条件を 1 行も変えない）**:
- pending INSERT は既存 `db::insert_pending_execution_with_provenance` をそのまま使う
  （savepoint で 23505 を捕捉 → 再 SELECT → 同一 body 既存返却 / 異 body 409 = `IdempotentHit`）。
- job_token 署名は `state.signer().sign(&claims)`（claims は version_id/kid/iat/exp）を完全に流用。
- publish は `Nats-Msg-Id=execution_id` 付き JetStream publish を流用（layer-3 冪等）。
- `reply_to` / `origin` を JobMessage に載せるのが唯一の新規。

HTTP `invoke()` は admission・input_ref 検証・presign を残したまま、コアを
`enqueue_execution` 呼び出しへ置換する（**振る舞い不変のリファクタ**で M1–M5 を壊さない）。
Cron/event/chain は admission を「fire 時に reserve する／しない」を方針として持ち（§5.4）、
`enqueue_execution` を再利用する。

> **dead_code 回避**: M6-0 では `enqueue_execution` を HTTP invoke が即座に消費する（薄い
> consumer と一緒に land）。`reply_to` 経路は M6a まで `None` 固定だが、フィールド自体は
> serde default で害がなく、JobMessage の他フィールドと同様 worker 側は M6a まで無視する。

---

## 3. migration 0008 — `cron_jobs` + `triggers` + `trigger_deliveries`

`migrations/0008_m6.sql`（次の空き番号）。0004/0005/0007 のパターン（FORCE RLS・
fail-closed `current_setting('app.tenant_id')`・DELETE 非付与の追記寄り権限）を厳守する。
**所有者ロールで適用**（CREATE TABLE/FORCE RLS/CREATE POLICY/GRANT は所有権が要る）。

### 3.1 DDL スケッチ

```sql
-- (A) cron_jobs: 定時起動の登録。
CREATE TABLE IF NOT EXISTS cron_jobs (
    id              TEXT PRIMARY KEY,                      -- cron_*
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    component_id    TEXT NOT NULL REFERENCES components(id),
    schedule        TEXT NOT NULL,                         -- cron 式（5 フィールド）
    input           JSONB NOT NULL DEFAULT 'null'::jsonb,  -- fire 時に Component へ渡す入力
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    next_fire_at    TIMESTAMPTZ NOT NULL,                  -- 常に UTC。スケジューラが前進させる
    last_fired_slot BIGINT,                                -- 直近 fire の scheduled_slot（unix 秒・冪等補助）
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_due
    ON cron_jobs (next_fire_at) WHERE enabled;            -- スケジューラの due スキャン用

-- (B) triggers: 外部イベント / component chain の登録。
CREATE TABLE IF NOT EXISTS triggers (
    id              TEXT PRIMARY KEY,                      -- trg_*
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    component_id    TEXT NOT NULL REFERENCES components(id), -- 起動対象（downstream）
    trigger_type    TEXT NOT NULL,                         -- 'object_storage' | 'chain'
    -- object_storage: {"bucket_prefix": "...", "events": ["put"]}
    -- chain:          {"source_component_id": "cmp_...", "on_status": "succeeded"}
    match_config    JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- payload→input マッピング指示（JSON Pointer ベース。未指定なら event payload を素通し）。
    input_mapping   JSONB,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_triggers_chain_source
    ON triggers (tenant_id, component_id);  -- chain 解決はテナント内で source を引く（下記注）

-- chain の source 引きを高速化する補助 index（match_config からの式 index は避け、
-- subscriber 側はテナント内の enabled な chain trigger を引いてアプリ側で source 照合する）。
CREATE INDEX IF NOT EXISTS idx_triggers_enabled
    ON triggers (tenant_id) WHERE enabled;

-- (C) trigger_deliveries: イベント配送の重複排除台帳（追記専用）。
--     event_idempotency_key を一意制約にし、同一イベント再送で 23505 → 既配送として無視。
CREATE TABLE IF NOT EXISTS trigger_deliveries (
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    trigger_id      TEXT NOT NULL,
    event_dedup_id  TEXT NOT NULL,                         -- 外部イベントの安定識別子
    execution_id    TEXT NOT NULL,                         -- enqueue した execution
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, trigger_id, event_dedup_id)    -- 配送冪等の権威
);
```

### 3.2 RLS + 権限（3 テーブル共通、0007 パターン）

```sql
ALTER TABLE cron_jobs           ENABLE ROW LEVEL SECURITY; ALTER TABLE cron_jobs           FORCE ROW LEVEL SECURITY;
ALTER TABLE triggers            ENABLE ROW LEVEL SECURITY; ALTER TABLE triggers            FORCE ROW LEVEL SECURITY;
ALTER TABLE trigger_deliveries  ENABLE ROW LEVEL SECURITY; ALTER TABLE trigger_deliveries  FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON cron_jobs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
-- triggers / trigger_deliveries も同一ポリシー（fail-closed: 第 2 引数なし）。

REVOKE ALL ON cron_jobs, triggers, trigger_deliveries FROM PUBLIC;
-- cron_jobs / triggers は CRUD する（scheduler が next_fire_at を UPDATE、CRUD API が DELETE する）。
GRANT SELECT, INSERT, UPDATE, DELETE ON cron_jobs, triggers TO faas_app;
-- trigger_deliveries は配送台帳: 追記専用（DELETE/UPDATE 不可で重複排除の権威を改竄不能に）。
GRANT SELECT, INSERT ON trigger_deliveries TO faas_app;
REVOKE UPDATE, DELETE ON trigger_deliveries FROM faas_app;
```

> **全テナント巡回**（スケジューラの due スキャン・event ルーティングのテナント解決）は、
> RLS 下で `set_tenant_guc` 無しに走らせられない。`reaper::list_active_tenant_ids` と同型の
> **SECURITY DEFINER 関数** `cron_due_tenant_jobs()`（owner 権限で due 行のテナント+id だけ返す）
> を 0008 に定義し、スケジューラはそれで対象を引いてから、**各テナントごとに**
> `set_tenant_guc(tenant)` した tx で fire する（fire の本処理は RLS 下）。

---

## 4. 不変条件の保全（明示セクション）

| 不変条件 | M6 での保全方法 |
|---------|---------------|
| **テナント分離 (RLS)** | 全 enqueue/finalize は `set_tenant_guc(tenant)` 済み tx 内（FORCE RLS）。Cron 巡回 / event ルーティングのテナント解決のみ SECURITY DEFINER 関数（認証前参照と同型）。reply subject はテナントを含まないが、終端化は依然 `tenant.*.component.result` 経由で subscriber が **subject 第 2 トークンから**テナントを導出（本文盲信せず）。新規 db:: 関数は `scripts/rls-lint.sh` の fns= に追加。 |
| **結果出所認証 (provenance, §3.3)** | Cron/event/chain で生成した execution も CP が **job_token を署名**して JobMessage に載せる（HTTP と同一の `Signer::sign`）。worker は keyless のまま echo。subscriber は result/DLQ で kid 検証 + claim 照合してから finalize。**同期 reply** で受けた結果も、クライアントへ返す前に `verify` を必ず通す（reply 経路は別 transport だが検証規約は同一）。 |
| **計量 (M5, §15)** | 全起動形態が `subscriber::commit_finalize_and_release` の単一 finalize パスを通る（CAS finalize と同一 tx で `upsert_usage_rollup`）。invocation_count は全終端で +1、リソース指標は succeeded のみ。同期 invoke の **timeout は finalize を起こさない**（クライアントへ待機打ち切りを返すだけ）ので二重計上しない。後着 result が後で正規に finalize+計量する。 |
| **冪等性 (3 層, §6.6)** | layer1 Idempotency-Key (UNIQUE(tenant_id, idempotency_key)): Cron は `cron:{job}:{slot}`、event は `event:{trigger}:{dedup}` を **idempotency_key として INSERT** し、重複 fire/再送は 23505 → 既存返却で吸収。layer2 execution_id dedup・layer3 JetStream `Nats-Msg-Id=execution_id` は enqueue ヘルパが既存どおり担保。event はさらに `trigger_deliveries` PK で配送台帳を二重化。 |
| **ステートレス×N** | 同期 invoke の waiter registry は **per-instance（`Arc<DashMap>`、共有しない）**。reply subject に `instance_id` を埋め、JobMessage を送った当該インスタンスだけが `reply.{instance_id}.*` を購読。所有インスタンス障害時は waiter が解決されず CP/クライアント timeout が発火し 202+execution_id へフォールバック（GET /executions でポーリング可能）。Cron の single-flight は DB（FOR UPDATE SKIP LOCKED）で調停し、CP インスタンス間の合意は不要。 |

---

## 5. 各ステップ詳細

### 5.1 M6a — 同期 Invoke

**reply-waiter 設計（ステートレス×N の核心）**

1. CP 起動時に `instance_id` を確定（env or 採番）し、`AppState` に保持。Core NATS で
   `reply.{instance_id}.*` を 1 本購読する常駐タスクを `main.rs` で spawn。
2. `AppState` に `waiters: Arc<DashMap<String /*correlation_id*/, oneshot::Sender<ResultMessage>>>`。
3. 同期モード判定: `POST /invoke` に `?wait=1`（または `Prefer: wait` ヘッダ）。同期時、
   `correlation_id` を採番し `reply_to = reply_subject(instance_id, correlation_id)` を
   `EnqueueRequest` に渡す。admission/冪等/INSERT/publish は **非同期と完全に同一**。
4. publish 後、`waiters` に `(correlation_id, oneshot::Sender)` を登録し、
   `tokio::time::timeout(sync_reply_timeout, rx)` で待つ。
   - 解決（reply 到達）→ **受信 ResultMessage を `verify` してから** 200 + 結果（output/error）。
   - timeout → waiters からエントリを除去（leak 防止）し、**202 + execution_id**（既存セマンティクス
     へフォールバック）。worker はバックグラウンドで実行を続け、result/DLQ/sweeper が finalize。
5. reply 購読タスク: subject から `correlation_id` を取り出し、`waiters.remove` した sender に
   ResultMessage を送る。未知 correlation（既に timeout 除去済み）は drop。

worker 側（`handle_payload`）: result 組み立て後、`publish_result` に加え `job.reply_to` が
`Some` なら **Core NATS で reply_to へも同じ ResultMessage を publish**（job_token echo 保持）。
ack 戦略は不変（reply publish 失敗は best-effort、終端化は依然 result/DLQ が担保するので false を
返さない＝reply は速い通知であって正の経路ではない）。

timeout の上限は `SYNC_REPLY_TIMEOUT_MS`（既定 5000）。worker の `max_wall_time_ms` を超える
設定は無意味なので、実運用では `min(SYNC_REPLY_TIMEOUT_MS, max_wall_time_ms + margin)` を目安に。

### 5.2 M6b — Cron

**CRUD API**（auth scope: 登録・変更・削除は `Deploy` スコープ = component ライフサイクル相当）:
- `POST /cron-jobs`（create: component 存在検証 + cron 式パース + 初回 `next_fire_at` 計算）
- `GET /cron-jobs`（list: Read スコープ）
- `DELETE /cron-jobs/{id}`（Admin スコープ = 他 DELETE と整合）

**スケジューラループ**（`main.rs` で spawn、間隔 `CRON_POLL_INTERVAL_SECS` 既定 10）:
```text
loop every CRON_POLL_INTERVAL_SECS:
  for (tenant_id, job_id) in cron_due_tenant_jobs():   -- SECURITY DEFINER, now() >= next_fire_at
    tx = begin; set_tenant_guc(tenant_id)
    -- single-flight: 同一行を複数 CP が同時に掴まない
    row = SELECT ... FROM cron_jobs WHERE id=job_id AND enabled AND next_fire_at <= now()
          FOR UPDATE SKIP LOCKED
    if row is None: continue            -- 他インスタンスが先取り（SKIP LOCKED）
    scheduled_slot = floor(next_fire_at as unix)
    idem_key = cron_idempotency_key(job_id, scheduled_slot)
    -- 次回 fire を前進させてからコミット（掴んでいる間に重複 due を消す）
    UPDATE cron_jobs SET next_fire_at = <next cron occurrence>, last_fired_slot = scheduled_slot
    enqueue_execution(state, tx, EnqueueRequest{
        execution_id: new_execution_id(),
        idempotency_key: Some(idem_key),  -- (tenant_id, idem_key) UNIQUE で二重発火吸収
        request_hash: Some(hash(component, input)),
        origin: "cron", reply_to: None, ... })
    commit
```
- **multi-instance 安全性**: `FOR UPDATE SKIP LOCKED` で行ロックを最初に掴んだ CP だけが fire。
  仮に 2 CP が due を観測しても、同じ `scheduled_slot` → 同じ idempotency_key なので
  万一両方が enqueue まで進んでも UNIQUE(tenant_id, idempotency_key) で 2 件目が既存返却に倒れ
  二重発火しない（DB ロック + 冪等キーの二重防御）。
- 計量は HTTP と同一 finalize パス。Cron fire の admission は **in-flight reserve しない**
  方針（定時バッチが 429 で落ちるのは望ましくない。レート制御は将来の per-tenant cron quota で）。
  ただし将来 admission をかける場合も enqueue 前に `reserve_inflight` を挟むだけで対応可能。

### 5.3 M6c — トリガー

**登録 API**（scope は cron と同じ Deploy/Read/Admin マッピング）:
- `POST /triggers`（trigger_type=object_storage|chain + match_config + input_mapping 検証）
- `GET /triggers` / `DELETE /triggers/{id}`

**(a) Object Storage イベント取込**: `POST /events/object-storage`（内部ネット / 専用トークン
スコープ。MinIO/S3 のバケット通知 or 外部 ingress が叩く）。
- ペイロードから `tenant_id` を **オブジェクトキーのテナントプレフィックス**
  （`tenants/{tenant_id}/...`）から導出（本文の任意フィールドを信用しない = anti-spoof）。
- `event_dedup_id = {bucket}/{key}/{etag}`（同一オブジェクト同一版は同一）。
- 当該テナントの enabled な object_storage trigger を引き `match_config.bucket_prefix` で照合。
- マッチした各 trigger について `event_idempotency_key(trigger_id, event_dedup_id)` を
  idempotency_key に、`trigger_deliveries` へ INSERT（PK 23505 = 既配送 → skip）してから
  `enqueue_execution(origin="event")`。input は `input_mapping` で event payload を変換（§5.5）。

**(b) Component チェーン**: subscriber の **終端成功フック**で実装。
`commit_finalize_and_release` が `Some(period_start)`（= 実際に遷移した）かつ
`status==Succeeded` のとき、**同一テナント内の enabled な chain trigger** を引き、
`match_config.source_component_id == 当該 component_id` && `on_status` 一致なら downstream を
enqueue。`event_dedup_id = 上流 execution_id`（再配送で finalize が再実行されても CAS が
no-op なのでフックは遷移時だけ発火 → chain は 1 度だけ。さらに `trigger_deliveries` PK が
二重防御）。chain enqueue は finalize tx の **後**（commit 後）に best-effort で別 tx で行う
（finalize の原子性を chain 失敗で巻き戻さない。取りこぼしは将来の outbox で補強可能）。

**(c) payload→input マッピング (§5.5)**: `input_mapping` は JSON Pointer ベースの単純変換。
- 未指定 → event payload（または上流 output）を **そのまま** Component input にする。
- 指定時 → `{"<dest_field>": "<json_pointer_into_event>"}` の map を評価し新 input を構築。
不正な mapping は登録時（POST /triggers）に検証し 422。実行時に解決不能な pointer は null。

### 5.4 admission 方針（non-HTTP）
HTTP invoke は rate_limit + reserve_inflight を維持。Cron/event/chain は **rate_limit を課さず**
（定時/イベント駆動は 429 リトライ前提でない）、in-flight も reserve しない初期方針。
将来必要なら per-origin quota として enqueue 前に gate を挿す拡張点を残す（設計上 enqueue
ヘルパの外側で完結するため後付け可能）。

### 5.5 config / env 追加（`config.rs` + Makefile + `.env.example`）
| env | 既定 | 用途 |
|-----|------|------|
| `INSTANCE_ID` | `inst_{uuid}` | 同期 invoke reply subject の per-instance 識別子 |
| `SYNC_REPLY_TIMEOUT_MS` | `5000` | 同期 invoke の待機上限（超過で 202 フォールバック） |
| `CRON_POLL_INTERVAL_SECS` | `10` | Cron スケジューラの due スキャン間隔 |

---

## 6. テスト方針

- **DB-free 単体**: subject フォーマッタ（reply_subject / correlation_from_reply_subject）、
  冪等キー生成（cron/event）、migration 0008 の文字列不変条件（FORCE RLS / fail-closed /
  trigger_deliveries が DELETE 非付与）を `#[test]` で検証（既存 0007 の migration_tests と同型）。
- **chaos_m6.rs（`#[ignore]`）**: S1 同期 invoke が timeout 内に 200 を返す / S2 Cron 登録で
  定時起動 + 重複 tick が二重発火しない / S3 同一イベント 2 回送付で execution が 1 つ。
  既定 `cargo test` では走らない（M4/M5 と同じ運用）。
- 各ステップ完了時 `cargo build --workspace` と `cargo test --workspace`（非 ignore）が緑。
- `make rls-lint` を各ステップで通す（新 db:: 関数を fns= へ追加）。

---

## 7. 影響ファイル一覧（ステップ別）

- **M6-0**: `crates/shared/src/lib.rs`（subjects/JobMessage/冪等キー）、
  `crates/control-plane/src/enqueue.rs`（新規）、`crates/control-plane/src/handlers.rs`
  （invoke をヘルパ呼び出しへ）、`crates/control-plane/src/main.rs`（mod 宣言）、
  `crates/control-plane/src/config.rs`、`migrations/0008_m6.sql`、`.env.example`、`Makefile`、
  `scripts/rls-lint.sh`。
- **M6a**: `crates/shared/src/lib.rs`（reply 経路は M6-0 で land 済み・ここで consume）、
  `crates/control-plane/src/state.rs`（waiters + instance_id）、
  `crates/control-plane/src/main.rs`（reply 購読タスク spawn）、`handlers.rs`（?wait=1）、
  `crates/worker/src/main.rs`（reply_to publish）。
- **M6b**: `migrations/0008_m6.sql`（cron_jobs は M6-0 で land、CRUD/scheduler はここ）、
  `crates/control-plane/src/db.rs`（cron CRUD + due スキャン + FOR UPDATE SKIP LOCKED）、
  `handlers.rs`（CRUD ハンドラ）、`main.rs`（scheduler spawn）、`build_router`（ルート）。
- **M6c**: `triggers`/`trigger_deliveries`（M6-0 で land）、`db.rs`（trigger CRUD + 配送台帳 +
  chain 解決）、`handlers.rs`（trigger CRUD + object-storage event ハンドラ）、
  `subscriber.rs`（chain フック）、`main.rs`/`build_router`（ルート）、`crates/shared`（input_mapping 評価）。
```
