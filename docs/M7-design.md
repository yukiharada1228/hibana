# M7 設計書 — デプロイ運用とコンフィグ（canary / per-function 設定 / Secrets Manager）(§15)

本書は 仕様書 §15 M7 の具体実装設計である。M7 の目標は「デプロイを『latest=active の一発差し替え』から
**段階的に出して安全に戻せる運用**へ引き上げ、Component の設定と資格情報を**プラットフォームが管理する**」こと。

**完了条件（仕様書 §15 M7）**:
> active-version を 10%→100% へ段階移行でき、ワンクリック rollback が効き、
> secret はログ・監査・他テナントへ漏れない。

設計の中核思想は二つ:

1. **ルーティングの権威を `components` の 1 行に閉じ、解決を「1 本の SQL + 1 個の純関数」に分ける。**
   SQL は I/O（`db.rs`）、選択は DB 非依存の純関数（新規 `routing.rs`）。乱数を使わず決定的ハッシュで
   バケットを決めるため、CI で走る DB-free ユニットテストだけで「10%→100% の分配が正しい」ことを
   **全列挙で証明**できる。段階移行の全操作は**単一 UPDATE 文**に閉じ、中間状態を観測させない。
2. **平文設定と暗号化 secret を、データモデル・注入経路・API・コードモジュールのすべてで分離し、
   「平文 secret に触れるコード」を grep 可能な最小面に閉じ込める。**
   worker は鍵を持たない（§3.3 keyless-by-design）まま、平文 secret は
   「CP のメモリ → 内部専用 listener 上の HTTP 応答 → worker のメモリ → `WasiCtx`」だけを通り、
   NATS / S3 / DB / ログ / 監査のどこにも平文で永続化されない。

---

## 含む / 含まない

### 含む

**(a) バージョン traffic splitting / canary + 即時 rollback（§6.7 / §15）**
- `components` への additive 4 列（`canary_version_id` / `canary_weight` / `previous_active_version_id` /
  `canary_updated_at`）による 2 ポインタ + 重みモデル。**新規テーブルを作らない**。
- 決定的ハッシュバケットによる重み付き選択（純関数 `routing.rs`）。sticky ルーティング
  （`X-Faas-Routing-Key`）と再送・cron slot・chain の版一貫性。
- 全起動形態（HTTP invoke / Cron / event / chain）が `enqueue.rs` の唯一の解決点を通る。
- 制御 API: `PUT/DELETE/GET /components/{id}/traffic`、`POST .../promote`、`POST .../rollback`。
- `POST /components/{id}/versions` の `activate=false`（段階移行の起点を作るために必須）。
- `executions.routing_reason` による version 別の go/no-go 判断材料。

**(b) per-function 環境変数・設定（§15 M7b）**
- `function_configs`（平文・キー単位行）と、`component_versions.capabilities.env` の
  **version 単位** 許可リスト。許可リストの書き込みは **admin 専用の別エンドポイント**。
- worker の `WasiCtxBuilder`（`crates/worker/src/main.rs` の唯一の注入点）への env 注入。
- 上限（キー名形式 / キー数 / 値長 / 総バイト）の CP 検証 + worker 側の防御的 clamp（二重防御）。

**(c) Secrets Manager（§10 / §15 M7c）**
- `function_secrets`（メタのみ）+ `function_secret_versions`（**追記専用**の封筒暗号台帳）。
- XChaCha20-Poly1305 の封筒暗号（KEK keyring + 行ごとの DEK）、AAD による cut-and-paste 遮断。
- 注入経路: **内部専用 listener** 上の `POST /internal/job-env` を、`JobMessage` にのみ載る
  **専用 env-token**（result/DLQ へ echo されない）で引き換える。
- write-only API（値を返す関数をコードに作らない）、`Redacted<T>` による型レベルの露出ガード。
- KEK の計画的ローテーション（再ラップ）と、旧 kid 残存件数の観測。

### 含まない（M8 以降の follow-up）

- **version 別の恒久的な課金集計**。`usage_rollups` の PK は `(tenant_id, period_start, component_id)`
  （`migrations/0007_usage_metering.sql`）で version 次元が無く、追加は additive でなく `GET /usage` の
  互換も壊す。M7 の version 別成績は `executions` 生表の直近ウィンドウ集計に留める。
- **自動 canary 判定 / 自動 promote / 自動 rollback**（エラー率 SLO によるゲート）。M7 は
  「人間が `GET /traffic` を見て次の重みを PUT する」半自動まで。
- **細粒度スコープ**（`rollout:write` / `secrets:write`）。`Scope` は `Read/Invoke/Deploy/Admin` の 4 値固定
  （`crates/shared/src/lib.rs` の `Scope`）で、追加は `Role::ceiling` / `authz::resolve_token_scopes` /
  `resolve_login_scopes` と**既発行トークン行の後方互換**に波及する。「CI/CD が deploy スコープだけでは
  secret も canary も回せない」という運用制約を明示的に受け入れる。
- **`net.allow_outbound` の実効的なネットワーク強制**。`wasi:sockets/` は baseline 非承認のまま
  （`crates/control-plane/src/validation.rs` の `BASELINE_APPROVED_PREFIXES`）。M7 の「Capability の
  outbound 認証情報管理」は **「資格情報を secret として安全に保管し、承認された env キー名で
  Component へ注入するところまで」**。egress allowlist の実効強制は §15 **M9**。
- `fs.scratch_dir` / `stdio.inherit` の capability 配線。
- **KEK 侵害からの復旧としての deep re-encryption**（DEK 再生成 + 平文再暗号化）。§4.7 で二択として
  明示し、M7 は wrap-only の rekey のみ実装する。侵害時の復旧手順は「値そのものの rotate」。
- chain 全体での版固定（canary 中の rollback で上流 canary / 下流 stable が混ざることを許容する）。
- 同期 invoke の reply 経路・Workflow Engine 等、M6 の非スコープはそのまま持ち越す。

---

## 0. サブマイルストン分解（依存順）

| ID | タイトル | 依存 | 完了時の状態 |
| --- | --- | --- | --- |
| M7a-1 | 解決経路の一本化（挙動不変） | — | migration `0009` + `routing.rs` + `enqueue::resolve_version_for_enqueue` + 3 呼び出し点の差し替え。canary 未設定なので **M6 と同一挙動**。`cargo test --workspace` と chaos_m4/m5/m6 が緑のまま |
| M7a-2 | canary 制御 API + promote + rollback | M7a-1 | `PUT/DELETE/GET /components/{id}/traffic`、`POST .../promote`、`POST .../rollback`。`PUT /active-version` に監査追記 + canary クリア。`delete_version` の 3 ポインタ保護。`activate=false` |
| M7a-3 | 観測 + chaos S1 | M7a-2 | version 別成績、`faas_canary_routed_total`、`chaos_m7.rs` S1 |
| M7-0 | `Redacted<T>` + config/state 衛生 | （M7a とは独立。M7b の前に必要） | `faas_shared::Redacted<T>`、`Config` の秘密フィールド、既存の平文 secret 応答型、rls-lint (4) |
| M7b-1 | `capabilities` の 2 キー構造 + admin 承認 API | M7-0 | migration `0010` + `PUT /components/{id}/versions/{version}/capabilities`（admin）。`upload_version` は deny-all 既定 |
| M7b-2 | `function_configs` CRUD + worker 注入 | M7b-1 | `crates/worker/src/env.rs` 新設、`WasiCtxBuilder` への注入、echo 拡張 |
| M7c-1 | `secrets.rs`（封筒暗号 + kid keyring） | M7-0 | DB 非依存。単体テストのみで完結 |
| M7c-2 | secrets DDL + write-only CRUD API | M7c-1, M7b-2 | migration `0011` |
| M7c-3 | 注入経路（内部 listener + env-token 引き換え） | M7c-2 | `INTERNAL_BIND_ADDR`、`POST /internal/job-env`、worker の fail-closed |
| M7c-4 | KEK 再ラップ + メトリクス | M7c-3 | `POST /admin/secrets/rekey`（テナント単位）+ 背景ジョブ |
| M7-Z | chaos_m7 S2/S3 + ドキュメント | 全部 | README / Makefile / `.env.example` |

各ステップは独立にテスト可能で先行マイルストンを壊さない。chaos テストは全て `#[ignore]`
（既定 `cargo test` は緑のまま）。

> **cross-cutting な決定 1（migration 番号の確定）**: migration は **サブマイルストンごとに別ファイル**にする。
>
> | ファイル | version | 内容 |
> | --- | --- | --- |
> | `migrations/0009_m7a_traffic_split.sql` | 9 | M7a: `components` の 4 列 + `executions.routing_reason` + index |
> | `migrations/0010_m7b_function_configs.sql` | 10 | M7b: `function_configs` + `components (tenant_id, id)` UNIQUE |
> | `migrations/0011_m7c_secrets.sql` | 11 | M7c: `function_secrets` + `function_secret_versions` + SECURITY DEFINER 関数 |
>
> 理由は 2 つ。(1) M6 は 1 ファイル（`migrations/0008_m6.sql`）だったが 1 PR で land した。M7 は 3 ステップに
> 分かれて land するため、開発環境に一度適用した `0009` へ後から追記すると sqlx が
> `_sqlx_migrations.checksum` の不一致を検出して全員の CP が起動しなくなる
> （`crates/control-plane/src/main.rs` の `static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations")`）。
> (2) `sqlx::migrate!` はファイル名先頭の数値を version として収集するので、同一 version のファイルが
> 2 本あると重複で失敗する。**`0009_m7.sql` という名前は使わない**。
>
> `mod migration_tests`（`crates/control-plane/src/main.rs` の `#[cfg(test)] mod migration_tests`）に置く
> const も **1 ファイル 1 const**（`M7A_SQL` / `M7B_SQL` / `M7C_SQL` を別々の `include_str!`）にし、
> テスト関数名は `migrator_includes_version_9` / `_10` / `_11` と `m7a_*` / `m7b_*` / `m7c_*` で分離する
> （0007/0008 が既に 1 ファイル 1 const の作法）。

> **cross-cutting な決定 2（HTTP ステータスの事実確認）**: `crates/control-plane/src/error.rs` の
> `AppError::status()` は `FaasError::InvalidRequest` を **400** にしか写像せず、422 は存在しない。
> `handlers.rs` / `validation.rs` のコメント群が「422 で拒否」と書いているのは実際には 400 である。
> **M7 の仕様記述は全て 400 前提で書く**（既存コメントの誤りは M7 のスコープ外として直さない）。

---

## 1. データモデルとマイグレーション

### 1.1 M7a: 新テーブルを作らず `components` へ additive 列

| 案 | invoke の追加コスト | RLS / GRANT | 判定 |
| --- | --- | --- | --- |
| **A. `components` へ列追加** | **0**（既存 JOIN に LEFT JOIN 1 本、クエリ本数は 1 のまま） | `migrations/0004_rls.sql` の `ENABLE+FORCE RLS`・`tenant_isolation`・`GRANT SELECT, INSERT, UPDATE, DELETE ON components, ... TO faas_app` をそのまま継承（RLS は行単位なので ADD COLUMN に追加ポリシー不要） | **採用** |
| B. 新テーブル `component_traffic` | +1 クエリ or LEFT JOIN（同じ）＋ 行が無いケースの分岐 | `0004_rls.sql` の GRANT はテーブル名列挙なので明示 GRANT が必須、`CREATE POLICY` / `FORCE RLS` も新規に書く必要あり | 却下 |

決め手は **退行リスクの構造的除去**。事前調査で「M7 最大の退行は GRANT の抜け（RLS 以前に権限エラーで落ちる）」と
特定されている。M7a は新テーブルを 1 つも作らないことで、そのリスクを設計段階で消す。
配分データは component と 1:1、行数は component 数、更新頻度は極低、読み取りは invoke ごと ——
分離テーブルにする理由（多重度・ライフサイクル差・巨大化）がどれも成立しない。

### 1.2 M7a の状態モデル（4 列）

- `active_version_id`（既存, `migrations/0001_init.sql`）= **stable ポインタ**。`100 - weight` の側。
- `canary_version_id`（新規）= **canary ポインタ**。`weight` の側。
- `canary_weight`（新規, 0..100 の整数パーセント）
- `previous_active_version_id`（新規）= 直前の stable。**引数ゼロの rollback** を成立させる唯一の材料。
- `canary_updated_at`（新規）= 最終ルーティング変更時刻（`components` に `updated_at` 列が無いため）。

`canary_weight = 0 AND canary_version_id IS NULL` が既定＝M6 までと同一の解決。

### 1.3 DDL: `migrations/0009_m7a_traffic_split.sql`

ヘッダは `migrations/0008_m6.sql` の 4 ブロック定型（(a) additive のみ / (b) `BASELINE_VERSIONS` に入れない＝
`MIGRATOR.run` が通常適用・`main.rs` 変更不要 / (c) 所有者ロール `MIGRATION_DATABASE_URL` で適用する理由 /
(d) 設計決定の箇条書き）を踏襲する。

```sql
-- M7a: バージョン traffic splitting / canary + 即時 rollback（仕様書 §6.7 / §15）。
--
-- 0001-0008 への ADD のみ（additive。追加列は NOT NULL + DEFAULT か nullable ＝ backfill 不要）。
-- sqlx migrate ランナー（control-plane 起動時）が 0008 の後に適用する（BASELINE_VERSIONS には
-- 入れない＝MIGRATOR.run が通常適用する。main.rs は変更不要）。
-- このマイグレーションは**テーブル所有ロール**（0001-0008 を適用した所有者 = MIGRATION_DATABASE_URL）
-- で実行する必要がある: ALTER TABLE / ADD CONSTRAINT / CREATE INDEX は所有権を要する
-- （ランタイムの faas_app では実行できない）。
--
-- ローリング更新時のロック窓（重要・運用注記）:
--   executions は本システムで最も行数が伸びる表であり、以下の 2 文が書き込みを一時的にブロックする。
--     (1) ALTER TABLE executions ADD COLUMN routing_reason ... NOT NULL DEFAULT 'stable'
--         → PG11+ は既定値をカタログに持つのでテーブル書き換えは起きない（メタデータのみ・短時間の
--           ACCESS EXCLUSIVE）。**インライン CHECK は書かない**（既存全行の検証走査を誘発するため。
--           値域は下の NOT VALID 制約 + Rust 側 RoutingReason の 2 値で担保する）。
--     (2) CREATE INDEX idx_executions_component_finished（CONCURRENTLY 不可: sqlx は各 migration を
--         1 tx で走らせる）→ 索引構築のあいだ ACCESS EXCLUSIVE を取る。行数に比例した停止時間になる。
--   本番では `--migrate-only` 経路（control-plane の起動引数）で保守窓に単独適用してから
--   ローリング更新すること。README トラブルシュートに手順を書く。
--
-- 設計（M7 設計書 §1.1-1.3）:
--  - **新規テーブルを作らない**。canary の配分は components への additive 列で持つ。components は
--    0004_rls.sql で ENABLE+FORCE RLS + fail-closed tenant_isolation 済みであり、RLS は列単位ではなく
--    行単位なので ADD COLUMN には既存ポリシーがそのまま全列に適用される（0006/0007 の ADD COLUMN と
--    同じ整合性根拠）。0004_rls.sql の GRANT も components を列挙済みのため追加 GRANT も不要。
--    → 「新テーブルの GRANT 漏れ / RLS ポリシー漏れ」という M7 最大の退行リスクを構造的に消す。
--  - ルーティングの権威は components の 2 ポインタ（active_version_id=stable / canary_version_id）と
--    canary_weight（0-100 の整数パーセント）。invoke はこの 1 行を LEFT JOIN 1 本で読む（追加クエリ 0）。
--  - canary_version_id には FK を張らない。active_version_id が FK なしの素の TEXT（0001_init.sql）
--    であることとの対称性を保つため、かつ FK では防げない soft delete（component_versions.deleted_at,
--    0002_m2.sql）を解決 SQL の JOIN 条件で fail-safe に倒すほうが強い保証になるため。参照存在検証は
--    アプリ側（db::find_version_id）で必ず行う。
--  - executions.routing_reason は「その実行がどちら側で選ばれたか」の不変な記録。components は可変
--    なので version_id だけでは昇格後に stable/canary の区別が失われる（0008_m6.sql の chain_depth と
--    同じ additive + NOT NULL DEFAULT の判断: 既存行は 'stable' が意味的に正しく backfill 不要）。

-- ===========================================================================
-- (A) components: canary 配分と rollback 用の直前 stable（§6.7 / §15 M7a）。
-- ===========================================================================
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS canary_version_id TEXT;                   -- canary 側の version（FK なし: 上記）
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS canary_weight SMALLINT NOT NULL DEFAULT 0; -- canary へ流す割合（%）
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS previous_active_version_id TEXT;          -- 直前の stable（引数なし rollback 用）
ALTER TABLE components
    ADD COLUMN IF NOT EXISTS canary_updated_at TIMESTAMPTZ;            -- 最終ルーティング変更

-- 値域と「配分先の無い重み」を DB で不可能にする。ALTER TABLE ... ADD CONSTRAINT は IF NOT EXISTS を
-- 書けず単体では非冪等だが、sqlx は _sqlx_migrations で 1 度だけ適用するため可
-- （0007_usage_metering.sql / 0008_m6.sql と同じ正当化。手動再実行は前提にしない）。
-- components は行数が小さい（component 数）ので即時 VALIDATE してよい（executions とは扱いが違う）。
ALTER TABLE components
    ADD CONSTRAINT components_canary_weight_range
        CHECK (canary_weight >= 0 AND canary_weight <= 100);
ALTER TABLE components
    ADD CONSTRAINT components_canary_weight_requires_version
        CHECK (canary_weight = 0 OR canary_version_id IS NOT NULL);

-- M7b/M7c の複合 FK（tenant 一致を DB で強制する）の被参照側。既存 PK (id) は維持したまま
-- (tenant_id, id) の一意性を足す（additive）。0010/0011 が
-- FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id) でこれを参照する。
ALTER TABLE components
    ADD CONSTRAINT components_tenant_id_id_key UNIQUE (tenant_id, id);

-- 進行中の canary を運用側から引く（件数が小さいので部分 index）。
CREATE INDEX IF NOT EXISTS idx_components_canary_active
    ON components (tenant_id) WHERE canary_weight > 0;

-- ===========================================================================
-- (B) executions.routing_reason: version 決定理由（§15 M7a の観測）。
-- ===========================================================================
-- version_id だけでは「昇格後に stable になった版が canary として選ばれた実行」を後から区別できない
-- （components は可変で、実行時点のスナップショットではない）。NOT NULL + DEFAULT 'stable' なので
-- 既存行の backfill は不要（canary 導入前の全実行は定義上 stable 相当）。
-- **インライン CHECK は書かない**（上のロック窓注記）。値域は NOT VALID 制約で前方だけ守る。
ALTER TABLE executions
    ADD COLUMN IF NOT EXISTS routing_reason TEXT NOT NULL DEFAULT 'stable';

-- NOT VALID: 既存行のフルスキャン検証を行わず、以後の INSERT/UPDATE にだけ効かせる。
-- 既存行は全て DEFAULT 'stable' なので実質的に既に適合しており、保守窓で
--   ALTER TABLE executions VALIDATE CONSTRAINT executions_routing_reason_chk;
-- を手で流せば完全化できる（README 運用手順）。
ALTER TABLE executions
    ADD CONSTRAINT executions_routing_reason_chk
        CHECK (routing_reason IN ('stable', 'canary')) NOT VALID;

-- canary 判断（version 別の成功率・レイテンシ）用の直近ウィンドウ走査。既存 index は
-- (tenant_id, created_at DESC) / (component_id, status)（0001_init.sql）と
-- (tenant_id, finished_at DESC) 部分 index（0007_usage_metering.sql）で、component を絞った
-- 終端行の時間窓走査に効かない。追加のみで既存クエリの計画は変えない。
CREATE INDEX IF NOT EXISTS idx_executions_component_finished
    ON executions (tenant_id, component_id, finished_at DESC)
    WHERE status IN ('succeeded', 'failed', 'timeout');

-- ===========================================================================
-- RLS / 権限について（明示）: 本マイグレーションは新規テーブルを作らないため、
-- CREATE POLICY / ENABLE|FORCE RLS / GRANT / REVOKE は一切不要である。
--  - components は 0004_rls.sql で ENABLE+FORCE RLS + fail-closed tenant_isolation 済み。
--  - executions も同じ（0004_rls.sql）。行単位ポリシーは追加列にもそのまま適用される。
--  - faas_app への GRANT も 0004_rls.sql が両テーブルを列挙済み（列単位 GRANT ではない）。
-- ===========================================================================
```

### 1.4 M7a: soft delete / ポインタ整合の全ケース

| 事象 | 挙動 | 根拠 |
| --- | --- | --- |
| canary version が soft delete された | 解決 SQL の LEFT JOIN 条件 `cvv.deleted_at IS NULL` が外れ `canary=None` → **全量 stable**（fail-safe）。`GET /traffic` の `canary` は null | §2.4 |
| canary version の削除 API 呼び出し | `delete_version` の 3 ポインタ保護で **409** | §2.7 |
| **`previous_active_version_id` が指す version の削除 API 呼び出し** | 3 ポインタ保護で **409**（`"version is the rollback target; switch or clear it first"`） | §2.7 |
| **`previous` が（API 外の経路で）soft delete 済みなのに rollback された** | `rollback_active_version` の UPDATE は戻り先を `component_versions.deleted_at IS NULL` 付きで解決するため 0 行 → **409** `"previous version was deleted; specify a version explicitly"`。**tombstone を黙って active にしない** | §2.6 |
| canary が別 component の version を指す | 解決 SQL の `cvv.component_id = c.id` で `canary=None` → 全量 stable | §2.4 |
| component の soft delete | 解決 SQL の `c.deleted_at IS NULL` が既に効く（変更なし） | `crates/control-plane/src/db.rs` の `active_version_storage` |
| stable(=active) が壊れたポインタ（別 component / 不在） | 解決 SQL の JOIN が外れ **行なし** → 従来の `"has no active version"` 400 / cron は advance-only skip。**別 component の wasm を実行しない** | §2.4 |
| `active_version_id IS NULL`（未デプロイ） | canary の有無に関わらず `None`（現状どおり） | 同上 |

### 1.5 M7b/M7c: config と secret を分けるか → **分ける**

| 観点 | 分けない（1 表 + `is_secret` / JSONB 1 列） | 分ける（採用） |
| --- | --- | --- |
| 露出面 | 平文列と暗号文列が同じ Rust 構造体に同居し、`serde_json::Value` として API 応答・`audit_logs.detail` へ横流れしやすい（`migrations/0005_provenance.sql` が「理由・id のみ（生トークン・秘密は記録しない）」と規約化しているが型では守れない） | 暗号列は `BYTEA`。`serde_json::Value` に混ざる型経路が存在しない |
| 読み出し API | 「フラグを見て値をマスクする」分岐 = 実装ミス 1 回で漏れる | secret 表には**値を返す関数がそもそも無い**（write-only をコード構造で保証） |
| 注入経路 | 平文も暗号も同じ経路を通らざるを得ず、最も厳しい方（暗号）に全体を合わせる | 平文は worker が DB 直読み（`resolve_limits` と同型・追加信頼境界ゼロ）、暗号だけ新経路 |
| 権限 | 単一 GRANT | 版台帳を追記専用（`REVOKE UPDATE, DELETE`）にできる |

さらに `crates/control-plane/src/handlers.rs` は既に 3300 行超であり、secret に触れるコードは
**専用モジュール**（`secrets.rs` + `handlers_secrets.rs`）へ分割する。分割の実利は
「監査時に平文 secret を扱うファイルを列挙できる」こと（§5.6 の rls-lint (4) の allowlist と一致させる）。

### 1.6 スコープの単位: 値は **component 単位**、許可リストは **version 単位**

- 値（config / secret）を `component_id` に紐づける。version には紐づけない。
  - 根拠: rollback / canary（M7a）は `components` のポインタを差し替える操作であり、version を戻したときに
    設定・資格情報まで巻き戻ると運用が破綻する。**設定は「環境」の属性、コードは「version」の属性**。
  - canary 中は 2 version が同時に走るが、値が component 単位なので両者に同じ値が入る（一貫）。
- 「どの env キーを受け取ってよいか」の許可リストは `component_versions.capabilities`（`migrations/0002_m2.sql`,
  JSONB DEFAULT `'[]'`）に **version 単位** で持つ（§4.4 が「付与単位は Component Version」と規定）。
  - これにより「新 version が新キーを要求しても、旧 version にはそのキーが注入されない」が自動的に成立し、
    canary と直交する。
- `tenants.quotas`（`migrations/0006_large_io.sql`）と同型の JSONB を per-function に使う案は**採らない**。
  `tenants` は RLS 対象外（`migrations/0004_rls.sql` のコメント「tenants は tenant_id 列を持たないため対象外」）で
  あり、per-function 設定を RLS の外に出すのは完了条件「他テナントへ漏れない」に反する。

### 1.7 DDL: `migrations/0010_m7b_function_configs.sql`

```sql
-- M7b: per-function の平文環境変数（仕様書 §15 M7 / §4.4）。
-- ヘッダの 4 ブロック（additive / BASELINE 非登録 / 所有者ロール / 設計決定）は 0008_m6.sql に倣う。

-- ===========================================================================
-- function_configs: per-function の平文環境変数。
--   行=1 キー。JSONB 1 列にしない理由: serde_json::Value 経由で API 応答 /
--   audit_logs.detail へ横流れする経路を型で塞ぎ、キー単位の監査 target を自然にするため。
--   複合 FK (tenant_id, component_id) -> components (tenant_id, id):
--     単一列 FK（components(id)）はテナント一致を強制しない。RLS の WITH CHECK は「自分の
--     tenant_id を書くこと」しか要求しないため、テナント A が「tenant_id=A, component_id=（B の cmp_*）」
--     という行を作れてしまう。0009 が足した components (tenant_id, id) UNIQUE を参照して DB 側でも
--     同一テナント内であることを保証する（RLS + WHERE + FK の三重防御）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS function_configs (
    tenant_id    TEXT NOT NULL REFERENCES tenants(id),
    component_id TEXT NOT NULL,
    key          TEXT NOT NULL,                      -- ^[A-Z_][A-Z0-9_]{0,63}$（CP が検証）
    value        TEXT NOT NULL,                      -- 平文。読み出し可（secret ではない）
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by   TEXT,                               -- Principal::actor()
    PRIMARY KEY (tenant_id, component_id, key),
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)
);

-- RLS（0007/0008 パターン: fail-closed = current_setting の第 2 引数フォールバックなし →
-- 未設定 GUC は ERROR）。CREATE POLICY は IF NOT EXISTS 不可だが sqlx は 1 度だけ適用するため可。
ALTER TABLE function_configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_configs FORCE  ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON function_configs
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

-- 権限。0004_rls.sql の GRANT はテーブル名の列挙であり新表を含まないため、ここで明示する
-- （書き忘れると RLS 以前に権限エラーで faas_app から一切触れない = M7 最頻の退行）。
REVOKE ALL                            ON function_configs FROM PUBLIC;
GRANT  SELECT, INSERT, UPDATE, DELETE ON function_configs TO   faas_app;
```

### 1.8 DDL: `migrations/0011_m7c_secrets.sql`

```sql
-- M7c: Secrets Manager（仕様書 §10 / §15）。
-- ヘッダの 4 ブロックは 0008_m6.sql に倣う。設計要点:
--  - BIGSERIAL を使わない。PK は複合キー。→ 0004_rls.sql の GRANT ... ON ALL SEQUENCES
--    （0004 適用時点のシーケンスにしか効かない）や 0005_provenance.sql の明示 SEQUENCE GRANT を
--    必要としない。退行の起きやすい GRANT 漏れを一つ減らす。
--  - 値は component 単位（version には紐づけない）。よって component_versions への FK は張らない。
--  - 複合 FK でテナント一致を DB 側でも強制する（0010 と同じ理由）。
--  - 版台帳は追記専用。値の変更（rotate）も KEK の再ラップ（rekey）も「新しい version 行の INSERT」
--    で表現する → UPDATE を一切 GRANT しないまま rotation が成立する（audit_logs / trigger_deliveries
--    と同じ追記思想）。

-- ===========================================================================
-- (A) function_secrets: メタデータ + 現行世代ポインタ。**平文も暗号文もこの表には無い**。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS function_secrets (
    id              TEXT PRIMARY KEY,                -- sec_*
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    component_id    TEXT NOT NULL,
    name            TEXT NOT NULL,                   -- 注入される env キー名と同一（別名を作らない）
    current_version INTEGER NOT NULL,                -- 注入対象の世代（versions.version を指す）
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at      TIMESTAMPTZ,                     -- soft delete（§6.7 と同思想）
    FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id),
    -- 0011 の中で自己参照される複合キー（versions 側の複合 FK 用）。
    CONSTRAINT function_secrets_tenant_id_id_key UNIQUE (tenant_id, id)
);

-- name の一意性は **生存行のみ**（部分 UNIQUE index）。
-- テーブル制約 UNIQUE (tenant_id, component_id, name) にすると、soft delete 後に同名で作り直せず
-- 23505 になる。「侵害された資格情報を削除して同名で入れ直す」というインシデント対応が最も基本の
-- 操作であり、これを不可能にしてはならない。0005_provenance.sql の部分 UNIQUE
-- (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL と同じ作法。
CREATE UNIQUE INDEX IF NOT EXISTS uq_function_secrets_live_name
    ON function_secrets (tenant_id, component_id, name) WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_function_secrets_component
    ON function_secrets (tenant_id, component_id) WHERE deleted_at IS NULL;

-- ===========================================================================
-- (B) function_secret_versions: 封筒暗号の版台帳（**追記専用**）。
-- ===========================================================================
CREATE TABLE IF NOT EXISTS function_secret_versions (
    tenant_id    TEXT    NOT NULL REFERENCES tenants(id),
    secret_id    TEXT    NOT NULL,
    version      INTEGER NOT NULL,                   -- 1 から単調増加
    kek_kid      TEXT    NOT NULL,                   -- この行を包んだ KEK の kid（signing.rs と同概念）
    wrapped_dek  BYTEA   NOT NULL,                   -- AEAD(KEK, dek_nonce, DEK, aad=dek_aad)
    dek_nonce    BYTEA   NOT NULL,                   -- 24 バイト（XChaCha20-Poly1305）
    nonce        BYTEA   NOT NULL,                   -- 24 バイト
    ciphertext   BYTEA   NOT NULL,                   -- AEAD(DEK, nonce, plaintext, aad=value_aad)
    value_len    INTEGER NOT NULL,                   -- 平文バイト長（**API へは出さない**, §5.4）
    reason       TEXT    NOT NULL,                   -- 'create' | 'rotate' | 'rekey'
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by   TEXT,                               -- Principal::actor()
    PRIMARY KEY (tenant_id, secret_id, version),
    FOREIGN KEY (tenant_id, secret_id) REFERENCES function_secrets (tenant_id, id),
    CONSTRAINT function_secret_versions_reason_chk
        CHECK (reason IN ('create', 'rotate', 'rekey'))
);

-- execution 基準の世代解決（§4.6）で使う: 指定 secret の created_at <= T な最大 version。
CREATE INDEX IF NOT EXISTS idx_function_secret_versions_created
    ON function_secret_versions (tenant_id, secret_id, created_at DESC);

-- ===========================================================================
-- RLS + 権限
-- ===========================================================================
ALTER TABLE function_secrets         ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_secrets         FORCE  ROW LEVEL SECURITY;
ALTER TABLE function_secret_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE function_secret_versions FORCE  ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON function_secrets
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));
CREATE POLICY tenant_isolation ON function_secret_versions
    USING (tenant_id = current_setting('app.tenant_id'))
    WITH CHECK (tenant_id = current_setting('app.tenant_id'));

REVOKE ALL                    ON function_secrets         FROM PUBLIC;
REVOKE ALL                    ON function_secret_versions FROM PUBLIC;

-- function_secrets は current_version の前進 / soft delete のため UPDATE が要る。DELETE は不要。
GRANT  SELECT, INSERT, UPDATE ON function_secrets         TO   faas_app;
REVOKE DELETE                 ON function_secrets         FROM faas_app;
-- 版台帳は追記専用（暗号文の改竄・消去を faas_app から不可能にする）。
GRANT  SELECT, INSERT         ON function_secret_versions TO   faas_app;
REVOKE UPDATE, DELETE         ON function_secret_versions FROM faas_app;

-- ===========================================================================
-- SECURITY DEFINER 関数（0008_m6.sql の cron_due_tenant_jobs() と同型）。
--
-- **重大な設計上の区別（レビュー指摘の反映）**: 本リポジトリの `admin` は
-- **テナント管理者**であってプラットフォーム管理者ではない（admin_routes は auth::authenticate 配下で
-- principal.tenant_id が権威。プラットフォーム唯一の非テナント面は POST /admin/tenants のみ）。
-- したがって **HTTP ハンドラから呼ぶ関数は必ず p_tenant を取る**。全テナント横断版は
-- `_all` 接尾辞を付け、**CP 内部の背景ジョブ（scheduler.rs / reaper.rs 型）からのみ**呼ぶ。
-- _all の結果を HTTP 応答に載せてはならない (MUST NOT)。
-- ===========================================================================

-- (a) テナント内の、現行 kid でない current 世代を持つ secret（HTTP rekey が使う）。
CREATE FUNCTION secrets_stale_kek(p_tenant text, p_active_kid text)
    RETURNS TABLE (tenant_id text, secret_id text)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT s.tenant_id, s.id
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        WHERE s.tenant_id = p_tenant AND s.deleted_at IS NULL AND v.kek_kid <> p_active_kid
    $$;
REVOKE EXECUTE ON FUNCTION secrets_stale_kek(text, text) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION secrets_stale_kek(text, text) TO   faas_app;

-- (b) 全テナント巡回版（**背景ジョブ専用**。HTTP からは呼ばない）。
CREATE FUNCTION secrets_stale_kek_all(p_active_kid text)
    RETURNS TABLE (tenant_id text, secret_id text)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT s.tenant_id, s.id
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        JOIN tenants t ON t.id = s.tenant_id AND t.status = 'active'
        WHERE s.deleted_at IS NULL AND v.kek_kid <> p_active_kid
    $$;
REVOKE EXECUTE ON FUNCTION secrets_stale_kek_all(text) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION secrets_stale_kek_all(text) TO   faas_app;

-- (c) kid 別の current 世代件数（**Prometheus gauge の内部更新専用**。HTTP 応答には載せない）。
CREATE FUNCTION secrets_kek_kid_counts_all()
    RETURNS TABLE (kek_kid text, n bigint)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
        SELECT v.kek_kid, count(*)
        FROM function_secrets s
        JOIN function_secret_versions v
          ON v.tenant_id = s.tenant_id AND v.secret_id = s.id AND v.version = s.current_version
        WHERE s.deleted_at IS NULL
        GROUP BY v.kek_kid
    $$;
REVOKE EXECUTE ON FUNCTION secrets_kek_kid_counts_all() FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION secrets_kek_kid_counts_all() TO   faas_app;
```

> これらが SECURITY DEFINER である理由は `0008_m6.sql` の `cron_due_tenant_jobs()` と同じ: `faas_app` は
> FORCE RLS 下にあり、`set_tenant_guc` なしに巡回できない。返す列は `(tenant_id, secret_id)` /
> `(kid, count)` に絞り、**暗号文も secret 名も返さない**（`0004_rls.sql` の認証前参照 3 関数が
> 「返す列は必要最小限」としている作法）。再ラップの本処理は呼び出し側が各テナントごとに
> `set_tenant_guc` した tx で RLS 下に行う。

---

## 2. M7a — バージョン traffic splitting / canary + 即時 rollback

### 2.1 乱数の出所: **乱数を使わない**（決定的バケット）

`rand::OsRng` を invoke ごとに引く案を却下し、**ルーティングキーの SHA-256 からバケットを導出**する。

- **テスト容易性**: 純関数 `routing_bucket(component_id, key) -> u8` と `select_version(routing, bucket)` に
  分離でき、`store.rs` の「時刻を引数注入して token-bucket を決定化する」パターンと同型。全 100 バケット
  列挙で分配を証明できる。
- **再送の一貫性**: 同一 Idempotency-Key の再送、cron の同一 slot 再発火、chain の再配送が**常に同じ版**へ落ちる。
- **依存追加ゼロ**: `sha2` は既に control-plane の依存（`crates/control-plane/Cargo.toml` の「Wasm 検証
  パイプライン」ブロック）で `handlers.rs` の `invoke_request_hash_for` が使用中。`rand` はハッシュ・
  トークン生成用途に閉じたまま（`crypto.rs`）で、ホットパスに RNG を持ち込まない。
- **一様性**: キーは高エントロピー（`execution_id` / dedup id / Idempotency-Key）なのでバケットは一様。

### 2.2 ルーティングキーの決定順（sticky ルーティング）

| 起点 | キー | 効果 |
| --- | --- | --- |
| HTTP invoke | ① `X-Faas-Routing-Key` ヘッダ（任意, ASCII 印字可能 1..=128） | 呼び出し側が user_id/session_id を入れれば**同一ユーザは常に同じ版**（sticky） |
| HTTP invoke | ② `Idempotency-Key`（あれば） | 再送が同じ版へ落ちる |
| HTTP invoke | ③ `execution_id`（既定） | 一様ランダム相当 |
| Cron | `faas_shared::cron_idempotency_key(job_id, slot)` | slot ごとに決定的（再発火・複数 CP の競合でも同一） |
| event / chain | `execution_id`（`enqueue_via_trigger` が解決前に採番済み） | 一様相当。配送台帳で 1 回しか起動しない |

**検証失敗時の挙動（明示）**: `X-Faas-Routing-Key` が非 ASCII / 印字不可 / 空 / 129 バイト以上なら
`FaasError::InvalidRequest` = **400**（`Idempotency-Key` が `validate_idempotency_key` で 400 にしている
のと同作法。非 ASCII ヘッダも既存 invoke が 400 にしている）。**未指定は正常**で ②→③ にフォールバックする。

**冪等 hash との関係（明示）**: routing key は `invoke_request_hash_for`（`handlers.rs`）の正準化に
**含めない**。したがって「同一 Idempotency-Key で routing key だけ違う再送」は body 不一致の 409 ではなく
**冪等ヒット**（既存 execution を返す）になる。routing key は「どの版へ振るか」の入力であって
リクエストの意味的同一性の一部ではない、という解釈を §6.6 の運用として固定する。

> `X-Faas-Routing-Key` は **ヘッダ**であり URL に置かない。`crates/control-plane/src/main.rs` の
> `TraceLayer::new_for_http()` は URI（クエリ込み）を span に載せるため、識別子をクエリに置くと
> 全ログ行に伝播する。

### 2.3 純関数（新規 `crates/control-plane/src/routing.rs`）

`mod routing;` は `crates/control-plane/src/main.rs` の mod 宣言群（`mod reaper;` と `mod scheduler;` の間、
アルファベット順）に追加。

```rust
//! canary ルーティングの純関数 (M7a, §6.7 / §15)。
//!
//! **DB / 時刻 / 乱数に依存しない**。分配の数学だけをここに閉じ、DB-free ユニットテストで
//! 全列挙検証する（cron.rs / store.rs のテスト作法と同じ）。

/// version 決定理由（executions.routing_reason へそのまま保存する安定文字列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingReason { Stable, Canary }

impl RoutingReason {
    pub fn as_str(self) -> &'static str {
        match self { Self::Stable => "stable", Self::Canary => "canary" }
    }
}

/// components 1 行から解決したルーティング設定（db::resolve_component_routing が返す）。
pub struct ComponentRouting {
    pub component_id: String,
    pub stable: crate::db::ActiveVersion,
    /// soft delete 済み / ポインタ不整合（別 component・別テナント）なら None（fail-safe: 全量 stable）。
    pub canary: Option<crate::db::ActiveVersion>,
    /// 0..=100。DB の CHECK で保証されるが、防御的に clamp して読む（state.rs の clamp と同思想）。
    pub canary_weight: u8,
}

/// ルーティングキーを 0..=99 のバケットへ決定的に写像する。
///
/// ドメイン分離のプレフィクスを入れ、invoke_request_hash_for や cron_idempotency_key と
/// 同じ入力でも別の値になるようにする（冪等キーの hash と相関させない）。
pub fn routing_bucket(component_id: &str, routing_key: &str) -> u8 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"faas:canary:v1\0");
    h.update(component_id.as_bytes());
    h.update(b"\0");
    h.update(routing_key.as_bytes());
    let d = h.finalize();
    // 4 バイト（2^32）を 100 で割る剰余バイアスは 2^-32 オーダーで無視できる。
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    (n % 100) as u8
}

/// 唯一の選択規則: `bucket < canary_weight` なら canary、それ以外は stable。
///
/// 端点は特別扱いしない（数学的に自然に成立する）:
///  - weight = 0   → bucket(0..=99) < 0 は常に偽 → 常に stable
///  - weight = 100 → bucket(0..=99) < 100 は常に真 → 常に canary
///  - canary = None（未設定 / soft delete 済み / ポインタ不整合）→ 重みに関わらず stable（fail-safe）
pub fn select_version(r: &ComponentRouting, bucket: u8) -> (&crate::db::ActiveVersion, RoutingReason) {
    match &r.canary {
        Some(c) if bucket < r.canary_weight => (c, RoutingReason::Canary),
        _ => (&r.stable, RoutingReason::Stable),
    }
}
```

**端点に `if` を書かない**ことが重要。`weight == 0` / `weight == 100` の特別分岐を入れると、そこが
「rollback したのに canary へ流れ続ける」バグの温床になる。単一不等式なら全列挙テストで完全に閉じる（§7.2）。

### 2.4 解決 SQL（`crates/control-plane/src/db.rs` の `active_version_storage` を置換）

現行 `active_version_storage` を `resolve_component_routing` に置き換える。呼び出し元は 3 箇所しかない
（`handlers.rs` の `invoke`、`scheduler.rs` の `fire_due_job`、`handlers.rs` の `enqueue_via_trigger`）ので
互換ラッパは残さない（残すと「canary をバイパスする経路」を意図せず作れてしまう）。

```rust
/// 解決 SQL (M7a)。stable / canary の両側で「id 一致 + tenant 一致 + component 一致」を JOIN 条件にする。
const RESOLVE_ROUTING_SQL: &str = "SELECT c.id AS component_id, c.canary_weight, \
        sv.id  AS stable_version_id, sv.version AS stable_version, \
        sv.storage_uri AS stable_storage_uri, sv.wasm_sha256 AS stable_wasm_sha256, \
        sv.resource_limits AS stable_resource_limits, \
        cvv.id AS canary_version_id, cvv.version AS canary_version, \
        cvv.storage_uri AS canary_storage_uri, cvv.wasm_sha256 AS canary_wasm_sha256, \
        cvv.resource_limits AS canary_resource_limits \
     FROM components c \
     JOIN component_versions sv \
       ON sv.id = c.active_version_id AND sv.tenant_id = c.tenant_id \
      AND sv.component_id = c.id \
     LEFT JOIN component_versions cvv \
       ON cvv.id = c.canary_version_id AND cvv.tenant_id = c.tenant_id \
      AND cvv.component_id = c.id \
      AND cvv.deleted_at IS NULL \
     WHERE c.tenant_id = $1 AND c.name = $2 AND c.deleted_at IS NULL \
       AND c.active_version_id IS NOT NULL";
```

**`component_id = c.id` を両側に入れる判断（レビュー指摘の反映）**:

- canary 側にこれが無いと、`canary_version_id` が**同一テナント内の別 component の version** を指した場合
  （将来の writer / 運用の手 UPDATE / バックアップ復元 / component 削除→再作成）に弾けない。その状態で
  invoke すると JobMessage が `component=A` / `version=<B の semver>` になり、worker の `resolve_limits`
  （`crates/worker/src/main.rs`、`(tenant, component 名, cv.version)` で引く）が行を引けず
  `ResourceLimits::default()` へ**無言でフォールバック**する＝承認外のリソース上限で実行される。さらに
  `executions.component_id`(A) と `version_id`(B の版) が食い違い M5 の課金帰属も壊れる。
  「壊れたポインタは fail-safe に stable へ倒れる」という設計の中心主張が成立しなくなる。
- stable 側にも同条件を入れる。**一貫したデータでは結果が変わらない厳密な絞り込み**（`active_version_id` は
  `upload_version` / `set_active_version` が `find_version_id` 検証済みの id しか書かない）であり、
  壊れたポインタのときだけ「行なし → 既存の 400 `has no active version`」に倒れる。**別 component の
  wasm を実行するより、起動しないほうが安全**という判断。
- したがって M7a-1 の「挙動不変」の主張は **「述語は同値（alias 改名 + 一貫データでは no-op の narrowing）」**
  であって「文字単位で同一」ではない。§7.2 の回帰テストも凍結リテラルの `contains` 照合にする。

その他:
- `LEFT JOIN` の 4 条件（id / **tenant** / **component** / **未削除**）が canary の fail-safe を丸ごと担う。
- `resource_limits` から `max_wall_time_ms` を解決するロジックは現行をそのまま両側に適用
  （`serde_json::from_value::<faas_shared::ResourceLimits>(...).unwrap_or_default().max_wall_time_ms`）。
- RLS の二重防御（GUC + `WHERE tenant_id = $1`）は現行どおり。呼び出しは必ず `db::set_tenant_guc` 済み tx。

### 2.5 差し込み位置（合流点は 1 箇所）

純関数と SQL を貼り合わせる**非純粋なラッパ**を `crates/control-plane/src/enqueue.rs` に置く。
理由: `enqueue.rs` は M6-0 で「全入口が合流する唯一の正規パス」として作られたモジュール（モジュール doc）で
あり、ここに置けば **どの入口も canary をバイパスできない**。

```rust
// crates/control-plane/src/enqueue.rs（EnqueueRequest 定義の直前あたり）に追加

/// 選ばれた版と、その選択理由。
pub struct RoutedVersion {
    pub component_id: String,
    pub selected: db::ActiveVersion,
    pub reason: &'static str,   // routing::RoutingReason::as_str()
}

/// 全入口（HTTP invoke / Cron / event / chain）が通る唯一のバージョン解決点 (M7a)。
///
/// `tx` は set_tenant_guc 済み（FORCE RLS）。active version が無ければ None（呼び出し側が
/// 従来どおり 400 / advance-only skip / delivery-only skip に倒す）。
///
/// **メトリクスはここで inc しない**（§2.9: 解決しても enqueue されない経路が複数あるため）。
pub async fn resolve_version_for_enqueue(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    component_name: &str,
    routing_key: &str,
) -> Result<Option<RoutedVersion>, sqlx::Error> {
    let Some(routing) = db::resolve_component_routing(&mut *tx, tenant, component_name).await? else {
        return Ok(None);
    };
    let bucket = crate::routing::routing_bucket(&routing.component_id, routing_key);
    let (selected, reason) = crate::routing::select_version(&routing, bucket);
    Ok(Some(RoutedVersion {
        component_id: routing.component_id.clone(),
        selected: selected.clone(),
        reason: reason.as_str(),
    }))
}
```

差し替える 3 箇所（**1 箇所でも漏らすと cron / event が canary をバイパスする**）:

| 位置（アンカー） | 現行 | 変更 |
| --- | --- | --- |
| `handlers.rs` `invoke()` 内、`find_component_by_name` 直後の `db::active_version_storage(...)` | `db::active_version_storage(&mut *tx, tenant, &req.component)` | `enqueue::resolve_version_for_enqueue(&mut *tx, tenant, &req.component, &routing_key)`。`routing_key` は §2.2 の ①→②→③ で **冪等 fast-path（`find_execution_by_idempotency_key` の分岐）の後・admission reserve（`admission::reserve_inflight`）の前**に決める |
| `scheduler.rs` `fire_due_job()` 内の `crate::db::active_version_storage(...)` | 同上 | 同上。`slot` / `idem_key` の算出（現行はこの呼び出しの**後**）を**呼び出しより前へ移動**し、`idem_key` をルーティングキーに使う |
| `handlers.rs` `enqueue_via_trigger()` 内の `db::active_version_storage(...)` | 同上 | 同上。ルーティングキーは既に採番済みの `execution_id` |

`EnqueueRequest`（`enqueue.rs`）へ 1 フィールド追加:

```rust
    /// M7a: version 決定理由（"stable" | "canary"）。executions.routing_reason へ保存し、
    /// publish ack 成功後の faas_canary_routed_total にも使う。
    pub routing_reason: &'static str,
```

`db::insert_pending_execution_with_provenance` に 12 番目の引数 `routing_reason: &str` を足し、INSERT の
列リストへ追加。呼び出しは 2 箇所:
- `enqueue.rs` の `enqueue_execution` 内（savepoint 内の本命）
- **`db.rs` の `insert_pending_execution`（`#[allow(dead_code)]` の簡易版）** —— これが 11 引数で
  `insert_pending_execution_with_provenance` を呼んでいるため、12 引数化に伴い `"stable"` を渡すよう修正する
  （設計時に見落とされやすい。§8 の影響ファイルに明記）。

**wire は変えない**。`JobMessage`（`crates/shared/src/lib.rs`）に `routing_reason` は載せない。worker は
選ばれた版の `version`（semver）を受け取って `resolve_limits` で `(tenant, component_name, version)` から
limits を引くので、canary で 2 版が同時に走っても `component_versions` の `UNIQUE (component_id, version)` で
正しく分岐する。**worker 側の変更はゼロ**。

`EnqueueRequest` は `version_id` を executions・job_token claim・`JobMessage.version` へ**同時供給**する
ため、canary で選んだ版が DB・provenance・wire で自動的に一致し、subscriber の claim 突合も素通りする。

### 2.6 db 層（`crates/control-plane/src/db.rs`）

**「stable ポインタを動かす操作は必ず 1 文で完結し、canary をクリアし、no-op では履歴を壊さない」**が
3 関数共通の不変条件である。

```rust
/// stable ポインタを切り替える (§6.7)。M7a: 直前 stable を退避し、canary 配分を必ずクリアする。
///
/// 戻り値は更新が起きたか。**根拠**: 既存の set_active_version 呼び出し 2 箇所（upload_version /
/// set_active_version ハンドラ）は同一 tx 内で先に find_component_by_id で 404 判定済みであり
/// rows_affected == 0 は到達しない。bool 化するのは **promote / rollback が事前 SELECT 無しで
/// 単一 UPDATE に閉じ、0 行を 404/409/400 へ写像する必要がある**ためで、対称性のためにここも揃える。
///
/// `previous_active_version_id` の CASE ガード: 同じ版を再 activate（宣言的 CI/CD が毎回同じ
/// version を PUT する運用）したときに previous を自分自身で潰さない。潰すと直後の rollback が
/// 「200 を返すのに何も戻らない」という最悪の failure mode になる。
pub async fn switch_active_version(
    executor: impl sqlx::PgExecutor<'_>,
    tenant_id: &str, component_id: &str, version_id: &str,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE components \
            SET previous_active_version_id = CASE \
                    WHEN active_version_id IS DISTINCT FROM $3 THEN active_version_id \
                    ELSE previous_active_version_id END, \
                active_version_id = $3, \
                canary_version_id = NULL, \
                canary_weight = 0, \
                canary_updated_at = now() \
          WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    ).bind(tenant_id).bind(component_id).bind(version_id)
     .execute(executor).await?;
    Ok(r.rows_affected() > 0)
}

/// canary を stable へ昇格する (M7a)。**単一 UPDATE + CAS**（read-then-write のレースを作らない）。
///
/// `target` が None なら現 canary_version_id を昇格。Some なら「オペレータが見た canary と昇格対象が
/// 一致すること」を CAS 条件にする（別 CP の PUT /traffic が割り込んで別の版を 100% 出す事故を防ぐ）。
/// 0 行 = component 不在 / canary 未設定かつ target 未指定 / CAS 不一致 → 呼び出し側が 404 or 409。
const PROMOTE_ACTIVE_VERSION_SQL: &str =
    "UPDATE components \
        SET previous_active_version_id = CASE \
                WHEN active_version_id IS DISTINCT FROM COALESCE($3, canary_version_id) \
                THEN active_version_id ELSE previous_active_version_id END, \
            active_version_id = COALESCE($3, canary_version_id), \
            canary_version_id = NULL, \
            canary_weight = 0, \
            canary_updated_at = now() \
      WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL \
        AND canary_version_id IS NOT NULL \
        AND ($3 IS NULL OR canary_version_id = $3) \
  RETURNING active_version_id, previous_active_version_id";

/// ワンクリック rollback: canary を破棄し、指定版（None なら previous_active_version_id）へ戻す。
///
/// **戻り先は必ず「未削除の version」として解決する**。単なる COALESCE UPDATE にすると、
/// previous が soft delete 済みのとき tombstone を黙って active にしてしまう（解決 SQL の stable 側は
/// deleted_at を見ないので、GET /components/{id}/versions に出てこない版が 100% を受ける幽霊状態になる）。
/// FROM 句の JOIN で `cv.deleted_at IS NULL` を要求し、満たさなければ 0 行 → 409 に倒す。
const ROLLBACK_ACTIVE_VERSION_SQL: &str =
    "UPDATE components c \
        SET previous_active_version_id = CASE \
                WHEN c.active_version_id IS DISTINCT FROM cv.id \
                THEN c.active_version_id ELSE c.previous_active_version_id END, \
            active_version_id = cv.id, \
            canary_version_id = NULL, \
            canary_weight = 0, \
            canary_updated_at = now() \
       FROM component_versions cv \
      WHERE c.tenant_id = $1 AND c.id = $2 AND c.deleted_at IS NULL \
        AND cv.id = COALESCE($3, c.previous_active_version_id) \
        AND cv.tenant_id = c.tenant_id AND cv.component_id = c.id \
        AND cv.deleted_at IS NULL \
  RETURNING c.active_version_id, c.previous_active_version_id";
```

新規 db 関数（全て `set_tenant_guc` 済み tx で呼ぶ）:
`resolve_component_routing` / `switch_active_version` / `promote_active_version` /
`rollback_active_version` / `set_traffic_split` / `clear_traffic_split` / `version_stats_for_component`。

**`scripts/rls-lint.sh` の `fns=` に必ず追記する**（M6 が同じことをした前例あり）:

```sh
# M7a canary ルーティング / 段階移行（すべて set_tenant_guc 済み tx で呼ぶ）。
fns="$fns|resolve_component_routing|switch_active_version|promote_active_version"
fns="$fns|rollback_active_version|set_traffic_split|clear_traffic_split|version_stats_for_component"
```

同時に `active_version_storage` を `fns` から**削除**する（関数を消すため）。`promote_active_version` の
追記漏れは実際にレビューで指摘された抜けなので、7 本すべてを 1 回で入れること。

### 2.7 既存ハンドラへの変更

| 位置（アンカー） | 変更 |
| --- | --- |
| `handlers.rs` `set_active_version` | (1) `db::switch_active_version` に差し替え（canary クリア + previous 退避 + CASE ガード）。(2) `db::insert_audit_log(action="active_version_switched")` を **commit 前・同一 tx** で追加（現行は `tracing::info!` のみで監査が無い）。(3) 戻り値 false は 404 に写像（事前 404 済みなので到達しない防御） |
| `handlers.rs` `upload_version` | multipart に `activate` フィールド（`"false"` のみ false 扱い、**既定 true**）。false なら `set_active_version` を呼ばず version の保存のみ。`component_versions.status` は現行どおり `'active'`（= 「利用可能」の意味。ルーティングの権威は `components` の 2 ポインタであることを doc に明記）。未知フィールドを無視する前方互換設計なので既存クライアントは無変更 |
| `handlers.rs` `delete_version` | 現行の active 保護 + pending/running 保護に加え、**canary 保護**と **previous 保護**を追加（計 3 ポインタ）。それぞれ 409: `"version is the canary target; clear the traffic split first"` / `"version is the rollback target; switch active-version or roll back first"` |
| `db.rs` `ComponentRow` / `find_component_by_id` / `find_component_by_name` | `canary_version_id` と `previous_active_version_id` を SELECT 列に追加（`delete_version` の 3 ポインタ判定に必要）。**`list_components` / `GET /components` の応答形は変えない**（M7a-2 は API 応答の後方互換を優先。canary の可視化は `GET /components/{id}/traffic` が担う） |

### 2.8 API 設計

#### 2.8.1 スコープの決定

**canary 制御 3 本と promote / rollback は全て Admin、参照のみ Read**。

根拠: 既存の `PUT /components/{component_id}/active-version` が Admin（`main.rs` の `admin_routes`）。
版の切替権限を 1 スコープに集約しないと「Deploy トークンで stable を動かせるが Admin でないと戻せない」
という非対称が生まれる。細粒度スコープは M7 非スコープ（「含まない」節）。
**README に「canary 運用には admin スコープのトークンが要る」と明記する**（CI/CD の設計に影響する）。

`authz::require_admin_role` は**付けない**（既存 `PUT /active-version` が付けていないため。付けると
既存の rollback 導線と非対称になる）。これは secret 系（§4.5、admin スコープ + admin ロールの二重ガード）
とは扱いを変える意図的な非対称であり、理由は「版の切替は既に admin スコープの既存操作である／
secret の付与は §4.4 の『承認』に当たる」。

#### 2.8.2 ルータ登録（`main.rs` の `build_router`）

```rust
    // --- Admin スコープに追加（M7a, §6.7 / §15） ---
    // PUT /components/{id}/traffic: canary の版と重みを設定（絶対値・冪等）。
    .route("/components/{component_id}/traffic", put(handlers::set_traffic_split))
    // DELETE /components/{id}/traffic: canary を解除（weight=0 + ポインタ NULL）。
    .route("/components/{component_id}/traffic", delete(handlers::clear_traffic_split))
    // POST /components/{id}/promote: canary を stable へ昇格（CAS つき単一 UPDATE）。
    .route("/components/{component_id}/promote", post(handlers::promote_version))
    // POST /components/{id}/rollback: ワンクリック rollback（canary 破棄 + 直前 stable へ復帰）。
    .route("/components/{component_id}/rollback", post(handlers::rollback_version))

    // --- Read スコープに追加 ---
    // GET /components/{id}/traffic: 現在の配分と version 別の直近成績（canary 判断の一次情報）。
    .route("/components/{component_id}/traffic", get(handlers::get_traffic_split))
```

`build_router` の doc コメントの列挙も更新する（M6 が各行にコメントを添えたのと同じ作法）。
新ハンドラ 5 本は `handlers.rs` の **`object_storage_event` の直後・`#[cfg(test)] mod tests` の直前**に
`// ---` 3 行のセクション見出し付きで置く（行番号ではなくこのアンカーで指定する）。

#### 2.8.3 状態遷移表（全操作の効果）

| 操作 | `active_version_id` | `previous_active_version_id` | `canary_version_id` | `canary_weight` |
| --- | --- | --- | --- | --- |
| `POST /versions`（既定 = 現状維持） | 新版 | 旧 active（新版と同一なら不変） | NULL | 0 |
| `POST /versions` `activate=false`（新規） | 不変 | 不変 | 不変 | 不変 |
| `PUT /traffic {canary_version, weight}` | 不変 | 不変 | 指定版 | 指定値 |
| `DELETE /traffic` | 不変 | 不変 | NULL | 0 |
| `PUT /active-version {version}`（既存） | 指定版 | 旧 active（指定版と同一なら不変） | NULL | 0 |
| `POST /promote` | 旧 canary（or 指定版・CAS） | 旧 active（同一なら不変） | NULL | 0 |
| `POST /rollback`（body 空） | `previous`（未削除であること） | 旧 active | NULL | 0 |
| `POST /rollback {version}` | 指定版（未削除であること） | 旧 active | NULL | 0 |

**「stable ポインタを動かす操作は必ず canary をクリアする」**が不変条件。これにより「新しい stable を
入れたのに古い canary 配分が残って混線する」が構造的に起きない。canary 未使用時のクリアは no-op なので
**既存 API の挙動は不変**。

#### 2.8.4 段階移行フロー（完了条件そのもの）

```
1. POST /components/{id}/versions   (version=0.2.0, activate=false)      -> 201
2. PUT  /components/{id}/traffic    {"canary_version":"0.2.0","weight":10}  -> 200
3. GET  /components/{id}/traffic    -> version 別の成功率/レイテンシで判断
4. PUT  /components/{id}/traffic    {"canary_version":"0.2.0","weight":50}  -> 200
5. PUT  /components/{id}/traffic    {"canary_version":"0.2.0","weight":100} -> 200
6. POST /components/{id}/promote    {"version":"0.2.0"}  -> 200（active=0.2.0, canary クリア）
   異常時はいつでも:
   POST /components/{id}/rollback   {}   -> 200
```

手順 1 の `activate=false` は **必須**。現行 `upload_version` は保存直後に無条件で `db::set_active_version`
を呼び新版を即 100% active にするため、これを回避できないと段階移行が形骸化する。

#### 2.8.5 各エンドポイント仕様

すべて `crates/control-plane/src/extract.rs` の `JsonBody<T>` を使う（素の `axum::Json` は rejection が
統一エンベロープを通らないため禁止）。テナントは常に `principal.tenant_id` が権威。

**`PUT /components/{component_id}/traffic`（admin）**

```json
// request
{ "canary_version": "0.2.0", "weight": 10 }
// response 200
{ "component_id": "cmp_...", "stable_version_id": "ver_...", "stable_version": "0.1.0",
  "canary_version_id": "ver_...", "canary_version": "0.2.0", "weight": 10 }
```

- `weight` が 0..=100 外 → **400**
- `canary_version` 空 → 400
- component 不在 → 404（`db::find_component_by_id` と同型）
- `canary_version` が当該 component の未削除 version でない → 404（`db::find_version_id`。
  **FK が無いのでアプリ側検証が唯一の参照整合**）
- `canary_version` が現 stable と同一 → 400（`"canary version must differ from the active version"`）
- `weight = 0` + `canary_version` 指定は許可（= 配分 0 で待機。段階移行の開始前状態）。完全解除は `DELETE`
- 冪等性: **絶対値の PUT**。同じ body の再送は同じ終状態。`Idempotency-Key` は使わない
  （executions の冪等機構は invoke 専用の概念）
- 監査: `db::insert_audit_log` を**同一 tx** で。`action="traffic_split_updated"`, `target=component_id`,
  `detail={"stable_version_id":..,"canary_version_id":..,"weight":10}`

**`DELETE /components/{component_id}/traffic`（admin）** — 204。
`canary_version_id=NULL, canary_weight=0, canary_updated_at=now()`。`action="traffic_split_cleared"`。
既に未設定でも 204（冪等）。component 不在は 404。

**`POST /components/{component_id}/promote`（admin）**

```json
// request（body 空 or {"version":"0.2.0"}）
{}
// response 200
{ "component_id": "...", "active_version_id": "ver_...", "version": "0.2.0",
  "previous_active_version_id": "ver_...", "canary_cleared": true }
```

`PROMOTE_ACTIVE_VERSION_SQL`（§2.6）の 1 文で確定。0 行の写像:
component 不在 → 404 / canary 未設定 → 400 `"no canary configured"` /
`version` 指定が現 canary と不一致 → 409 `"canary version changed; re-read GET /traffic"`。
判別のため 0 行時のみ `find_component_by_id` + 現 canary を再読して理由を確定する（**成功パスは
1 文のまま**なのでレースは起きない）。`action="version_promoted"`。

**`POST /components/{component_id}/rollback`（admin）— ワンクリック**

```json
// request（body 空 = 既定の安全側）
{}
// response 200
{ "component_id": "...", "active_version_id": "ver_...", "version": "0.1.0",
  "rolled_back_from": "ver_...", "canary_cleared": true }
```

「ワンクリック」である根拠（5 点）:

1. **引数がいらない**。`previous_active_version_id` を DB が持つので、オペレータは「どの版に戻すか」を
   調べる必要がない（`PUT /active-version` は semver の指定が要る）。
2. **呼び出し側が現在の状態を知らなくてよい**。canary 進行中（weight>0）でも昇格後でも、同じ 1
   エンドポイントが両方を安全側へ倒す。「今どっちのモードか」を分岐する運用手順書が不要。
3. **単一 tx・単一 UPDATE 文**で確定する。「重みだけ 0 になって active は新版のまま」といった中間状態が
   一切観測されない。
4. **伝播待ちがゼロ**（ただし §2.10 の実効範囲の限定を必ず併読すること）。版の解決は invoke ごとに
   `components` を読む（キャッシュ層なし）ため、commit 直後の**次の enqueue**から効く。
5. **監査に 1 行残る**。`action="version_rollback"` を同一 tx で `audit_logs` へ追記する。

0 行の写像: component 不在 → 404 /
previous が NULL かつ version 未指定 → 409 `"no previous version to roll back to; specify a version"` /
戻り先が soft delete 済み → 409 `"previous version was deleted; specify a version explicitly"`。

セマンティクスは **undo**（直前の切替を取り消す）。連続 2 回呼ぶと元に戻るため、応答に必ず
`rolled_back_from` / `active_version_id` の両方を返して「今どこにいるか」を明示する。

**`GET /components/{component_id}/traffic`（read）**

```json
{
  "component_id": "...",
  "stable": { "version_id": "ver_a", "version": "0.1.0" },
  "canary": { "version_id": "ver_b", "version": "0.2.0" },
  "weight": 10,
  "updated_at": "2026-08-25T00:00:00Z",
  "window_minutes": 60,
  "version_stats": [
    { "version_id": "ver_a", "succeeded": 900, "failed": 3, "timeout": 0,
      "canary_routed": 0, "p50_wall_time_ms": 12, "p95_wall_time_ms": 40 },
    { "version_id": "ver_b", "succeeded": 95, "failed": 12, "timeout": 1,
      "canary_routed": 108, "p50_wall_time_ms": 15, "p95_wall_time_ms": 220 }
  ]
}
```

`window_minutes` は **固定 60**（クエリパラメータを取らない）。理由: 素の `axum::extract::Query` は
rejection が統一エンベロープ（`error.rs`）を通らず、`extract.rs` が禁じる shape-divergence を増やすため。
可変窓は M8 以降の follow-up。

### 2.9 観測（M5 を壊さない additive 変更のみ）

#### `usage_rollups` は触らない
PK が `(tenant_id, period_start, component_id)` で version 次元が無い（「含まない」節に記載）。
canary 判断に必要な窓は「直近 30〜60 分」であり、日次粒度の rollup はそもそも判断材料にならない。

#### `executions` 生表 + 追加 index + 追加列

```sql
-- version_stats_for_component（GET /components/{id}/traffic）。$3 = window_minutes（固定 60）。
SELECT version_id,
       COUNT(*) FILTER (WHERE status = 'succeeded')::bigint AS succeeded,
       COUNT(*) FILTER (WHERE status = 'failed')::bigint    AS failed,
       COUNT(*) FILTER (WHERE status = 'timeout')::bigint   AS timeout,
       COUNT(*) FILTER (WHERE routing_reason = 'canary')::bigint AS canary_routed,
       percentile_cont(0.5)  WITHIN GROUP (ORDER BY wall_time_ms)::bigint AS p50_wall_time_ms,
       percentile_cont(0.95) WITHIN GROUP (ORDER BY wall_time_ms)::bigint AS p95_wall_time_ms
  FROM executions
 WHERE tenant_id = $1 AND component_id = $2
   AND status IN ('succeeded', 'failed', 'timeout')
   AND finished_at >= now() - ($3::int * interval '1 minute')
 GROUP BY version_id
```

`wall_time_ms` が全て NULL の版（DLQ/timeout のみ）では percentile が NULL になるので Rust 側は
`Option<i64>`。`invocation_count` の解釈は M5 と同じ（全終端で計上、リソース指標は計測済みのみ）。

#### Prometheus（既存メトリクスを 1 つも変えない）

`faas_executions_total`（`metrics.rs`）に version ラベルは**足さない**（既存ダッシュボード破壊 +
カーディナリティ爆発）。代わりに新規カウンタを 1 本だけ追加:

```rust
    /// M7a: canary ルーティングの選択結果のうち、**実際に JetStream へ publish された**もの。
    /// labels: reason (stable / canary)。version / tenant はラベルにしない（カーディナリティ）。
    pub canary_routed_total: IntCounterVec,   // &["reason"]
```

**inc の位置（レビュー指摘の反映）**: `resolve_version_for_enqueue` の中では inc **しない**。
HTTP 経路のこの呼び出しは admission の in-flight reserve（429）、presign 失敗、`PublishBackpressure` の
いずれよりも**前**にあり、弾かれた invoke まで canary として数えてしまう。cron 経路では
`EnqueueOutcome::IdempotentHit` も数えてしまう。canary が原因で 429 が増えている状況ほど
`executions.routing_reason` 由来の `canary_routed`（`GET /traffic`）との乖離が広がり、go/no-go 判断が
ノイズに埋もれる。

→ inc は `enqueue::enqueue_execution` が **`EnqueueOutcome::Enqueued` を返す直前**（publish ack 成功後）に
`req.routing_reason` を使って 1 箇所だけ行う。これで 3 起点すべてが自動的に、かつ「実際に起動した数」
だけが計上される。`README` には「配分の一次情報は `GET /components/{id}/traffic`（executions 直読み）で
あり、Prometheus カウンタは publish 済みジョブの内訳である」と明記する。

### 2.10 rollback の実効範囲（保証の境界を明示する）

版の解決は **enqueue 時に確定**し、`EnqueueRequest.version_id / version / wasm_url` がそのまま JobMessage と
job_token claim に焼き込まれる。JetStream の再配送は同一メッセージの replay なので、rollback 後も
**既に publish 済みの canary ジョブは canary 版で完走・再試行する**。

> **MUST（README 運用手順にも書く）**: rollback は**新規 enqueue のルーティングのみ**を変える。
> 既存 in-flight の最大 drain 時間 = `ACK_WAIT_SECS × MAX_DELIVER`（既定 30×5 = 150 秒）、
> `BACKOFF_SECS` 併用時（worker 既定 `[5,15,60]`）はその総和。DLQ 終端も canary 版として計上される。
> 「壊れた canary を即座に全停止する」ことは M7 の rollback では**できない**（それは M8 以降の
> in-flight kill / drain API のスコープ）。

この境界は chaos S1 の手順 (7) で実測して固定する（§7.3）。

### 2.11 non-HTTP 起点（cron / trigger / chain）

§2.5 のとおり 3 起点すべてが `enqueue::resolve_version_for_enqueue` を通る。合流点が `enqueue.rs` に
あるため、**新しい入口を足しても canary をバイパスできない**。

| 起点 | ルーティングキー | 決定性 | 備考 |
| --- | --- | --- | --- |
| HTTP invoke | `X-Faas-Routing-Key` → `Idempotency-Key` → `execution_id` | 再送・sticky で同一 | 冪等 fast-path は解決の**前**に返るので、再送は版を再抽選しない |
| Cron | `cron_idempotency_key(job_id, slot)` | slot ごとに決定的 | 複数 CP の競合（`lock_due_cron_job` の SKIP LOCKED）でも、どの CP が掴んでも同じ版 |
| event / chain | `execution_id` | 配送台帳（`trigger_deliveries` PK）が 1 回起動を保証 | 再配送は台帳で弾かれるので抽選も 1 回 |

- **低頻度 cron の観測性**: 1 slot = 1 起動なので weight=10% の canary は平均 10 slot に 1 回しか当たらない。
  「分布が偏っている」のではなく標本が少ないだけである旨を README に明記する（`slot` を含むキーの
  ハッシュなので、slot をまたげば擬似ランダムに散る）。
- **chain の版混在**: chain 下流の起動は finalize commit 後の best-effort 別 tx で、その時点の設定で解決する。
  canary 中に rollback すると「上流は canary 版、下流は stable 版」が 1 チェーン内で混ざる。これは
  **起動時点解決の設計上の帰結として許容**し、README にも明記する（「含まない」節）。

### 2.12 「canary 未設定なら現状と同じ」の根拠（3 層）

1. **DDL 層**: 追加列の既定は `canary_version_id IS NULL` / `canary_weight = 0`。
   両 CHECK は既存行（両方既定）で真。`routing_reason` は `NOT NULL DEFAULT 'stable'` なので backfill 不要。
2. **SQL 層**: `RESOLVE_ROUTING_SQL` の stable 側述語は現行 `active_version_storage` と**同値**
   （alias 改名 + 一貫データでは no-op の `component_id` narrowing、§2.4）。`LEFT JOIN` は行数を増やさない。
3. **選択層**: `select_version` は `canary = None` または `weight = 0` で必ず stable を返す
   （§7.2 の全列挙テストで証明）。

M7a-1 完了時点で `cargo test --workspace --exclude echo` が緑、かつ chaos_m4/m5/m6 が手元で緑であることが
「壊していない」ことの証明になる。

---

## 3. M7b — per-function 環境変数・設定

### 3.1 capability `env` 許可リスト（§4.4）— **admin 承認を迂回させない**

現状:
- `component_versions.capabilities`（`migrations/0002_m2.sql`）には `validation.rs` の `match_capabilities` が
  解決した**承認済み import 名の配列**が入るだけ。`validation.rs` のモジュール doc と `upload_version` の
  コメントが「クライアント宣言値（multipart `capabilities`）は**信用しない**。保存するのは
  `validated.approved_imports`」という不変条件を実装で保証している。
- worker の `WasiCtxBuilder` は env も preopen も一切与えていない。
- `wasi:sockets/` は baseline 非承認（`BASELINE_APPROVED_PREFIXES`）＝ゲストはそもそも socket を import できない。

**設計判断（レビュー指摘の反映・重要）**: `capabilities.env` の書き込みを `upload_version` から受け取っては
ならない。`POST /components/{id}/versions` は **Deploy スコープ**であり、仕様書 §4.4 は「付与（承認）は
`admin` スコープを要する (MUST)」と規定する。upload 経路で `env` を受け取ると、deploy スコープの CI トークンが
`env: ["PROD_API_KEY"]` を宣言した新 version を上げ、その wasm が `wasi:cli/environment`（baseline 承認済み）で
読んだ値を invoke 出力へ返すだけで、admin だけが書けるはずの secret を平文で取得できる
（`wasi:sockets/` が非承認でも**出力経路で足りる**）。これは権限昇格である。

したがって:

1. `capabilities` JSONB を **2 キー構造**へ拡張: `{"imports": [...既存の配列...], "env": ["API_KEY", "LOG_LEVEL"]}`。
   - **後方互換は backfill せず読み取り時に吸収**する: 値が配列なら `imports` とみなし `env` は空とする
     （`0008_m6.sql` の「NOT NULL + DEFAULT なら backfill 不要」と同じ additive 精神）。
   - パース不能な JSON は **deny-all にフォールバック**（`imports` 空 / `env` 空）。
2. `upload_version` が保存する値は **`{"imports": validated.approved_imports, "env": []}`（deny-all 既定）**。
   multipart の `capabilities` に `env` キーが含まれていたら **400**
   （`"capabilities.env is admin-approved; use PUT /components/{id}/versions/{version}/capabilities"`）。
   宣言値をログに残すだけの現行挙動（`tracing::debug!`）は維持する。
3. `env` 許可リストの書き込みは **admin 専用の別エンドポイント**（§4.5 の表）。
4. 注入時に **CP 側（job-env 引き換え）と worker 側（config の DB 直読み）の両方で**この許可リストで
   フィルタする（二重防御）。許可リストに無いキーは、値が DB に存在しても注入しない。
5. secret に「どの outbound ホスト用か」という binding 概念は**持たせない**。secret は単なる名前付き値であり、
   「どの API に使うか」は Component コードの責務。プラットフォームが保証するのは
   「**承認された env 名だけが注入面になる**」こと。
6. host interface 方式（`faas:secrets/store` 相当）は採らない。`validation.rs` のテスト
   `baseline_rejects_unapproved_host_import` が `assert!(!approved.approves("faas:secrets/store"))` で
   「secret 用ホスト interface は baseline 未承認」を現行契約として固定しており、これを緩めると
   §4.4 deny-all の境界が静かに後退する。ゲストは `wasi:cli/environment`（baseline 承認済み）を使う。
   → **baseline の変更は不要**。

この直交設計により、M9 で `net` capability が入ったとき `capabilities.net.allow_outbound` と
`capabilities.env` は独立に効き、M7 の実装を作り直さずに済む。

### 3.2 上限（`faas_shared` に定数を置き、CP のバリデーションと worker の防御的 clamp の両方で使う）

| 項目 | 値 | 根拠 |
| --- | --- | --- |
| キー名 | `^[A-Z_][A-Z0-9_]{0,63}$`（最長 64） | POSIX env 名の慣行。小文字・`=`・NUL を弾く |
| キー数 / component | 64（config + secret 合計） | `WasiCtx` 構築コストと運用可読性 |
| 値バイト長 | 4096（config / secret 共通） | secret は資格情報であり数 KiB 超の用途は M7 非スコープ |
| 注入 env 総バイト | 32768 | HTTP 応答と `WasiCtx` の上限 |

超過は `FaasError::InvalidRequest` → **400**（§0 の cross-cutting 決定 2）。

**キー名の衝突**: 同一 component で `function_configs.key` と `function_secrets.name`（生存行）が衝突したら
**409 Conflict で拒否**（どちらかが勝つ挙動を作らない）。「どちらが勝つか」を運用者が誤解すると、
secret を上書きしたつもりの平文 config が注入される事故になる。config の PUT / secret の PUT の両方で
相互排他を検査する。

### 3.3 API（per-function config）

| メソッド パス | スコープ | 説明 |
| --- | --- | --- |
| `GET /components/{component_id}/config` | **deploy** | `{"env": {"K":"V"}, "updated_at": ...}`。平文なので値を返す |
| `PUT /components/{component_id}/config` | deploy | 全置換。`{"env": {"K":"V"}}`。`PATCH` は既存作法に無いので使わない |
| `DELETE /components/{component_id}/config/{key}` | deploy | キー単位削除 |

**`GET /config` を read ではなく deploy に置く判断（レビュー指摘の反映）**: config は平文であり、注入時に
secret と同じ env 名前空間へ混ざる。運用者が資格情報を誤って config 側に入れる確率は現実的に高く、その瞬間
read スコープが資格情報の読み取り権限になる。read（監視・ダッシュボード用途で最も広く配られるスコープ）から
1 段引き上げることで被害面を縮める。書き込みが deploy なので読み書きが同一スコープになり運用も単純。
**README の露出ガード節に「config は平文であり deploy スコープで読める。資格情報は必ず secrets 側に置くこと」
を明記する**。

`deploy` に置く根拠（書き込み側）: per-function 設定は component ライフサイクル相当で、CI/CD が回す対象
（`create_cron_job` / `create_trigger` を deploy に置いた判断と同じ）。

監査 action: `function_config_updated` / `function_config_deleted`。

### 3.4 注入経路（平文 config）— worker が DB 直読み、往復ゼロ

`resolve_limits`（`crates/worker/src/main.rs`）の SELECT に `cv.capabilities` を足し、**同じ tx で**もう 1 本
`function_configs` を引く。`set_tenant_guc` + `WHERE tenant_id=$1` の二重防御という既存パターンをそのまま
踏襲する。追加の信頼境界も HTTP 往復もゼロ。

```sql
SELECT c.id AS component_id, cv.resource_limits, cv.capabilities
  FROM component_versions cv JOIN components c ON c.id = cv.component_id
 WHERE c.tenant_id = $1 AND c.name = $2 AND cv.version = $3 LIMIT 1;

SELECT key, value FROM function_configs WHERE tenant_id = $1 AND component_id = $2;
```

`resolve_limits` はこれに伴い `ResolvedVersion { component_id, limits, allowed_env }` を返す形へ拡張する
（現行の `Ok(ResourceLimits::default())` フォールバックは維持。**ただし行が引けなかった場合の
`allowed_env` は空 = deny-all** とする）。

### 3.5 wasmtime への注入点

- **`crates/worker/src/main.rs` の `let wasi = WasiCtxBuilder::new().inherit_stderr().build();`** が唯一の
  注入点（`run_component` 内、`StoreLimits` 構築の直後）。現状 env は一切渡していない。
- `run_component` のシグネチャに `env: Vec<(String, String)>` と `contain_stderr: bool` を追加。
- `execute` で `resolve_limits` の直後に env を解決し、`run_component` へ渡す。
- `Component` は sha256 キャッシュ共有だが `WasiCtx` は実行ごとに構築されるため、**テナント混線は
  構造的に起きない**（この不変条件を実装コメントに書く）。

**stderr の扱い（レビュー指摘の反映・重要）**:
`inherit_stderr()` はゲストの stderr を worker プロセスの stderr（＝コンテナログ、全テナント共有の
ストリーム）へ直結させる。env 注入を入れた瞬間、ゲストが自分の env を print する／ライブラリが起動時に
設定をダンプする、のいずれでも平文 secret が**プラットフォームのログ**に落ちる。§5 は CP 側の tracing を
塞ぐが、この経路は別物である。

> **MUST**: **secret を 1 件以上注入する実行では `inherit_stderr()` を使わない。**
> `wasmtime_wasi::pipe::MemoryOutputPipe`（上限付き。既定 64 KiB）へ差し替え、その内容は
> **ログにも `executions.error` にも載せない**（`Drop` で捨てる）。捨てたバイト数だけを
> `faas_guest_stderr_dropped_bytes_total`（ラベルなし）で観測する。
> 平文 config のみ（secret ゼロ）の実行は従来どおり `inherit_stderr()` でよい。
> この分岐と根拠を `run_component` の実装コメントに書く。

対応して §8.2 の chaos 手順（`docker compose logs | grep -c "<sentinel>"` == 0）が実運用でも破れなくなる。

### 3.6 echo Component の拡張（chaos の前提）

`components/echo/src/lib.rs` は入力をそのまま `{"echo": v}` で返すだけで env を観測できない。
出力に `env` フィールド（`wasi:cli/environment` から読んだ**キー名でソートした (name, value) の一覧**）を
足す（既存の `echo` フィールドは壊さない）。`.github/workflows/ci.yml` の component ジョブがそのまま
ビルド + `wasm-tools validate` を回すので追加コストは小さい。

> 注: これは「secret を出力に晒すテスト用コンポーネント」である。README の chaos 節に
> 「echo は検証用に env を出力する。実運用の Component をこう書いてはならない」と明記する。

---

## 4. M7c — Secrets Manager（§10）

### 4.1 AEAD の選定

- **リポジトリに AEAD は存在しない**。`crates/control-plane/Cargo.toml` の暗号依存は sha2 / ed25519-dalek /
  argon2 / rand の 4 本で、`Cargo.lock` に `aes-gcm` / `chacha20poly1305` / `aead` は無い。一方
  `hkdf` / `hmac` / `subtle` / `zeroize` は既に推移的に存在する（`Cargo.lock` に entry あり）。
  → **AEAD の新規依存は不可避**。
- 採用: **`chacha20poly1305`（RustCrypto）の `XChaCha20Poly1305`**。
  - pure-Rust。ルート `Cargo.toml` が TLS で `rustls-ring` を明示選択して `aws-lc-sys` / `cc` を回避している
    方針に整合する。
  - ソフトウェア AES（AES-NI 非依存環境）はタイミング面で不利。ChaCha20 は定数時間実装が素直。
  - **XChaCha**（24 バイト nonce）を選ぶ理由: nonce を毎回 `OsRng` でランダム生成しても衝突確率が
    無視できる。12 バイト nonce だとカウンタ管理（＝状態）が必要になり、ステートレス×N の CP と噛み合わない。
- `zeroize` を workspace 依存へ昇格する（`Cargo.lock` に既存なので新規ダウンロードは発生しない）。

### 4.2 封筒（envelope）構成

```
SECRETS_MASTER_KEY (env, 32B) ── kid で選択 ──> KEK
                                                 │  AEAD(KEK, dek_nonce, DEK, aad=dek_aad)
                                                 ▼
                              行ごとにランダムな DEK (32B, OsRng)
                                                 │  AEAD(DEK, nonce, plaintext, aad=value_aad)
                                                 ▼
                                            ciphertext
```

**なぜ鍵導出（HKDF only）ではなく真の封筒か**: 版台帳が追記専用なので「新 version 行を INSERT」で再ラップ
する形になり、その際 **値の平文をメモリに載せずに KEK を回せる**（DEK を unwrap → 新 KEK で wrap するだけ）。
これが封筒の実利である。ただしこの性質は §4.7 で述べるとおり**侵害復旧にはならない**。

### 4.3 AAD（cut-and-paste 攻撃の遮断）

`faas_shared::job_claims_signing_bytes` と**同じ正準化作法**（ドメインタグ + 4 バイト BE 長さ前置 + UTF-8）を
再利用し、`secrets::aad_bytes` を書く。serde_json は使わない（map/key 順序が非正準で signer/verifier 間
ドリフトを生む、という `lib.rs` の理由がそのまま当てはまる）。

| 用途 | ドメインタグ | 含めるフィールド | 防ぐ攻撃 |
| --- | --- | --- | --- |
| 値本体 (`ciphertext`) | `b"faas-secret-value-v1"` | `tenant_id`, `component_id`, `name` | DB 書き込み権限を得た攻撃者が、他テナント / 他 component / 他キー名の行へ暗号文を貼り替える（貼り替えると Poly1305 で復号失敗） |
| DEK ラップ (`wrapped_dek`) | `b"faas-secret-dek-v1"` | `tenant_id`, `secret_id`, `version`(u32 BE), `kek_kid` | ラップ済み DEK の他行への転用・version の巻き戻し |

- **`value_aad` に `version` と `kek_kid` を入れない**のは意図的: 再ラップ（`reason='rekey'`）で
  `ciphertext` をそのままコピーできるようにするため。version の巻き戻し耐性は `dek_aad` 側が担う。
- `component_id` を `value_aad` に含めるため、**secret を別 component へ移すことはできない**（仕様として明示。
  移設は「新しい secret として登録し直す」）。

### 4.4 キーリングと config への追加

`signing.rs` の `Signer`（現行鍵 1 本 + kid）と `Verifier`（kid -> 鍵の HashMap）の 2 段構成をそのまま写す。

```rust
/// KEK キーリング (§10)。暗号化は常に active_kid、復号は kid で選ぶ（rotation-ready）。
pub struct SecretKeyring {
    active_kid: String,
    keys: HashMap<String, Zeroizing<[u8; 32]>>, // active + retired（復号専用）
}
// Debug は kid の一覧のみ（鍵素材・鍵長・先頭バイトも出さない）。
impl std::fmt::Debug for SecretKeyring { /* keys.keys() のみ */ }
```

env（`config.rs` の `const DEFAULT_*` → `struct Config` → `from_env()` の 3 点セットに
`// --- M7 デプロイ運用 / Secrets（§10 / §15） ---` 見出しで追加）:

| env | 既定 | 用途 |
| --- | --- | --- |
| `SECRETS_MASTER_KEY` | **必須** | 現行 KEK seed 32 バイト。hex / base64url / base64。`signing::decode_seed` を `decode_key32(raw, env_name)` へ一般化して再利用（現状は `"JOB_SIGNING_KEY"` がエラーメッセージにハードコード） |
| `SECRETS_MASTER_KID` | **必須** | 新規暗号化に使う kid。`job_signing_key` / `job_signing_kid` の隣が定位置 |
| `SECRETS_RETIRED_KEYS` | 空 | 復号専用の旧 KEK。`kid:seed,kid:seed` の CSV（worker の `parse_backoff_secs` と同型のパーサ）。再ラップ完了まで残す |
| `INTERNAL_BIND_ADDR` | `127.0.0.1:8081` | **内部専用 listener**（§4.6）。`POST /internal/job-env` だけを載せる |
| `JOB_ENV_EXCHANGE_RATE_PER_MIN` | `600` | `/internal/job-env` の per-IP 上限（無認証面のグローバル保護） |
| `MAX_FUNCTION_ENV_KEYS` | `64` | component あたり config + secret 合計のキー数上限 |
| `MAX_FUNCTION_ENV_VALUE_BYTES` | `4096` | 1 値の上限（config / secret 共通） |
| `MAX_FUNCTION_ENV_TOTAL_BYTES` | `32768` | 注入 env の総バイト数上限 |

worker 側: `CONTROL_PLANE_INTERNAL_URL`（必須。`INTERNAL_BIND_ADDR` を指す）、
`JOB_ENV_FETCH_TIMEOUT_MS`（既定 2000）を `Settings::from_env()` に追加。

**`.env.example` の扱い（レビュー指摘の反映）**: `SECRETS_MASTER_KEY` に `JOB_SIGNING_KEY` と同じ要領で
「決定的ダミー」を置くと、公開リポジトリに載る既知鍵で開発/ステージングの secret が暗号化される。
署名鍵と違い**暗号文は DB に永続する**ので影響が長い。さらに `Makefile` は `include .env` + `export` するため
値が全 make 子プロセスへ export される。

> **MUST**: `.env.example` の値は明らかなプレースホルダ
> （`SECRETS_MASTER_KEY=CHANGE_ME_REPLACE_WITH_32_BYTE_KEY_BEFORE_USE`）とし、
> `Config::from_env()` は **この既知プレースホルダと一致したら起動失敗**させる（warn ではない）。
> `.env.example` には既存の `JOB_SIGNING_KEY` ブロックと同じ「露出ガード注記」と
> 「値の後ろに行内コメント禁止」注記を複写する。README トラブルシュートに移行手順を書く。

### 4.5 API（secrets）

| メソッド パス | スコープ | 説明 |
| --- | --- | --- |
| `PUT /components/{id}/versions/{version}/capabilities` | **admin** + `require_admin_role` | §3.1 の `env` 許可リスト承認。`{"env": ["API_KEY","LOG_LEVEL"]}` 全置換。`imports` は受け付けない（検証器が権威）。監査 `capability_env_approved` |
| `GET /components/{component_id}/secrets` | **read** | **メタデータのみ**: `[{"name","version","has_value","updated_at"}]`。**値を返す経路をコードに作らない** |
| `PUT /components/{component_id}/secrets/{name}` | admin + role | `{"value":"..."}`。新規なら 201 + version=1、既存なら 200 + 新 version。応答は `{"name","version"}` のみ |
| `POST /components/{component_id}/secrets/{name}/rotate` | admin + role | `{"value":"..."}`。実装は PUT と同じだが監査 action が `secret_rotated`（監査の意図が違うので分ける） |
| `DELETE /components/{component_id}/secrets/{name}` | admin + role | soft delete。版台帳は追記専用なので残る。同名の再作成は部分 UNIQUE index により可能（§1.8） |
| `GET /components/{component_id}/secrets/keys` | admin + role | 運用者向け: `[{"name","version","kek_kid","value_len"}]`。**kek_kid / value_len はここだけ** |
| `POST /admin/secrets/rekey` | admin + role | **当該テナントのみ**を現行 kid で再ラップ。応答 `{"rewrapped": n}` |
| `POST /internal/job-env` | 認証 middleware 対象外・**内部 listener 専用** | env-token の署名そのものが認証（§4.6） |

**メタデータ露出の設計判断（レビュー指摘の反映）**: `GET /secrets` から `value_len` と `kek_kid` を外す。
`value_len` は平文長のオラクルであり、短い PIN やパターンの決まった資格情報では有意な情報になる。
read スコープには `has_value: bool` だけを見せ、運用に必要な `kek_kid` / `value_len` は
admin 専用の `GET /secrets/keys` へ移す。

**`POST /admin/secrets/rekey` の応答から `remaining_by_kid` を外す（レビュー指摘の反映・blocker）**:
本リポジトリの admin は**テナント管理者**であり、全テナント横断の集計を返すと他テナントの secret 総数が
漏れる。`{"rewrapped": n}` のみを返し、kid 別の全体像は Prometheus gauge（内部更新）でのみ観測する。
呼ぶ SECURITY DEFINER 関数も `secrets_stale_kek(p_tenant, p_active_kid)`（テナント引数必須）を使う（§1.8）。

その他の規約:
- 書き込み系は **`admin` スコープ + `require_admin_role` の二重ガード**（`create_user` / `create_token` と
  同じ多層防御）。根拠: §4.4 が「付与（承認）は `admin` スコープを要する (MUST)」と規定し、
  outbound 認証情報の管理はこの「付与」に含まれると解する。
- 入力抽出は全て `JsonBody<T>`。クエリパラメータは増やさない。
- `component_id` の所有検査は `set_tenant_guc` + `WHERE tenant_id` の二重防御で行い、不在は **404**
  （既存の存在秘匿の作法。403 にしない）。
- 停止テナントの遮断は `auth::authenticate` が `tenants.status` を見るので middleware 配下では自動。
  `/internal/job-env` は middleware 外なので §4.6 の MUST で個別に確認する。

エラー写像（`error.rs` の既存 variant のみ使う。新 variant は追加しない —— 追加すると
`code()` / `status()` / `retryable()` の 3 match すべての更新が必要）:

| 事象 | FaasError | HTTP |
| --- | --- | --- |
| キー名不正 / 値長超過 / 個数超過 / 許可リスト外のキー / `capabilities.env` を upload に混ぜた | `InvalidRequest` | 400 |
| component 不在 / secret 不在 / 他テナントの id | `NotFound` | 404 |
| config と secret のキー衝突 | `Conflict` | 409 |
| 復号失敗 / 未知 kid | `Internal("")`（詳細はログの安定 reason のみ） | 500 |
| env-token 署名不正 / exp 切れ / aud 不一致 / 行不一致 | `Unauthorized` | 401 |
| execution が終端済み / テナント停止中 | `Forbidden` | 403 |

### 4.6 注入経路 — 4 案の比較と採用案

| 案 | 内容 | worker が鍵を持つか | NATS/JetStream への平文永続化 | 判定 |
| --- | --- | --- | --- | --- |
| A | CP が復号して `JobMessage` に平文 env を同梱 | 持たない | **する**（`FAAS_INVOKE` は既定 `StreamConfig` ＝ `max_age` 無し・File storage。`JobMessage` は `derive(Debug)` でログ経路も） | 却下 |
| B | worker が DB から暗号文を読んで自前で復号 | **持つ（KEK）** | しない | 却下 |
| C | CP が S3 に置いて短命 presigned GET（`input_url` 同型） | 持たない | しない（代わりに **S3 に平文残留**） | 却下 |
| **D（採用）** | worker が短命トークンを提示して CP の**内部専用**エンドポイントから引き換える | 持たない | しない | **採用** |

- **B は却下**: worker は未信頼 wasm を同一プロセスで実行するランタイムであり、そこに全テナントの KEK を
  配ることは §3.3 の keyless-by-design（`worker/main.rs` のコメント、`shared/lib.rs` の「worker は job_token を
  opaque として echo するだけ」）に正面から反する。wasm エスケープ 1 件で全テナントの secret が平文化する。
- **A は却下**: 平文 secret が**ディスクに無期限に残る**。完了条件「ログ・監査に漏れない」に対して恒久的な
  漏洩面を新設する。加えて `LOG_FORMAT=json` の `with_current_span(true)` と組むと 1 行の `debug!(?job)` で
  全ログに載る。
- **C は却下**: 削除ライフサイクルという新しい失敗モード（消し忘れ＝残留）を増やす。

#### 4.6.1 採用案 D の 3 つの必須修正（レビュー指摘の反映・blocker）

**(1) 内部専用 listener に載せる。公開 listener には生やさない。**

当初案の「`main.rs` の認証除外ルート群（`/healthz` `/readyz` `/metrics` `/auth/login` `/admin/tenants`）へ
追加」は、`BIND_ADDR`（既定 `0.0.0.0:8080`）＝インターネット到達しうる同一 listener に無認証の secret
引き換え口を生やすことを意味する。

> **MUST**: CP は axum サーバを 2 本立てる。worker の `METRICS_BIND_ADDR`（既定 `0.0.0.0:9090`、内部ネット
> 限定前提）と同型に、CP へ `INTERNAL_BIND_ADDR`（既定 `127.0.0.1:8081`）を新設し、`POST /internal/job-env`
> は**そこにだけ**マウントする。公開ルータ（`build_router`）には一切追加しない。
> `build_internal_router(state)` を別関数にし、doc に「このルータは公開してはならない」と書く。

**(2) `job_token` を引き換えに流用しない。専用の env-token を新設する。**

`job_token` は `JobMessage` だけでなく **`ResultMessage` と `FailedMessage` にも verbatim に echo される**
（`crates/shared/src/lib.rs` の両構造体の `job_token` フィールド、worker の publish 経路）。つまり
FAAS_RESULT / DLQ / reply subject の購読権しか持たない主体（案 A では平文 secret を得られない）が、
job_token 流用の案 D では引き換えトークンを得て secret を読める。監査面が 1 ストリームから 3 ストリーム +
reply へ広がる。

> **MUST**: `JobMessage` に **`env_token: Option<String>`** を新設する（`#[serde(default,
> skip_serializing_if = "Option::is_none")]` で後方互換）。claim は
> `EnvClaims { execution_id, tenant_id, version_id, component_id, aud: "job-env", kid, iat, exp }` で、
> 署名バイトは `job_claims_signing_bytes` と**別のドメインタグ** `b"faas-env-token-v1"` を使う
> （同じ正準化作法・4 バイト BE 長さ前置）。鍵は既存の `Signer`（`JOB_SIGNING_KEY`）を流用してよいが、
> ドメインタグと `aud` により job_token とは相互に使い回せない。
> **worker は `env_token` を result / DLQ / reply へ echo してはならない (MUST NOT)。**
> フィールド doc にこの禁止を書き、`shared/lib.rs` の `reply_to`（「揮発フィールドで DB に永続化しない」）と
> 同型の宣言にする。CP は enqueue 時、当該 component に生存する secret が 1 件以上あるときだけ `Some` を載せる
> （secret 名も件数も wire に載せない。`needs_secrets: bool` は不要 —— `env_token` の有無がそのシグナルになる）。

**(3) 引き換えハンドラ内で `tenants.status = 'active'` を明示確認する。**
middleware 外なので `auth::authenticate` の停止テナント遮断が効かない。これは「実装チェックリスト」ではなく
**設計書本文の MUST** とする。

#### 4.6.2 引き換えの検証手順（順序も規約）

`POST {CONTROL_PLANE_INTERNAL_URL}/internal/job-env`、body `{"env_token": "..."}`。CP 側:

1. per-IP レート制限（`JOB_ENV_EXCHANGE_RATE_PER_MIN`）。**無認証面なのでこれが最初**。
2. `Verifier::verify` 相当で署名検証（`aud == "job-env"` を要求。不一致は 401）。
3. `exp` 検査（`config.rs` の `token_exp_offset_secs` 由来で有限。既定 `210 + wall` 秒）。
4. `tenants.status = 'active'` を確認（**MUST**、上記 (3)）。停止中は 403。
5. `set_tenant_guc(claims.tenant_id)` した tx を開始（以降すべて RLS 下）。
6. `executions` 行を `claims.execution_id` で引き、`tenant_id` / `version_id` / `component_id` が claim と
   一致し、`status` が `pending` / `running` であることを要求（終端済みは 403）。
   `subscriber.rs` が result 側で行っている claim ↔ 行の突き合わせと同じ思想。
7. テナント名前空間のレート制限（§5.5 の `rate_limit_ns("env", tenant, ..)`）。JetStream 再配送
   （`max_deliver` 既定 5）を許容するため single-use にはしない。
8. `claims.version_id` から `capabilities.env` 許可リストを解決（パース不能なら deny-all）。
9. `function_configs` と `function_secrets` を許可リストで絞り、secret のみ復号。
10. `{"env": {"NAME": "value", ...}}` を返す。
11. 監査: 成功で `action="secret_material_issued"`, `target=execution_id`,
    `detail={"names":[...], "count":n}`（**値は載せない**）。失敗は `action="secret_material_denied"` +
    安定 reason。

worker 側の失敗時（CP 不達 / 401 / 403 / 復号失敗 / 許可名の解決不能）は **fail-closed**:
`ExecError::Failed("secret material unavailable")` で終端させる。**secret 欠損のまま実行してはならない。**
JetStream の backoff 再配送が自然にリトライになる。

#### 4.6.3 「NATS 上を env_token が流れる」ことの緩和（多層）

1. **不変条件**: `JobMessage` に平文 secret / KEK / DEK を載せない (MUST NOT)。`env_token` 以外に
   secret 由来の情報を wire に出さない。
2. `env_token` は `JobMessage` にのみ載り、result / DLQ / reply へ echo されない（§4.6.1 (2)）。
   → 漏洩面は FAAS_INVOKE stream の購読権に限定される（案 A ではその同じ相手が平文を直接得る）。
3. NATS の認証・TLS・最小権限は §3.3 の既存要件。M7 はこれを前提とし、前提が崩れている環境への展開を
   README の露出ガードで禁じる。
4. **時間窓**: `exp` が有限。引き換え側でも検査する。
5. **実行の生存確認**（§4.6.2 手順 6）。
6. **回数上限**（per-IP + per-tenant 名前空間）。
7. **監査**（値を載せない detail 構築点を 1 本に絞る、§5.3）。

### 4.7 世代の固定と、鍵ローテーションの意味論

#### 4.7.1 注入する secret 世代は **execution に固定する**（レビュー指摘の反映）

当初案の「引き換え時点の `function_secrets.current_version` を解決する」は、worker がクラッシュして
再配送される間に `POST .../rotate` が走ると、同一 `execution_id` の 1 回目の試行は旧値、2 回目は新値で走る。
at-least-once なので**両方の資格情報が外部 API に到達しうる**（rotation の目的である「旧鍵を止める」が
保証されず、旧鍵の最終使用時刻も特定できない）。版台帳が追記専用で旧世代が残るのに、その利点を注入側が
使っていない。

> **MUST**: 引き換えは `current_version` ではなく **execution 基準**で解決する。
>
> ```sql
> SELECT v.version, v.kek_kid, v.wrapped_dek, v.dek_nonce, v.nonce, v.ciphertext
>   FROM function_secrets s
>   JOIN LATERAL (
>        SELECT * FROM function_secret_versions v2
>         WHERE v2.tenant_id = s.tenant_id AND v2.secret_id = s.id
>           AND v2.created_at <= $3            -- executions.created_at
>         ORDER BY v2.version DESC LIMIT 1
>   ) v ON TRUE
>  WHERE s.tenant_id = $1 AND s.component_id = $2
>    AND s.deleted_at IS NULL AND s.name = ANY($4)   -- 許可リスト
> ```
>
> `JOIN LATERAL ... ON TRUE` は INNER 相当なので、**version 行が 1 つも解決できない secret は行が返らない**。
> 「生存 secret 行があるのに世代が解決できない」ケースは呼び出し側が検出して **fail-closed**
> （`SecretError::VersionUnresolved` → 500 / worker は `secret material unavailable`）。
> `reason='rekey'` 行は同一平文なので選ばれても等価。

不変条件は「同一 execution の再配送は**同一世代**を返す」と書き換える（§6 の表）。

#### 4.7.2 KEK ローテーション — signing の overlap と意味が違う

仕様書 §3.3 の signing overlap は「旧 kid で発行されうるトークンの最大有効期限まで」＝ **TTL で有限時間に
終わる**。一方 secret は **DB の暗号文が旧 kid のまま永続する**。`signing.rs` の作法をそのまま写すと
「TTL 経過＝撤去可」と誤解して復号不能事故を起こす。

M7 の規約（README / 本書に明記する）:

1. `SECRETS_MASTER_KID` を新 kid に切り替え、旧 kid は `SECRETS_RETIRED_KEYS` へ移す
   （この時点で新規書き込みは新 kid、既存は旧 kid で復号可能）。
2. `POST /admin/secrets/rekey`（同期・**テナント単位**）または背景ジョブ（`scheduler.rs` / `reaper.rs` と
   同型のループ、`secrets_stale_kek_all(active_kid)` を使う）が対象を引き、各テナント `set_tenant_guc` した
   tx で `reason='rekey'` の新 version を INSERT → `current_version` を前進。
3. メトリクス `faas_secret_versions_by_kid{kid}`（`secrets_kek_kid_counts_all()` 由来・**内部更新専用**）が
   旧 kid で **0 になったことを確認してから**、`SECRETS_RETIRED_KEYS` から旧 kid を外す。
4. 復号時の未知 kid は `SecretError::UnknownKid` → 安定文字列 `"unknown_kid"`（`signing.rs` の
   `VerifyError::UnknownKid` と同型）で監査 + ログ。値も kid 以外の識別子も出さない。

#### 4.7.3 rekey は侵害復旧では **ない**（明示的な二択）

`reason='rekey'` の再ラップは `ciphertext` / `nonce` / `value_len` を前 version からコピーし、
`wrapped_dek` / `dek_nonce` / `kek_kid` だけを更新した新行を INSERT する（`value_aad` に `version` と
`kek_kid` を含めないのはこのため、§4.3）。**DEK も ciphertext も不変**なので、
**旧 KEK と旧 DB ダンプを持つ攻撃者は再ラップ後も全平文を復元できる**。

| モード | 内容 | 平文をメモリに載せるか | 前方秘匿性 | M7 での扱い |
| --- | --- | --- | --- | --- |
| **wrap-only rekey（採用）** | DEK を unwrap → 新 KEK で wrap。ciphertext 不変 | **載せない** | 無し（旧 KEK + 旧ダンプで復号可能） | **M7 で実装** |
| deep re-encryption | DEK を新規生成し平文を再暗号化 | 載せる（一時的） | 有り | **M8 follow-up**（「含まない」節） |

wrap-only を採る理由: (a) 平文を触らずに KEK 寿命管理ができるという封筒暗号本来の利点を、実装量最小で
得られる。(b) deep 版は「復旧のために全 secret の平文を CP メモリへ載せる」ことになり、そのバッチ処理自体が
新たな露出面になる。M7 の完了条件は「secret が漏れない」であって「侵害後の前方秘匿」ではない。

> **MUST（README の露出ガード節に書く）**: `rekey` は **KEK の計画的ローテーション（鍵寿命管理）専用**で
> あり、**KEK 侵害時の復旧手段ではない**。旧 kid 件数が 0 になっても侵害からは何も復旧していない。
> KEK 侵害時の唯一の復旧経路は、**全 secret を `rotate` で新しい値に差し替えること**
> （＝外部サービス側で資格情報を発行し直す）である。

---

## 5. 露出ガード — 構造的保証（M7-0）

現状、リポジトリに **マスキング機構は一切存在しない**（`redact` / `mask` / `zeroize` の grep ヒットは
`error.rs` の 5xx redaction のみ）。`CreateTokenResponse`（`handlers.rs`）が既に平文 secret を持ちながら
`#[derive(Debug, Serialize)]` している。M7 ではこれを型で塞ぐ。

### 5.1 `faas_shared::Redacted<T>`

```rust
/// 秘密値のラッパ (§7 / §10)。Debug/Display は常に "<redacted>"。
///
/// **Serialize を実装しない**（うっかり API 応答型へ入れるとコンパイルエラーになる）。
/// 平文の取り出しは expose() の 1 経路のみ = grep で監査できる。
///
/// 境界を **構造体宣言側** に書くこと: Rust は Drop 実装に構造体宣言と同一の境界を要求するため、
/// `pub struct Redacted<T>(T);` + `impl<T: Zeroize> Drop for ...` は E0367 でコンパイルできない。
/// String / Vec<u8> / [u8; 32] は zeroize が実装済み。
#[derive(Clone)]  // Clone は秘密を複製する。呼び出し箇所を最小に保つこと（doc に明記）。
pub struct Redacted<T: Zeroize>(T);

impl<T: Zeroize> Redacted<T> {
    pub fn new(v: T) -> Self { Self(v) }
    /// 平文を取り出す。**呼び出しは allowlist ファイルのみ**（rls-lint (4) が検査）。
    pub fn expose(&self) -> &T { &self.0 }
}
impl<T: Zeroize> std::fmt::Debug   for Redacted<T> { /* "<redacted>" */ }
impl<T: Zeroize> std::fmt::Display for Redacted<T> { /* "<redacted>" */ }
impl<T: Zeroize> Drop              for Redacted<T> { fn drop(&mut self) { self.0.zeroize(); } }
```

> 注: `Drop` を実装すると値のムーブアウトができなくなるため `into_inner()` は提供しない
> （`expose(&self) -> &T` のみ）。これは意図した制約であり、平文の取り出し口を 1 本に保つ。

適用先:
- `Config` の秘密フィールド（`bootstrap_admin_token` / `s3_secret_key` / `job_signing_key` / 新
  `secrets_master_key` / `secrets_retired_keys`）。`Config` は現在 `#[derive(Debug, Clone)]` なので、
  **`Debug` を手動実装**して秘密フィールドを `<redacted>` にする（`Redacted` を入れるだけでも Debug は
  塞がるが、`Config` 全体の Debug 契約を明示するため手動実装を選ぶ）。`main.rs` は現状 `Config` を `{:?}` して
  いない（個別フィールドのみログ）ので**今なら無コストで塞げる**。
- `CreateTokenResponse.secret`（`handlers.rs`）、`LoginResponse` の token（`login.rs`）、
  secret の PUT リクエスト body。
- 「一度だけ返す」ケース（login / token 発行）は `#[serde(serialize_with = "faas_shared::expose_once")]` と
  いう**明示的で grep 可能な**フィールド属性を必須にする。`Redacted` 自体には `Serialize` を実装しない。

### 5.2 復号 API を 1 本に絞る

`secrets.rs` が公開する復号系 API は次の 1 本だけにする（それ以外は `pub(crate)` にもしない）:

```rust
/// job-env 引き換え専用。許可リストで絞った secret のみを、**execution 基準の世代**で復号して返す。
pub(crate) async fn resolve_for_injection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, // set_tenant_guc 済み
    tenant_id: &str,
    component_id: &str,
    execution_created_at: chrono::DateTime<chrono::Utc>,
    allowed: &std::collections::BTreeSet<String>,
) -> Result<Vec<(String, Redacted<String>)>, SecretError>;
```

**関数名は本書内で 1 つに統一する**（rls-lint の `fns=` にもこの名前を書く。当初案は §5.2 と §5.6 で
`resolve_for_injection` / `load_secret_versions_for_injection` に食い違っていた）。

モジュール doc に「`auth::hash_token`（sha256 の**不可逆**ハッシュ）は再利用できない。secret は復号して
worker に渡す必要があるため別系統である」と明記する（`0003_auth.sql` の一方向ハッシュと混同させない）。

### 5.3 監査ログ

`db::insert_audit_log` の `detail: Option<&Value>` は任意 JSON を受け取れてしまう。
→ `secrets::audit_detail(name: &str, version: i32, reason: &str) -> Value` を**唯一の detail 構築点**にし、
**値を受け取らないシグネチャ**にする。生値が `insert_audit_log` に到達する型経路を消す。

action 語彙:
`secret_created` / `secret_rotated` / `secret_deleted` / `secret_rekeyed` /
`secret_material_issued` / `secret_material_denied` /
`function_config_updated` / `function_config_deleted` / `capability_env_approved` /
（M7a）`traffic_split_updated` / `traffic_split_cleared` / `version_promoted` / `version_rollback` /
`active_version_switched`。
既存 `token_issued`（detail は `{scopes, user_id}` のみ）と同型。

### 5.4 エラー

`error.rs` の `From<anyhow::Error>` は `e.to_string()` を `FaasError::Internal` に詰め、
`tracing::error!(error = %self.0)` としてログに出す。**secret 経路は anyhow を経由させない**。

```rust
pub enum SecretError { UnknownKid, BadEnvelope, DecryptFailed, KeyMissing, VersionUnresolved }
impl SecretError { pub fn reason(&self) -> &'static str { /* "unknown_kid" 等の安定文字列 */ } }
```

`signing.rs` の `VerifyError` + `reason()` と同型。HTTP へは `FaasError::Internal(String::new())` に潰す
（`From<sqlx::Error>` が詳細をログのみに残して空 Internal にするのと同じ扱い）。5xx ボディは既存の
redaction で「internal server error」になる。

### 5.5 tracing / メトリクス / URL / レート制限

- secret 系 handler は `#[tracing::instrument(skip_all)]`（invoke と同型）。`LOG_FORMAT=json` は
  `with_current_span(true)` で span フィールドを全イベントに伝播させるため、素の
  `#[tracing::instrument]` は**禁止**。
- **secret 名は path、値は必ず body**。`TraceLayer::new_for_http()` は URI（クエリ文字列込み）を span に載せる。
- メトリクスのラベルに secret 名も値も入れない。`faas_secret_material_issued_total`（ラベルなし）、
  `faas_secret_versions_by_kid{kid}`（kid のみ）、`faas_guest_stderr_dropped_bytes_total`（ラベルなし）。
  既存 `executions_total` がラベル `status` のみである作法に揃える。
- **レート制限の名前空間を型で分ける（レビュー指摘の反映）**: 当初案の「`Store::rate_limit` に
  `"env:{tenant}"` を渡す」は `rate_key(tenant) = format!("rl:{tenant}")` により `rl:env:{tenant}` になる
  文字列結合ヒューリスティックで、名前空間の型的保証が無い。`Store` trait に
  `rate_limit_ns(namespace: &str, key: &str, params, now_ms)` を足し、既存 `rate_limit` はその
  `namespace="invoke"` 版として実装する（`InProcStore` / Redis / Noop の 3 実装に波及するが、
  文字列結合より安全）。`/internal/job-env` は `namespace="env"` を使い invoke 予算を食わない。
  加えて **per-IP の固定上限**（`JOB_ENV_EXCHANGE_RATE_PER_MIN`）を無認証面の保護として入れる。
- `Makefile` の secret 系ターゲットは **body を echo しない**（既存 deploy 系ターゲットの
  `echo "    $$BODY"` を**意図的に踏襲しない**。HTTP コードのみ表示し、逸脱理由をコメントに書く）。
  `include .env` + `export` により `.env` の値が全ターゲットの子プロセスへ export される点も README に注記。

### 5.6 CI で効く静的ガード（`scripts/rls-lint.sh`）

- **(2) の `fns=` に M7 の tenant スコープ関数を追記**（M6 の追記が前例）:
  - M7a（§2.6 の 7 本）
  - M7b: `upsert_function_config|list_function_configs|delete_function_config|approve_capability_env`
  - M7c: `insert_secret_meta|bump_secret_current_version|insert_secret_version|list_secrets_meta|
    soft_delete_secret|resolve_for_injection`
  - `secrets_stale_kek` / `secrets_stale_kek_all` / `secrets_kek_kid_counts_all` は SECURITY DEFINER で
    GUC 不要のため**意図的に除外**し、その旨をコメントで明記（M6 の除外コメントと同型）。
- **(2) の正規表現を広げる**: 現行は `db::($fns)\(\s*state\.pool\(\)` のみ。`secrets::resolve_for_injection`
  は `secrets::` 名前空間なので素通りする。`(db|secrets)::($fns)\(\s*state\.pool\(\)` へ広げる。
- **(4) 新設 — `Redacted::expose()` の allowlist**: allowlist は
  `crates/control-plane/src/secrets.rs`、`crates/control-plane/src/handlers_secrets.rs`、
  **`crates/worker/src/env.rs`** の 3 ファイルのみ。
  当初案は worker 本体（`crates/worker/src/main.rs`、1700 行超）を丸ごと allowlist に入れており実質ガードが
  効かなかった。→ **worker の env 組み立てを `crates/worker/src/env.rs` へ切り出す**（§3.5 / §8）。
  既存 (1)(2) と同じ grep + コメント除去のヒューリスティックで実装する。
- **(5) 新設 — worker 側のテナントスコープ生 SQL**: M7b の worker 側 config 直読みは `db::` も
  `state.pool()` も使わない生の sqlx なので (2) の対象外。`crates/worker/` 内で `function_configs` /
  `function_secret` を含む SQL 文字列が現れる関数に `set_tenant_guc` が同居することを grep で確認する。
  実装が難しければ最低限、`scripts/rls-lint.sh` のヘッダ注意書き（現行の「保証ではない」）に
  「worker の新規テナントスコープ読み取りは (2) の対象外である」ことを追記する。

---

## 6. 不変条件の保全（明示セクション）

| 不変条件 | M7 での保全方法 |
| --- | --- |
| **テナント分離 (RLS)** | M7a は新テーブルを作らず `components` / `executions` の既存 FORCE RLS + fail-closed `tenant_isolation` をそのまま継承（追加 GRANT/POLICY 不要）。M7b/M7c の新 3 表は `ENABLE+FORCE RLS` + fail-closed ポリシー + 明示 GRANT/REVOKE。さらに複合 FK `(tenant_id, component_id) -> components (tenant_id, id)` で DB 側でもテナント一致を強制する（RLS の `WITH CHECK` は「自分の tenant_id を書くこと」しか要求しないため、それだけでは他テナントの component_id を紐づけられてしまう）。worker 側の DB 直読みも `set_tenant_guc` + `WHERE tenant_id=$1` の二重防御。SECURITY DEFINER 関数は **HTTP から呼ぶものは必ず `p_tenant` を取る**（`_all` 版は背景ジョブ専用）。 |
| **結果出所認証 (provenance, §3.3)** | 変更なし。canary で選んだ `version_id` が executions・job_token claim・`JobMessage.version` へ**同時供給**されるため（`EnqueueRequest` の単一供給点）、subscriber の claim 突合が素通りする。`env_token` は provenance とは別系統の capability であり、`aud="job-env"` とドメインタグで job_token と相互使用不可。 |
| **worker は鍵を持たない (§3.3)** | 案 D により維持。KEK は CP のみ。worker が新たに持つのは短命 `env_token`（CP 署名の execution スコープ capability）だけで、**新しい長期認証情報は 1 つも配らない**。 |
| **`JobMessage` に平文 secret / KEK / DEK を載せない** | M7 で新設する不変条件 (MUST NOT)。`env_token: Option<String>` 以外に secret 由来の情報を wire に出さない（secret 名も件数も出さない）。**`env_token` は result / DLQ / reply へ echo しない (MUST NOT)**。 |
| **計量 (M5)** | 影響なし。`usage_rollups` に触らない。`executions` に足すのは `routing_reason` の 1 列のみで、リソース指標の解釈は M5 のまま。 |
| **冪等性 (3 層, §6.6)** | ルーティングキーは決定的なので、layer1（Idempotency-Key）の冪等ヒットは版を再抽選しない。cron の同一 slot・event の同一 dedup は同一キー→同一版。routing key は `invoke_request_hash_for` に含めないので、routing key だけ違う再送は 409 ではなく冪等ヒット（§2.2）。 |
| **secret 世代の冪等** | `/internal/job-env` は副作用を持たない（POST だが冪等）。**同一 execution の再配送は同一世代を返す**（`created_at <= executions.created_at` の LATERAL 解決, §4.7.1）。secret の PUT は「新 version を追記して current を前進」なので再送で version が増えるだけ（値は同じ）。 |
| **capability deny-all (§4.4)** | baseline（`BASELINE_APPROVED_PREFIXES`）を緩めない。`validation.rs` の `faas:secrets/store` 非承認アサートを維持する。`capabilities.env` の既定は空配列＝deny-all で、書き込みは admin 専用エンドポイントのみ（deploy 経路は 400）。 |
| **Capability 承認は admin (§4.4)** | secret 書き込み・`capabilities.env` 承認は admin スコープ + `require_admin_role` の二重ガード。canary/rollback は admin スコープのみ（既存 `PUT /active-version` との対称性、§2.8.1）。 |
| **ステートレス×N** | canary の権威は DB の 1 行のみでキャッシュ層を持たない。promote / rollback / switch は**単一 UPDATE + CAS**なので複数 CP の read-then-write レースが構造的に起きない。secret の keyring はプロセス内だが env から決定的に構築されるため全インスタンスで同一。 |
| **段階移行の一貫性** | 「stable ポインタを動かす操作は必ず canary をクリアする」＋「no-op 切替では `previous` を破壊しない」（CASE ガード）＋「戻り先は未削除 version としてのみ解決する」の 3 点を SQL 文字列不変条件テストで固定する（§7.2）。 |

---

## 7. テスト方針 / 完了条件の自動検証

`.github/workflows/ci.yml` には postgres / nats / redis / minio の services が無く `#[ignore]` は実行されない。
**CI で守れるのは DB-free 単体（分配の数学 / migration 文字列不変条件 / redaction / 純関数）だけ**であり、
e2e は `chaos_m7.rs` に置くが CI では走らない —— この分担を明記する。

### 7.1 各ステップ共通のゲート（M6 設計書 §6 と同じ 4 点）

1. DB-free 単体が緑。
2. `chaos_m7.rs` は全て `#[ignore]`。
3. 各ステップ完了時に `cargo build/test --workspace --exclude echo` が緑、
   `cargo clippy --workspace --exclude echo --all-targets -- -D warnings` と
   `cargo fmt --all -- --check` が通る（`--all-targets` は `tests/` も対象なので `chaos_m7.rs` も
   clippy 完全準拠が必要。`chaos_m6.rs` 冒頭の `#![allow(clippy::needless_return)]` を写し忘れないこと）。
4. 各ステップで `make rls-lint` を通す（§5.6 の `fns=` 追記 + チェック (4)(5)）。

### 7.2 DB-free ユニットテスト（CI で毎回走る本丸）

**`crates/control-plane/src/routing.rs` の `#[cfg(test)] mod tests`**（`cron.rs` / `store.rs` の作法）:

| テスト | 内容 |
| --- | --- |
| `weight_zero_is_always_stable` | bucket 0..=99 を**全列挙**し、weight=0 で必ず Stable |
| `weight_100_is_always_canary` | bucket 0..=99 を全列挙し、weight=100 で必ず Canary |
| `weight_equals_canary_bucket_count` | `for w in 0..=100`: bucket 0..=99 のうち Canary になる個数が**ちょうど w** ＝ 分配比率の完全証明。10 / 50 / 100 の完了条件がここで閉じる |
| `selection_is_monotone_in_weight` | bucket 固定で w を 0→100 に上げると Stable→Canary の遷移が**高々 1 回**（rollback 時に逆流しない根拠） |
| `missing_canary_falls_back_to_stable` | `canary=None` なら weight=100 でも Stable（soft delete / ポインタ不整合の fail-safe） |
| `bucket_is_deterministic_and_in_range` | 同一入力で同値、値域 0..=99、同一 key でも component_id が違えば独立 |
| `bucket_matches_golden_vectors` | `(component_id, key)` の固定 3 組に対する期待バケットをハードコードした golden test。ハッシュ式（ドメインタグ・連結順・先頭 4 バイト BE・mod 100）のどれか 1 つでも変わると落ちる。**当初案の「`invoke_request_hash_for` と相関しない」テストは型も値域も違って形式化できないため採らない** |
| `domain_prefix_changes_the_bucket` | テスト内にプレフィクス無し版を複製し、同一入力で bucket が一致しないことを固定（ドメイン分離の回帰ガード） |
| `bucket_distribution_is_roughly_uniform` | 10,000 個の合成 key（`format!("k{i}")`）で各バケット件数が 100±40（決定的入力なのでフレークしない） |
| `routing_key_validation` | `X-Faas-Routing-Key` バリデータ（空 / 129 バイト / 非 ASCII / 制御文字を拒否、境界 128 を許可） |

**`crates/control-plane/src/main.rs` の `mod migration_tests`**（0007/0008 と同型、1 ファイル 1 const）:

| テスト | 内容 |
| --- | --- |
| `migrator_includes_version_9` / `_10` / `_11` | `MIGRATOR.iter()` が収集し description が `m7a` / `m7b` / `m7c` を含む |
| `version_9_is_not_baselined` / `_10` / `_11` | `BASELINE_VERSIONS` に無い |
| `m7a_creates_no_new_table` | `!M7A_SQL.contains("CREATE TABLE")` ＝ **0009 に新テーブルが無いこと**を静的に固定。これが「RLS/GRANT 漏れが構造的に起き得ない」ことの CI 上の証明（0009 に新テーブルを足す変更は必ずこれを落とす）。**`M7A_SQL` は 0009 のみを `include_str!` した const に紐づける**（統合ファイルにしないことの担保） |
| `m7a_grants_nothing_new` | `!M7A_SQL.contains("GRANT")` |
| `m7a_columns_are_additive` | `ADD COLUMN IF NOT EXISTS` が 5 個、`DROP COLUMN` / `ALTER COLUMN` を含まない |
| `m7a_canary_weight_is_range_checked` | `canary_weight <= 100` の CHECK と `canary_weight = 0 OR canary_version_id IS NOT NULL` の CHECK が存在 |
| `m7a_executions_check_is_not_valid` | `executions_routing_reason_chk` が `NOT VALID` を伴う（ローリング更新のロック窓回避の固定）かつ `ADD COLUMN ... routing_reason` の行にインライン `CHECK` が無い |
| `m7b_tables_are_force_rls` / `m7c_tables_are_force_rls` | 空白正規化して `ALTER TABLE {t} FORCE ROW LEVEL SECURITY`（既存 `m6_tables_are_force_rls` と同じ実装） |
| `m7b_tenant_isolation_is_fail_closed` / `m7c_*` | `current_setting('app.tenant_id')` を含み `, true)` / `, TRUE)` を含まない |
| `m7b_tables_have_explicit_grants` / `m7c_*` | `REVOKE ALL ... FROM PUBLIC` と `GRANT ... TO faas_app` が各表に現れる（0004 の GRANT 列挙に新表が含まれないことによる最頻の退行を CI で捕まえる） |
| `m7c_secret_versions_is_append_only_for_faas_app` | `function_secret_versions` に対する `GRANT UPDATE` / `GRANT DELETE` / `GRANT ALL` が現れない（既存 `audit_logs_is_append_only_for_faas_app` と同型） |
| `m7c_secret_name_uniqueness_is_soft_delete_aware` | `CREATE UNIQUE INDEX ... ON function_secrets (...) WHERE deleted_at IS NULL` を含み、テーブル制約の `UNIQUE (tenant_id, component_id, name)` を含まない（同名再作成が可能であることの固定） |
| `m7c_definer_functions_are_tenant_scoped` | `secrets_stale_kek(` の定義に `p_tenant text` が現れ、本体に `s.tenant_id = p_tenant` が現れる。全テナント版は `_all` 接尾辞のものだけ（テナント引数の消失を CI で捕まえる） |
| `m7bc_foreign_keys_are_composite` | 新 3 表の component/secret 参照 FK が `FOREIGN KEY (tenant_id, ...)` の 2 列形であること |

**`crates/control-plane/src/db.rs` の `mod tests`**（SQL 文字列不変条件の既存作法）:

| テスト | 内容 |
| --- | --- |
| `resolve_routing_sql_is_tenant_scoped` | `RESOLVE_ROUTING_SQL` が `c.tenant_id = $1` を含み、文字列結合を含まない |
| `resolve_routing_sql_fails_safe_on_bad_canary` | LEFT JOIN 側に `cvv.deleted_at IS NULL` / `cvv.tenant_id = c.tenant_id` / **`cvv.component_id = c.id`** を含む |
| `resolve_routing_sql_preserves_stable_predicates` | **凍結リテラルとの `contains` 照合**にする: `ON sv.id = c.active_version_id AND sv.tenant_id = c.tenant_id` / `sv.component_id = c.id` / `c.deleted_at IS NULL` / `c.active_version_id IS NOT NULL`。当初案の「現行 `active_version_storage` と文字単位で同一」は alias 改名（`cv`→`sv`）と関数削除により成立しないため採らない |
| `switch_active_version_clears_canary` | `canary_weight = 0` と `canary_version_id = NULL` を含む |
| `switch_active_version_preserves_previous_on_noop` | 3 SQL すべてが `CASE WHEN active_version_id IS DISTINCT FROM` を含む（no-op 再 activate で rollback 履歴を壊さない固定） |
| `promote_sql_is_single_statement_cas` | `PROMOTE_ACTIVE_VERSION_SQL` が `canary_version_id IS NOT NULL` と `($3 IS NULL OR canary_version_id = $3)` と `RETURNING` を含み、`SELECT` を含まない |
| `rollback_sql_requires_live_version` | `ROLLBACK_ACTIVE_VERSION_SQL` が `cv.deleted_at IS NULL` と `cv.component_id = c.id` を含む |
| `secret_injection_sql_is_execution_pinned` | 注入クエリが `v2.created_at <= ` を含み `current_version` を含まない |

**`crates/control-plane/src/secrets.rs`**（`crypto.rs` の 4 本立て作法を踏襲）:

- encrypt → decrypt ラウンドトリップ
- **AAD 改竄**: 同じ暗号文を別 `tenant_id` / 別 `component_id` / 別 `name` の AAD で復号すると失敗
  （cut-and-paste 遮断の証明）
- **未知 kid** → `SecretError::UnknownKid`、`reason()` が `"unknown_kid"`
- nonce が呼び出しごとに異なる（`OsRng`）
- `wrapped_dek` を別 version の AAD で unwrap すると失敗（version 巻き戻し遮断）
- `Debug for SecretKeyring` の出力に鍵素材が含まれず kid だけが出る
- wrap-only rekey のラウンドトリップ（新 kid で unwrap→wrap した DEK で ciphertext が復号できる）

**`Redacted<T>` と config**:
- `format!("{:?}", Redacted::new("hunter2".to_string()))` が `"hunter2"` を含まない
- `Redacted<String>` が `Clone + Debug` を満たし、`Drop` で zeroize される（`Zeroize` を実装した
  テスト用型で drop 後の観測）
- `Config` の `{:?}` に `SECRETS_MASTER_KEY` / `JOB_SIGNING_KEY` / `S3_SECRET_KEY` /
  `BOOTSTRAP_ADMIN_TOKEN` の値が含まれない
- `SecretError` 由来の `Internal` がボディに reason も値も出さない（既存
  `internal_5xx_body_is_redacted` と同型）

**その他の純関数**（`invoke_request_hash_for` の隣に置く作法。doc に「純関数（DB/ストア非依存）」と書く）:
- env キー名バリデータ（小文字 / `=` / NUL / 65 文字 / 先頭数字を拒否、境界 64 を許可）
- 値長・キー数・総バイトの上限判定（境界値）
- `capabilities` の後方互換パース（配列 → `imports` とみなし `env` 空 / オブジェクト形 /
  壊れた JSON → deny-all）
- 許可リストフィルタ（許可外キーが必ず落ちる）
- config / secret のキー衝突検出
- `upload_version` 経路で `capabilities.env` が非空になる入力が存在しないこと
  （`validation.rs` のテストとして固定。§3.1 の権限昇格の回帰ガード）

**worker 側純関数**（`crates/worker/src/env.rs`）:
- `(config, secrets, allowed)` を畳んで `Vec<(String,String)>` を組み立てる関数の単体テスト。
  許可外が落ちる / 上限で切り詰める / secret と config が衝突しない前提の assert /
  出力順が決定的（キー名ソート）

### 7.3 `crates/control-plane/tests/chaos_m7.rs`（全て `#[ignore]`）

雛形は `chaos_m6.rs` の冒頭（ヘッダ doc 4 点セット + 信頼境界節 + 実行方法 +
`base_url()` / `token()` / `echo_component()` ヘルパ複製 + `#![allow(clippy::needless_return)]`）。
シナリオ区切りは 3 行バナー。属性は
`#[tokio::test]` + `#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m7 --ignored chaos_t1_"]`。
関数名は `chaos_t{N}_{検証内容}`（M6 が `chaos_s1_`、M4 が `chaos_a_`、M5 が `chaos_e1_` と milestone ごとに
接頭辞を変えている慣習に倣い、M7 は `chaos_t`）。

**信頼境界の明示節（`chaos_m5.rs` の作法）**: 「黒箱で検証できるのは API 応答・invoke 出力・エラーコードまで。
**他テナントへ漏れないことの構造的保証は RLS（0010/0011）+ `migration_tests` + `rls-lint` に委譲する**。
黒箱で検証できないのは `audit_logs` の内容（参照 API が無い）のみ。
`routing_reason` は `GET /components/{id}/traffic` の `version_stats[].canary_routed` として
**露出しているので検証できる**（当初案は信頼境界に入れていたが、これは自分で捨てていた一次情報だった）。」

#### S1 — canary 段階移行と ワンクリック rollback（`chaos_t1_canary_stepwise_shift_and_one_click_rollback`）

```
準備: echo component に v1(0.1.0) が active。v2(0.2.0) を activate=false で upload。
     ヘルパ invoke_and_version(key) -> version_id
       = POST /invoke（X-Faas-Routing-Key: key）→ 終端まで 1 秒間隔 30 秒ポーリング
         （chaos_m5.rs の作法）→ GET /executions/{id} の version_id

(1) weight=0（端点・決定的）
    PUT /traffic {canary_version:"0.2.0", weight:0}
    40 個の異なる key で invoke → **全て v1**（統計ではなく端点なのでフレークしない）

(2) weight=100（端点・決定的）
    PUT /traffic {..., weight:100}
    同じ 40 key で invoke → **全て v2**
    ここまでで「10%→100% の両端が効く」ことが決定的に示せる。

(3) 単調性（統計に頼らない中間の検証）
    key 集合 K（**200 個**）を固定し、weight を 10 / 50 / 100 と上げながら
    canary(v2) へ落ちた key 集合 S10 / S50 / S100 を採る。
    assert: S10 ⊆ S50 ⊆ S100 かつ S100 == K
    assert: |S10| >= 1 かつ |S50| > |S10|
    → bucket < weight という単一不等式の帰結。比率そのものは assert しない。
    **key 数を 40 ではなく 200 にする理由（レビュー指摘の反映）**: バケットは
    routing_bucket(component_id, key) で component_id を含むため、固定 component の固定キー集合は
    その stack で決定的に固まる。40 キーだと P(全キーが bucket >= 10) = 0.9^40 ≈ 1.5% で、
    当たりの悪い component_id を持つ環境では**毎回落ちる**（しかも「重みが効いていない」という
    誤った症状に見える）。200 キーなら 0.9^200 ≈ 7e-10、|S50| は Binomial(200, 0.5) なので
    |S50| > |S10| もまず落ちない。ハッシュ式をテスト側に複製しない（乖離の温床にしない）。

(4) canary_routed の一次情報（GET /traffic の露出を使う）
    weight を 10 → 50 と上げる前後で GET /components/{id}/traffic を採り、
    v2 の version_stats[].canary_routed が **単調増加**することを assert。
    （version_id 集合の単調性と併せて二重確認になる。）

(5) sticky（決定性の黒箱証明）
    weight=50 のまま同じ key で 3 回 invoke → 3 回とも同じ version_id。

(6) 削除保護と fail-safe
    weight=50 のまま DELETE /components/{id}/versions/0.2.0 → **409**（canary 保護）
    → DELETE /traffic → 再度 version 削除 → 204 → invoke は全て v1
    さらに: PUT /active-version {0.2.0} を経て previous=v1 の状態で
    DELETE /components/{id}/versions/0.1.0 → **409**（previous 保護）

(7) rollback の実効範囲（§2.10 の境界を実測で固定する）
    weight=100 にして invoke を 1 件 publish（終端待ちはしない）→ 即 POST /rollback {}
    → その execution が終端したときの version_id が **v2 のまま**であることを assert。
    「rollback は新規 enqueue のみを変える」という保証の境界を黒箱で固定する。

(8) 宣言的 no-op 切替に対する rollback（レビュー指摘の回帰ガード）
    PUT /active-version {0.1.0} → PUT /active-version {0.2.0} → PUT /active-version {0.2.0}（同一版を 2 回）
    → POST /rollback {} → active が **v1 に戻る**ことを assert
    （CASE ガードが無いと previous=v2 になり、200 を返すのに何も戻らない。）

(9) ワンクリック rollback
    PUT /traffic {canary_version:"0.2.0", weight:100} → POST /promote {"version":"0.2.0"}（active=v2）
    → **POST /rollback（body {} の 1 リクエストのみ）**
    assert: 応答 active_version_id == v1 の id
    assert: GET /traffic の weight == 0 かつ canary == null
    assert: 直後に 10 回 invoke → 全て v1
    「HTTP 呼び出し 1 回で戻った」ことをテスト側の呼び出し回数で担保する。

(10) 後始末（**assert より前に実行する**, chaos_m6.rs の作法）
    let _ = DELETE /traffic;  let _ = PUT /active-version {version:"0.1.0"};
```

#### S2 — per-function 環境変数 / secret の注入 e2e とテナント分離（`chaos_t2_env_and_secret_injection`）

```
 1. PUT /components/{id}/versions/0.1.0/capabilities {"env":["LOG_LEVEL","API_KEY"]}（admin）
 2. PUT /components/{id}/config {"env":{"LOG_LEVEL":"debug"}}
 3. PUT /components/{id}/secrets/API_KEY {"value":"chaos-secret-<uuid>"}（sentinel）
 4. POST /invoke?wait=1 → 出力の env に LOG_LEVEL=debug と API_KEY=<sentinel> が現れる
 5. capabilities.env に無いキー（例 OTHER）を config へ入れると **400**
 6. multipart の capabilities に env を混ぜて POST /versions すると **400**（権限昇格の回帰ガード）
 7. 2 つ目のテナントを POST /admin/tenants で作り同名 component を deploy →
    invoke 出力に 1 テナント目の値が **現れない**（他テナント非漏洩の黒箱確認）
 8. 後始末（config / secret / component の削除）は **assert より前**に let _ = client.delete(...)
```

#### S3 — secret 非漏洩の黒箱検証（決定的 assert のみ・統計に依存しない）（`chaos_t3_secret_non_disclosure`）

```
 1. GET /components/{id}/secrets のレスポンス**全文**に sentinel が含まれない（メタデータのみ、
    かつ value_len / kek_kid が含まれないことも assert）
 2. GET /executions/{id} のレスポンス全文に sentinel が含まれない
 3. 値長超過（4097 バイト）で **400**、他テナントの component_id で **404**
 4. secret を rotate → 新 version 番号が返り、invoke 出力の値が新値になり、**旧値は二度と観測できない**
 5. **in-flight rotation の世代固定**（§4.7.1 の回帰ガード）:
    slow component（既存 components/slow）へ invoke を 1 件投げ、実行中に rotate → その execution の
    出力が **旧値** のままであることを assert（execution 基準の世代解決が効いている証明）
 6. POST /internal/job-env を **公開 listener（BIND_ADDR）へ** 投げると 404（そこには生えていない）
 7. 内部 listener（CHAOS_INTERNAL_URL、既定 http://127.0.0.1:8081）へ:
    - env_token 無し → 401
    - **job_token を env_token として提示 → 401**（aud / ドメインタグ分離の回帰ガード）
    - 終端済み execution の env_token → 403
 8. 後始末（assert より前）
```

**手順として README に書く（テスト内から叩かない）**: `docker compose logs control-plane worker |
grep -c "<sentinel>"` が 0、および `SELECT detail FROM audit_logs WHERE action LIKE 'secret%'` に sentinel が
無いこと。テスト内から `docker compose` / `psql` を呼ぶのは環境依存が強いため、README の chaos 節と同型の
「手で叩く最小手順」ブロックとして記述する。

**フレーク回避**: `chaos_m6.rs` の S2/S3 が「直接の二重発火検出は会計に委ね間接確認に留める」と明示的に
逃げた前例に倣い、M7 の S2/S3 も**分布や時間依存の assert を持たない**。

---

## 8. 影響ファイル一覧（ステップ別）

> 挿入位置は **行番号ではなくアンカー**で書く（行番号は既に drift しており、当初案の
> 「`handlers.rs:2660` 付近 = `mod tests` の直前」は実際には約 170 行ずれていた）。

### M7a-1（挙動不変）
- `migrations/0009_m7a_traffic_split.sql`（新規）
- `crates/control-plane/src/routing.rs`（新規: 純関数 + ユニットテスト）
- `crates/control-plane/src/main.rs`: `mod routing;`（`mod reaper;` と `mod scheduler;` の間）、
  `mod migration_tests` に `M7A_SQL` const + 0009 用テスト群
- `crates/control-plane/src/db.rs`:
  - `active_version_storage` → `resolve_component_routing`（`ActiveVersion` は流用）
  - `insert_pending_execution_with_provenance` に `routing_reason` 引数（12 引数目）
  - **`insert_pending_execution`（`#[allow(dead_code)]` の簡易版）の呼び出し側修正**（11→12 引数）
  - `mod tests` に SQL 文字列不変条件テスト
- `crates/control-plane/src/enqueue.rs`: `RoutedVersion` / `resolve_version_for_enqueue` 新設、
  `EnqueueRequest.routing_reason` 追加、`enqueue_execution` の INSERT 引数追加
- `crates/control-plane/src/handlers.rs`: `invoke()` の解決点差し替え + routing key 決定 +
  `X-Faas-Routing-Key` バリデータ（純関数、`invoke_request_hash_for` の隣）、
  `enqueue_via_trigger()` の解決点差し替え
- `crates/control-plane/src/scheduler.rs`: `fire_due_job()` の解決点差し替え + `slot`/`idem_key` 算出の前倒し
- `scripts/rls-lint.sh`: `fns=` に M7a の 7 本を追記、`active_version_storage` を削除

### M7a-2（API）
- `crates/control-plane/src/main.rs`: `build_router` の `read_routes` / `admin_routes` に 5 ルート追加 +
  doc コメントの列挙更新
- `crates/control-plane/src/handlers.rs`:
  - `object_storage_event` の直後・`#[cfg(test)] mod tests` の直前に `// ---` 3 行のセクション見出し付きで
    新ハンドラ 5 本（`set_traffic_split` / `clear_traffic_split` / `get_traffic_split` /
    `promote_version` / `rollback_version`）
  - `upload_version`: multipart `activate` アーム（`match field.name()` に追加）+
    `capabilities` に `env` が来たら 400（M7b-1 で有効化。M7a-2 では `activate` のみ）
  - `delete_version`: 3 ポインタ保護
  - `set_active_version`: `switch_active_version` へ差し替え + 監査追記
- `crates/control-plane/src/db.rs`: `switch_active_version` / `promote_active_version` /
  `rollback_active_version` / `set_traffic_split` / `clear_traffic_split`、
  `ComponentRow` に `canary_version_id` / `previous_active_version_id` を追加し
  `find_component_by_id` / `find_component_by_name` の SELECT 列を拡張
  （**`list_components` / `GET /components` の応答形は変えない**）
- `scripts/rls-lint.sh`（追記漏れの再確認）

### M7a-3（観測 + e2e）
- `crates/control-plane/src/metrics.rs`: `canary_routed_total` 追加（既存メトリクスは不変）
- `crates/control-plane/src/enqueue.rs`: `EnqueueOutcome::Enqueued` を返す直前で inc
- `crates/control-plane/src/db.rs`: `version_stats_for_component`
- `crates/control-plane/tests/chaos_m7.rs`（新規、S1）

### M7-0（露出ガード基盤）
- `crates/shared/src/lib.rs`: `Redacted<T>` + `expose_once` + env 上限定数
- `crates/control-plane/src/config.rs`: 秘密フィールドを `Redacted` 化 + `Debug` 手動実装
- `crates/control-plane/src/handlers.rs`（`CreateTokenResponse.secret`）/
  `crates/control-plane/src/login.rs`（`LoginResponse` の token）
- `Cargo.toml`（workspace）: `zeroize` を workspace 依存へ昇格
- `scripts/rls-lint.sh`: チェック (4) 新設

### M7b-1（capability env 承認）
- `migrations/0010_m7b_function_configs.sql`（新規）
- `crates/control-plane/src/validation.rs`: `capabilities` の 2 キー構造 + 後方互換パース +
  「upload 経路で env が非空にならない」テスト
- `crates/control-plane/src/handlers.rs`: `upload_version` で `env` 混入を 400、保存値を
  `{"imports":..., "env":[]}` へ、`PUT /components/{id}/versions/{version}/capabilities` ハンドラ
- `crates/control-plane/src/main.rs`: admin ルート追加 + `mod migration_tests` に `M7B_SQL`
- `crates/control-plane/src/db.rs`: `approve_capability_env`
- `scripts/rls-lint.sh`

### M7b-2（config CRUD + worker 注入）
- `crates/control-plane/src/db.rs`: `upsert_function_config` / `list_function_configs` /
  `delete_function_config`
- `crates/control-plane/src/handlers.rs`: config ハンドラ 3 本、`crates/control-plane/src/main.rs`: deploy ルート
- **`crates/worker/src/env.rs`（新規）**: 許可リスト畳み込み + 上限 clamp（rls-lint (4) の allowlist 対象）
- `crates/worker/src/main.rs`: `mod env;`、`resolve_limits` の拡張（`capabilities` + `function_configs`）、
  `execute` / `run_component` のシグネチャ、`WasiCtxBuilder` への `.envs(..)` と stderr 分岐
- `components/echo/src/lib.rs`: 出力に `env` を追加

### M7c-1（暗号）
- `crates/control-plane/src/secrets.rs`（新規）、`crates/control-plane/src/main.rs`: `mod secrets;`
- `crates/control-plane/src/signing.rs`: `decode_seed` → `decode_key32(raw, env_name)` へ一般化
- `crates/control-plane/src/config.rs`: M7 ブロック 3 点セット + プレースホルダ一致で起動失敗
- `Cargo.toml` / `crates/control-plane/Cargo.toml`: `chacha20poly1305` 追加（用途コメント付き）
- `.env.example`（`JOB_SIGNING_KEY` ブロック直下に M7 ブロック。行内コメント禁止注記を複写）

### M7c-2（DDL + write-only CRUD）
- `migrations/0011_m7c_secrets.sql`（新規）
- `crates/control-plane/src/handlers_secrets.rs`（新規。secret 系ハンドラを隔離）
- `crates/control-plane/src/db.rs`: secret 系 db 関数 + SQL 文字列不変条件テスト
- `crates/control-plane/src/main.rs`: admin ルート + `mod handlers_secrets;` + `M7C_SQL`
- `scripts/rls-lint.sh`: `fns=` 追記 + (2) の正規表現を `(db|secrets)::` へ

### M7c-3（注入経路）
- `crates/shared/src/lib.rs`: `JobMessage.env_token`、`EnvClaims`、
  `env_claims_signing_bytes`（ドメインタグ `faas-env-token-v1`）
- `crates/control-plane/src/main.rs`: `build_internal_router` + 2 本目の axum サーバ spawn
  （`INTERNAL_BIND_ADDR`）
- `crates/control-plane/src/enqueue.rs`: `env_token` の mint（生存 secret が 1 件以上のときのみ）
- `crates/control-plane/src/handlers_secrets.rs`: `job_env` ハンドラ（§4.6.2 の 11 手順）
- `crates/control-plane/src/store.rs`: `rate_limit_ns` を trait + 3 実装へ
- `crates/worker/src/main.rs`: `CONTROL_PLANE_INTERNAL_URL` / `JOB_ENV_FETCH_TIMEOUT_MS`、
  `execute` から env 引き換え、fail-closed、**`env_token` を result/DLQ へ echo しない**

### M7c-4（rekey）
- `crates/control-plane/src/secrets.rs`: 再ラップ
- `crates/control-plane/src/handlers_secrets.rs`: `POST /admin/secrets/rekey`（テナント単位）
- `crates/control-plane/src/metrics.rs`: `faas_secret_versions_by_kid`（kid ゲージ）+
  `faas_secret_material_issued_total` + `faas_guest_stderr_dropped_bytes_total`
- `crates/control-plane/src/main.rs`: 背景ジョブ spawn（`scheduler.rs` / `reaper.rs` と同型）

### M7-Z（ドキュメント / 運用）
- `crates/control-plane/tests/chaos_m7.rs`: S2 / S3
- `README.md`: タイトルを M7 へ / `## M7 スコープ（と非スコープ）` 節を新設
  （M6 節の「含まない: per-function 環境変数・Secrets Manager（M7, §10）」を書き換え）/
  env 表に M7 の env / エンドポイント表に M7 の 12 行（`PUT /components/{id}/active-version` の直後）/
  chaos 節に S1-S3 の実行手順と sentinel grep 手順 / ディレクトリ構成に
  `migrations/0009,0010,0011`・`crates/control-plane/src/routing.rs`・`secrets.rs`・
  `handlers_secrets.rs`・`crates/worker/src/env.rs`・`crates/control-plane/tests/chaos_m7.rs` /
  トラブルシュートに「0009 のロック窓と `--migrate-only` 手順」「`SECRETS_MASTER_KEY` プレースホルダで
  起動失敗したとき」「`VALIDATE CONSTRAINT` の実行タイミング」/ 次のマイルストーン
- `Makefile`: 冒頭の流れコメント、`# --- M7: canary 段階移行 / secrets（§6.7 / §10 / §15） ---` の env ブロック、
  `canary` / `promote` / `rollback` / `set-secret` ターゲット（既存 deploy 系の作法: `@set -e` 一括シェル →
  `TOKEN=$$($(MAKE) -s login)` → `curl -sS -o file -w '%{http_code}'` → `## 日本語 1 行説明` → `.PHONY` 追記。
  **ただし secret 系は body を echo しない**）、2 版が要るので `make deploy VERSION=0.2.0` の導線を使う
- `.env.example`: M7 の env 一式
- `crates/control-plane/src/config.rs`: 冒頭の `TODO(§8): M3 で設定ソースを secrets manager 等へ移す。` を
  M7 の実態（「per-function の secret は M7 の Secrets Manager が持つ。CP 自身のブートストラップ設定
  （KEK・署名鍵）は依然 env である」）に書き換える

---

## 9. 完了条件と、その検証手段の対応

| 完了条件（仕様書 §15 M7） | 実現する設計 | 検証手段 |
| --- | --- | --- |
| **active-version を 10%→100% へ段階移行できる** | `components` の 2 ポインタ + `canary_weight`（§1.2）、決定的バケット選択（§2.1-2.3）、`PUT /traffic` の絶対値 API（§2.8.5）、`activate=false`（§2.7） | CI: `weight_equals_canary_bucket_count`（0..=100 の全 weight × 全 bucket 列挙で分配比率を**証明**）+ `selection_is_monotone_in_weight`。chaos S1 (1)(2)(3)(4)（端点の決定的 assert + 単調性 + `canary_routed` の単調増加） |
| **ワンクリック rollback が効く** | `previous_active_version_id`（§1.2）、`ROLLBACK_ACTIVE_VERSION_SQL` の単一 UPDATE（§2.6）、`POST /rollback` body 空（§2.8.5）、no-op 切替での履歴保護（CASE ガード）、tombstone 復活の禁止（§1.4 / §2.6） | CI: `switch_active_version_preserves_previous_on_noop` / `rollback_sql_requires_live_version`。chaos S1 (6)(8)(9)（削除保護 409 / 宣言的 no-op 後の rollback / **HTTP 1 リクエスト**で v1 に戻ることをテスト側の呼び出し回数で担保）。境界は S1 (7) で明示的に実測（§2.10） |
| **secret がログへ漏れない** | `Redacted<T>`（Serialize 非実装・Debug は `<redacted>`, §5.1）、`#[tracing::instrument(skip_all)]`、値は必ず body（§5.5）、**secret 注入実行では `inherit_stderr()` を使わない**（§3.5）、`SecretError` は anyhow を経由せず空 `Internal` に潰す（§5.4）、Makefile は body を echo しない | CI: `Redacted` の Debug/Drop テスト、`Config` の `{:?}` テスト、5xx redaction テスト。chaos S3 (1)(2) + README 手順の `docker compose logs \| grep -c "<sentinel>"` == 0 |
| **secret が監査へ漏れない** | `secrets::audit_detail(name, version, reason)` を唯一の detail 構築点にし**値を受け取らないシグネチャ**にする（§5.3）、action 語彙の固定 | 型による保証（生値が `insert_audit_log` に到達する型経路が無い）+ README 手順の `SELECT detail FROM audit_logs WHERE action LIKE 'secret%'` |
| **secret が他テナントへ漏れない** | 新 3 表の `ENABLE+FORCE RLS` + fail-closed `tenant_isolation` + 明示 GRANT/REVOKE（§1.7 / §1.8）、複合 FK によるテナント一致の DB 強制、`WHERE tenant_id=$1` の二重防御、**HTTP から呼ぶ SECURITY DEFINER 関数は必ず `p_tenant` を取る**（§1.8）、rekey 応答から `remaining_by_kid` を削除（§4.5）、AAD に `tenant_id` を含める（§4.3） | CI: `m7b/m7c_tables_are_force_rls` / `_tenant_isolation_is_fail_closed` / `_tables_have_explicit_grants` / `m7c_definer_functions_are_tenant_scoped` / `m7bc_foreign_keys_are_composite`、`secrets.rs` の AAD 改竄テスト。chaos S2 (7)（2 テナント目に 1 テナント目の値が現れない） |
| **secret の引き換え口が漏洩面にならない** | 内部専用 listener（§4.6.1 (1)）、`JobMessage` にのみ載り result/DLQ へ echo されない専用 env-token（同 (2)）、ハンドラ内の `tenants.status` 確認（同 (3)）、per-IP + per-tenant 名前空間のレート制限（§5.5）、execution 基準の世代固定（§4.7.1） | chaos S3 (5)(6)(7)（公開 listener に無いこと / job_token 流用が 401 / 終端済みが 403 / in-flight rotate で値が変わらない） |

> **CI と chaos の分担（再掲・重要）**: `.github/workflows/ci.yml` には postgres / nats / redis / minio の
> services が無く `#[ignore]` は実行されない。上表の「CI:」は毎コミットで守られるが、「chaos:」は
> **手元 / 手動**でのみ走る。段階移行の**数学的正しさ**（10%→100% の分配）は CI 側の全列挙テストに、
> **配線の正しさ**（API・DB・NATS・worker が実際につながっていること）は chaos 側に、それぞれ責任を
> 分けている。どちらか一方だけでは完了条件を満たしたとは言えない。
